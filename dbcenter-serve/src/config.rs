//! Configuration, read from the environment.
//!
//! Everything is an environment variable because that is what a container
//! platform hands a process. Defaults are chosen so that `docker run` with only
//! the S3 credentials set does something sensible.

use dbcenter_core::vfs::s3::S3Config;
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
    /// Collections to create at boot, comma separated.
    ///
    /// A collection is not a plain table: in delta mode it is a view over
    /// `<name>_base`, `<name>_adds` and `<name>_dels` with triggers routing
    /// writes. A client connecting over the wire protocol cannot conjure that
    /// with CREATE TABLE, so a container declares its schema here.
    pub collections: Vec<String>,
}

pub enum StorageChoice {
    S3 { config: S3Config, prefix: String },
    Local { path: String },
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
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
            pull_enabled: matches!(
                var("DBCENTER_PULL").as_deref(),
                Some("1") | Some("true") | Some("yes")
            ),
            compact_interval: Duration::from_secs(parsed(
                "DBCENTER_COMPACT_INTERVAL_SECS",
                900u64,
            )?),
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
