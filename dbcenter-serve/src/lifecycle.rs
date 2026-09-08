//! Boot, serve, flush, stop.

use crate::config::{ServeConfig, StorageChoice};
use dbcenter_core::storage::{LocalStorageAdapter, StorageAdapter};
use dbcenter_core::sync_engine::{SyncEngine, SyncMode};
use dbcenter_core::vfs::s3::S3StorageAdapter;
use dbcenter_core::DBCenter;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

type Storage = Arc<Box<dyn StorageAdapter + Send + Sync>>;

/// Download and unpack the PostgreSQL distribution, then exit.
///
/// Run during the container build so the image ships with the binaries in
/// place. `DBCENTER_POSTGRES_VERSION` selects the distribution; it must match
/// what the running container asks for, or the container will fetch its own.
pub fn prefetch_binaries() -> Result<std::path::PathBuf, String> {
    let mut config = dbcenter_core::DBCenterConfig::default();
    if let Ok(version) = std::env::var("DBCENTER_POSTGRES_VERSION") {
        if !version.is_empty() {
            config.postgres_version = version;
        }
    }
    let (initdb, _postgres) = dbcenter_core::binary::ensure_binaries(&config)
        .map_err(|e| format!("Could not fetch the PostgreSQL binaries: {}", e))?;
    Ok(initdb.parent().map(|p| p.to_path_buf()).unwrap_or(initdb))
}

pub fn run(config: ServeConfig) -> Result<(), String> {
    let storage = open_storage(&config)?;

    // Delta mode is what SyncEngine works against: writes land in <table>_adds,
    // and that is what gets pushed to storage as Parquet.
    let mut builder = DBCenter::builder()
        .path(&config.data_dir)
        .persist(true)
        .host(&config.listen_host)
        .port(config.port)
        .multi_user_sync_mode(true)
        .columnar(config.columnar);

    // The password has to be part of the initial configuration, not applied
    // afterwards: it is what makes the engine write a scram rule into
    // pg_hba.conf for network clients in the first place.
    if let Some(password) = &config.password {
        builder = builder.password(password);
    } else {
        tracing::warn!(
            "DBCENTER_PASSWORD is not set: TCP clients will be refused. Only the \
             Unix socket inside this container is usable."
        );
    }

    let db = builder
        .build()
        .map_err(|e| format!("Could not start the database: {}", e))?;

    // An existing cluster was initialised before, possibly without a password
    // or with a different one, so make sure the role matches what was asked for.
    if let Some(password) = &config.password {
        db.set_user_password("postgres", password)
            .map_err(|e| format!("Could not set the postgres password: {}", e))?;
    }

    // Declared collections have to exist before the first client connects, and
    // before a restore, which needs the delta tables to apply rows into.
    for name in &config.collections {
        let _ = db.collection(name);
        tracing::info!("Collection '{}' ready", name);
    }

    // Declared fields become real columns. Without them a wire client's writes
    // all land in the `data` JSONB column, and the Parquet that gets published
    // carries one JSON string per row instead of one column per field.
    for declared in &config.schema {
        db.collection(&declared.collection)
            .ensure_fields(&declared.fields)
            .map_err(|e| {
                format!(
                    "Could not declare the schema of '{}': {}",
                    declared.collection, e
                )
            })?;
        tracing::info!(
            "Collection '{}' has {} declared field(s)",
            declared.collection,
            declared.fields.len()
        );
    }

    let engine = storage
        .as_ref()
        .map(|s| SyncEngine::new(Arc::clone(&db), Arc::clone(s)));

    if let (Some(engine), Some(storage)) = (&engine, &storage) {
        restore(engine, storage, &db, config.lazy_restore)?;
    }

    // The view has to exist even for a table this machine has no local rows
    // for: with a lazy restore that is every table, and the view is then the
    // only way to read the history at all.
    if let Some(engine) = &engine {
        if config.columnar {
            for name in collection_names(&db)? {
                match engine.ensure_columnar_view(&name) {
                    Ok(()) => tracing::info!("Columnar view ready for '{}'", name),
                    Err(e) => tracing::warn!("No columnar view for '{}': {}", name, e),
                }
            }
        }
    }

    tracing::info!(
        "DBCenter is serving on {}:{} as node {}",
        config.listen_host,
        config.port,
        config.node_id
    );

    serve_until_signalled(&config, engine.as_ref(), &db)?;

    // The platform gave us a moment before it kills the machine; spend it
    // making sure the bucket has everything the local disk does.
    if let Some(engine) = &engine {
        tracing::info!("Shutting down: flushing pending changes");
        match push_all(engine, &db, config.sync_batch) {
            Ok(rows) => tracing::info!("Flushed {} rows", rows),
            Err(e) => tracing::error!("Final flush failed, data may be only local: {}", e),
        }
    }

    db.shutdown()
        .map_err(|e| format!("Could not shut down cleanly: {}", e))?;
    tracing::info!("Stopped");
    Ok(())
}

fn open_storage(config: &ServeConfig) -> Result<Option<Storage>, String> {
    let adapter: Box<dyn StorageAdapter + Send + Sync> = match &config.storage {
        None => {
            tracing::warn!(
                "DBCENTER_STORAGE is not set: running with local state only, \
                 which is lost when this container goes away"
            );
            return Ok(None);
        }
        Some(StorageChoice::Local { path }) => {
            tracing::info!("Durable copy: local directory {}", path);
            Box::new(LocalStorageAdapter::new(path.into()))
        }
        Some(StorageChoice::S3 { config: s3, prefix }) => {
            tracing::info!(
                "Durable copy: s3 bucket '{}' at {} (prefix {:?})",
                s3.bucket,
                s3.endpoint,
                prefix
            );
            Box::new(
                S3StorageAdapter::with_prefix(s3.clone(), prefix)
                    .map_err(|e| format!("Could not open object storage: {}", e))?,
            )
        }
    };
    Ok(Some(Arc::new(adapter)))
}

/// Table names the bucket holds data for.
///
/// A compaction writes `<table>_BASE_<timestamp>_<uuid>.parquet` and an
/// incremental push writes `<table>_DELTA_<timestamp>_<uuid>.parquet`, so the
/// table name is whatever precedes the first of those markers. A table that has
/// only ever been pushed incrementally still has to be restored, so both count.
fn tables_in_storage(storage: &Storage) -> Result<Vec<String>, String> {
    let keys = storage
        .list("")
        .map_err(|e| format!("Could not list object storage: {}", e))?;

    let mut tables: Vec<String> = keys
        .iter()
        .filter(|key| key.ends_with(".parquet"))
        .filter_map(|key| {
            let base = key.split_once("_BASE_").map(|(table, _)| table);
            let delta = key.split_once("_DELTA_").map(|(table, _)| table);
            // If both markers somehow appear, the earlier one starts the name.
            match (base, delta) {
                (Some(b), Some(d)) => Some(if b.len() < d.len() { b } else { d }),
                (Some(b), None) => Some(b),
                (None, Some(d)) => Some(d),
                (None, None) => None,
            }
            .map(|t| t.to_string())
        })
        .collect();
    tables.sort();
    tables.dedup();
    Ok(tables)
}

fn restore(
    engine: &SyncEngine,
    storage: &Storage,
    db: &Arc<DBCenter>,
    lazy: bool,
) -> Result<(), String> {
    let tables = tables_in_storage(storage)?;
    if tables.is_empty() {
        tracing::info!("Nothing in object storage yet; starting empty");
        return Ok(());
    }

    tracing::info!("Restoring {} table(s) from object storage", tables.len());
    for table in tables {
        let started = Instant::now();

        // The bucket is the list of what exists, so a table there is restored
        // whether or not it was declared in DBCENTER_COLLECTIONS. Creating the
        // collection first puts the delta tables in place for the rows to land
        // in; it is a no-op when they already exist.
        let _ = db.collection(&table);

        // Lazy: take the deltas and leave the base snapshot in the bucket. The
        // machine is then current on recent changes and holds none of the
        // history, which is what keeps its volume small.
        let mode = if lazy {
            SyncMode::Partial
        } else {
            SyncMode::Full
        };
        engine
            .pull_and_apply(&table, mode)
            .map_err(|e| format!("Could not restore '{}': {}", table, e))?;
        tracing::info!(
            "Restored '{}' in {:?}{}",
            table,
            started.elapsed(),
            if lazy { " (deltas only)" } else { "" }
        );
    }
    Ok(())
}

/// Take in whatever the other nodes have published since the last pass.
///
/// Deltas only: a base snapshot is a full rewrite of a table, and re-reading
/// one every interval would dominate the traffic. A node that needs the base
/// gets it at boot, when it restores.
fn pull_all(engine: &SyncEngine, db: &Arc<DBCenter>) -> Result<(), String> {
    let names = collection_names(db)?;
    if names.is_empty() {
        return Ok(());
    }
    for name in &names {
        if let Err(e) = engine.pull_and_apply(name, SyncMode::Partial) {
            tracing::error!("Could not pull '{}': {}", name, e);
        }
    }
    tracing::info!("Pulled {} collection(s) from object storage", names.len());
    Ok(())
}

/// Fold each table's accumulated deltas into a fresh base snapshot.
///
/// Every push leaves another Parquet object in the bucket and a restore reads
/// all of them, so cold starts get slower without bound until something
/// compacts. The engine takes the storage lock for this, so with several nodes
/// running exactly one does the work and the rest find the lock held and move
/// on - which is what the lock is there for.
fn compact_all(engine: &SyncEngine, db: &Arc<DBCenter>) {
    let names = match collection_names(db) {
        Ok(names) => names,
        Err(e) => {
            tracing::error!("Could not list collections to compact: {}", e);
            return;
        }
    };

    for name in names {
        match engine.vacuum(&name) {
            Ok(()) => tracing::info!("Compacted '{}'", name),
            // Another node holding the lock is the normal case, not a fault.
            Err(e) => tracing::debug!("Skipped compacting '{}': {}", name, e),
        }
    }
}

/// Collection names, derived from their delta tables.
///
/// In delta mode a collection is a view over `<name>_base`, `<name>_adds` and
/// `<name>_dels`, and list_collections reads pg_tables, which does not include
/// views - so the view name never appears. The `_adds` table is both present in
/// that list and exactly where pending rows live.
fn collection_names(db: &Arc<DBCenter>) -> Result<Vec<String>, String> {
    // `list_collections` already names collections rather than the tables
    // behind them. It used to return the raw table list, so this looked for an
    // `_adds` suffix to work out what a collection was called - and when the
    // core stopped returning those, every sync pass quietly found nothing to
    // do.
    let mut names = db
        .list_collections()
        .map_err(|e| format!("Could not list tables: {}", e))?;
    names.sort();
    names.dedup();
    Ok(names)
}

/// Push pending changes for every collection this database knows about.
fn push_all(engine: &SyncEngine, db: &Arc<DBCenter>, batch: usize) -> Result<usize, String> {
    let mut total = 0;
    for name in collection_names(db)? {
        match engine.sync_table(&name, batch) {
            Ok(rows) => {
                if rows > 0 {
                    tracing::info!("Pushed {} row(s) from '{}'", rows, name);
                }
                total += rows;
            }
            // One unsyncable table should not stop the others, especially
            // during the shutdown flush.
            Err(e) => tracing::error!("Could not push '{}': {}", name, e),
        }
    }
    Ok(total)
}

/// Run until the platform asks us to stop.
///
/// SIGTERM is what Docker and Fly send before stopping a container; SIGINT is
/// what a terminal sends. Both mean the same thing here.
fn serve_until_signalled(
    config: &ServeConfig,
    engine: Option<&SyncEngine>,
    db: &Arc<DBCenter>,
) -> Result<(), String> {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .map_err(|e| format!("Could not install the signal handler: {}", e))?;
    }

    let mut last_sync = Instant::now();
    let mut last_compact = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        // Wake often enough that a stop signal is acted on promptly, rather
        // than after a whole sync interval.
        std::thread::sleep(Duration::from_millis(250));

        let Some(engine) = engine else { continue };

        if last_sync.elapsed() >= config.sync_interval {
            last_sync = Instant::now();

            // Push first: publishing what this node knows before taking in
            // what others wrote keeps a crash between the two from losing
            // local work.
            if let Err(e) = push_all(engine, db, config.sync_batch) {
                tracing::error!("Periodic push failed: {}", e);
            }

            if config.pull_enabled {
                if let Err(e) = pull_all(engine, db) {
                    tracing::error!("Periodic pull failed: {}", e);
                }
            }
        }

        if !config.compact_interval.is_zero() && last_compact.elapsed() >= config.compact_interval {
            last_compact = Instant::now();
            compact_all(engine, db);
        }
    }

    tracing::info!("Stop requested");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::collection_names;
    use dbcenter_core::DBCenter;

    /// A sync pass acts on whatever this returns, so if it returns nothing the
    /// node runs forever and pushes nothing. It did: it was looking for tables
    /// ending in `_adds`, and the core stopped listing those.
    #[test]
    fn a_collection_is_found_by_the_name_a_client_would_use() {
        let dir = std::env::temp_dir().join("dbcenter_serve_collection_names");
        let _ = std::fs::remove_dir_all(&dir);

        let db = DBCenter::builder()
            .path(dir.to_str().unwrap())
            .persist(false)
            .multi_user_sync_mode(true)
            .build()
            .expect("embedded postgres should start");
        let _ = db.collection("orders");

        let names = collection_names(&db).unwrap();
        assert!(names.contains(&"orders".to_string()), "{names:?}");
        for internal in ["orders_base", "orders_adds", "orders_dels"] {
            assert!(!names.contains(&internal.to_string()), "{names:?}");
        }

        db.shutdown().unwrap();
    }

    #[test]
    fn table_names_come_from_both_markers() {
        let keys = [
            "users_BASE_1700000000_abc.parquet",
            "users_DELTA_1700000100_def.parquet",
            "order_items_DELTA_1700000200_ghi.parquet",
            "notes.txt",
        ];
        let mut tables: Vec<String> = keys
            .iter()
            .filter(|key| key.ends_with(".parquet"))
            .filter_map(|key| {
                let base = key.split_once("_BASE_").map(|(t, _)| t);
                let delta = key.split_once("_DELTA_").map(|(t, _)| t);
                match (base, delta) {
                    (Some(b), Some(d)) => Some(if b.len() < d.len() { b } else { d }),
                    (Some(b), None) => Some(b),
                    (None, Some(d)) => Some(d),
                    (None, None) => None,
                }
                .map(|t| t.to_string())
            })
            .collect();
        tables.sort();
        tables.dedup();
        assert_eq!(tables, vec!["order_items".to_string(), "users".to_string()]);
    }
}
