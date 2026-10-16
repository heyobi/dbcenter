//! Configuration, read from the environment.
//!
//! Everything is an environment variable because that is what a container
//! platform hands a process. Defaults are chosen so that `docker run` with only
//! the S3 credentials set does something sensible.

use dbcenter_core::vfs::s3::S3Config;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::time::Duration;

pub struct ServeConfig {
    /// Where the embedded PostgreSQL keeps its data inside the container.
    pub data_dir: String,
    /// Address PostgreSQL binds for TCP.
    ///
    /// `*` is every interface in both address families. `::` alone binds IPv6
    /// only, which looks right for Fly's private IPv6 network but silently
    /// refuses Docker's IPv4 port forwarding.
    pub listen_host: String,
    pub port: u16,
    /// Password for the `postgres` role. Without one, only the local Unix
    /// socket is usable, which makes the TCP listener pointless.
    pub password: Option<String>,
    /// Object storage holding the durable copy. None runs purely local, which
    /// is useful for a smoke test.
    pub storage: Option<StorageChoice>,
    /// How often to push pending changes while running.
    pub sync_interval: Duration,
    /// Rows pushed per table per sync pass.
    pub sync_batch: usize,
    /// Whether to pull other nodes' changes on each pass.
    ///
    /// Only worth paying for when more than one node writes to the bucket. A
    /// single scale-to-zero machine restores once at boot and is then the only
    /// writer, so pulling again buys nothing.
    pub pull_enabled: bool,
    /// How often to compact a table's deltas into a fresh base snapshot.
    ///
    /// Every incremental push leaves another Parquet object behind, and a
    /// restore reads all of them, so without compaction cold starts get slower
    /// without bound. Zero disables it.
    pub compact_interval: Duration,
    /// Identifies this instance in storage locks.
    pub node_id: String,
    /// Install pg_duckdb and keep a `<name>_columnar` view of every collection.
    ///
    /// The view reads the Parquet in object storage directly, so an aggregate
    /// over history never touches the local tables. Costs about 120 MB of
    /// resident set, which is why it is not the default on a small machine.
    pub columnar: bool,

    /// Build collections as a table their `_base` and `_adds` inherit from.
    ///
    /// Without it a collection is a view over the two, and PostgreSQL sorts the
    /// whole thing to answer `ORDER BY ... LIMIT` - 256.9 ms against 0.16 ms
    /// over 200,000 rows. Existing collections keep the shape they were made
    /// with; `DBCENTER_MIGRATE_DELTA=1` converts them on startup.
    pub inherited_delta: bool,

    /// Convert view-shaped collections to the inherited shape when the server
    /// starts. Off by default: it takes a lock on each collection and drops the
    /// view, which is not something to do to a running deployment by surprise.
    pub migrate_delta: bool,
    /// Restore from the deltas only, leaving the base snapshot in the bucket.
    ///
    /// The machine comes up current on recent changes without downloading the
    /// history, which is the point of a small volume. Anything the local tables
    /// cannot answer goes to object storage instead - which needs `columnar`,
    /// or a client that calls `smart_query`, to be answerable at all.
    pub lazy_restore: bool,
    /// Fields each collection has, as JSON: a map of collection name to a map
    /// of field name to type.
    ///
    ///     DBCENTER_SCHEMA={"events":{"user_id":"int","amount":"float"}}
    ///
    /// Types are `int`, `float`, `text`, `bool` and `json`. Without this a
    /// collection has only the columns the delta layer gives it, and a client
    /// on the wire protocol writes every field into the `data` JSONB column -
    /// which works, but costs the columnar read path its whole advantage,
    /// because the Parquet then holds one JSON string instead of one column
    /// per field.
    pub schema: Vec<CollectionSchema>,
    /// Collections to create at boot, comma separated.
    ///
    /// A collection is not a plain table: in delta mode it is a view over
    /// `<name>_base`, `<name>_adds` and `<name>_dels` with triggers routing
    /// writes. A client connecting over the wire protocol cannot conjure that
    /// with CREATE TABLE, so a container declares its schema here.
    pub collections: Vec<String>,
}

/// One collection's declared fields, as sample values the core reads for type.
pub struct CollectionSchema {
    pub collection: String,
    pub fields: HashMap<String, JsonValue>,
}

pub enum StorageChoice {
    S3 { config: S3Config, prefix: String },
    Local { path: String },
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Turn the declared types into sample values, which is what the schema
/// evolution in the core reads to decide a column type.
fn parse_schema(raw: Option<&str>) -> Result<Vec<CollectionSchema>, String> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let parsed: HashMap<String, HashMap<String, String>> = serde_json::from_str(raw)
        .map_err(|e| format!("DBCENTER_SCHEMA is not valid JSON: {}", e))?;

    let mut out = Vec::new();
    for (collection, fields) in parsed {
        let mut samples = HashMap::new();
        for (field, ty) in fields {
            let sample = match ty.to_ascii_lowercase().as_str() {
                "int" | "integer" | "bigint" => JsonValue::from(0i64),
                "float" | "double" | "real" => JsonValue::from(0.0f64),
                "text" | "string" | "str" => JsonValue::from(""),
                "bool" | "boolean" => JsonValue::from(false),
                "json" | "jsonb" => JsonValue::Object(Default::default()),
                other => {
                    return Err(format!(
                        "DBCENTER_SCHEMA: unknown type {:?} for {}.{}; use int, float, text, bool or json",
                        other, collection, field
                    ))
                }
            };
            samples.insert(field, sample);
        }
        out.push(CollectionSchema {
            collection,
            fields: samples,
        });
    }
    // A stable order, so two boots create the columns the same way round.
    out.sort_by(|a, b| a.collection.cmp(&b.collection));
    Ok(out)
}

fn flag(name: &str) -> bool {
    matches!(var(name).as_deref(), Some("1") | Some("true") | Some("yes"))
}

fn parsed<T: std::str::FromStr>(name: &str, default: T) -> Result<T, String> {
    match var(name) {
        None => Ok(default),
        Some(raw) => raw
            .parse()
            .map_err(|_| format!("{} is not a valid value: {:?}", name, raw)),
    }
}

impl ServeConfig {
    pub fn from_env() -> Result<Self, String> {
        let storage = match var("DBCENTER_STORAGE").as_deref() {
            Some("s3") => Some(StorageChoice::S3 {
                config: S3Config::from_env(var("DBCENTER_BUCKET").as_deref())
                    .map_err(|e| e.to_string())?,
                prefix: var("DBCENTER_PREFIX").unwrap_or_default(),
            }),
            Some("local") => Some(StorageChoice::Local {
                path: var("DBCENTER_STORAGE_PATH")
                    .unwrap_or_else(|| "/var/lib/dbcenter/storage".to_string()),
            }),
            Some("none") | None => None,
            Some(other) => {
                return Err(format!(
                    "DBCENTER_STORAGE must be s3, local or none; got {:?}",
                    other
                ))
            }
        };

        Ok(Self {
            data_dir: var("DBCENTER_DATA_DIR").unwrap_or_else(|| "/data/db".to_string()),
            listen_host: var("DBCENTER_LISTEN").unwrap_or_else(|| "*".to_string()),
            port: parsed("DBCENTER_PORT", 5432u16)?,
            password: var("DBCENTER_PASSWORD"),
            storage,
            sync_interval: Duration::from_secs(parsed("DBCENTER_SYNC_INTERVAL_SECS", 30u64)?),
            sync_batch: parsed("DBCENTER_SYNC_BATCH", 10_000usize)?,
            pull_enabled: flag("DBCENTER_PULL"),
            compact_interval: Duration::from_secs(parsed(
                "DBCENTER_COMPACT_INTERVAL_SECS",
                900u64,
            )?),
            schema: parse_schema(var("DBCENTER_SCHEMA").as_deref())?,
            columnar: flag("DBCENTER_COLUMNAR"),
            inherited_delta: flag("DBCENTER_INHERITED_DELTA"),
            migrate_delta: flag("DBCENTER_MIGRATE_DELTA"),
            lazy_restore: flag("DBCENTER_LAZY_RESTORE"),
            // Fly sets FLY_MACHINE_ID; anywhere else a random id is fine, since
            // it only has to be unique among the writers sharing the bucket.
            collections: var("DBCENTER_COLLECTIONS")
                .map(|raw| {
                    raw.split(',')
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            node_id: var("DBCENTER_NODE_ID")
                .or_else(|| var("FLY_MACHINE_ID"))
                .unwrap_or_else(|| format!("node-{}", uuid::Uuid::new_v4())),
        })
    }
}
