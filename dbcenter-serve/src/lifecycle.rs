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
        .multi_user_sync_mode(true);

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

    let engine = storage
        .as_ref()
        .map(|s| SyncEngine::new(Arc::clone(&db), Arc::clone(s)));

    if let (Some(engine), Some(storage)) = (&engine, &storage) {
        restore(engine, storage, &db)?;
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

#[cfg(test)]
mod tests {
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

fn restore(engine: &SyncEngine, storage: &Storage, db: &Arc<DBCenter>) -> Result<(), String> {
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

        engine
            .pull_and_apply(&table, SyncMode::Full)
            .map_err(|e| format!("Could not restore '{}': {}", table, e))?;
        tracing::info!("Restored '{}' in {:?}", table, started.elapsed());
    }
    Ok(())
}

/// Push pending changes for every collection this database knows about.
fn push_all(engine: &SyncEngine, db: &Arc<DBCenter>, batch: usize) -> Result<usize, String> {
    let tables = db
        .list_collections()
        .map_err(|e| format!("Could not list tables: {}", e))?;

    // In delta mode a collection is a *view* over <name>_base, <name>_adds and
    // <name>_dels, and list_collections reads pg_tables, which does not include
    // views. Filtering the view name out of that list therefore left nothing to
    // sync at all. Derive the collection name from its _adds table instead,
    // which is also exactly where the pending rows live.
    let mut names: Vec<String> = tables
        .iter()
        .filter_map(|t| t.strip_suffix("_adds"))
        .map(str::to_string)
        .collect();
    names.sort();
    names.dedup();

    let mut total = 0;
    for name in names {
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
    while !stop.load(Ordering::Relaxed) {
        // Wake often enough that a stop signal is acted on promptly, rather
        // than after a whole sync interval.
        std::thread::sleep(Duration::from_millis(250));

        if let Some(engine) = engine {
            if last_sync.elapsed() >= config.sync_interval {
                last_sync = Instant::now();
                if let Err(e) = push_all(engine, db, config.sync_batch) {
                    tracing::error!("Periodic sync failed: {}", e);
                }
            }
        }
    }

    tracing::info!("Stop requested");
    Ok(())
}
