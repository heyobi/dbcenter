use std::path::PathBuf;

/// Default PostgreSQL distribution version downloaded for embedded usage
pub const DEFAULT_POSTGRES_VERSION: &str = "18.0.0";

#[derive(Clone, Debug)]
pub struct DBCenterConfig {
    /// Data directory for PostgreSQL
    pub data_dir: PathBuf,

    /// Whether to persist data after shutdown
    pub persist: bool,

    /// Keep service running after program ends (daemon mode)
    pub daemon_mode: bool,

    /// PostgreSQL host (127.0.0.1 for Windows, socket for Unix)
    pub host: Option<String>,

    /// PostgreSQL port (None = auto-assign free port)
    pub port: Option<u16>,

    /// Database name
    pub dbname: String,

    /// User name
    pub user: String,

    /// Password for authentication (None = trust)
    pub password: Option<String>,

    /// Binary paths
    pub initdb_path: Option<PathBuf>,
    pub postgres_path: Option<PathBuf>,

    /// Requested PostgreSQL distribution version (Maven artifact version)
    pub postgres_version: String,

    /// Enable Phase 2 Delta Layer (Multi-User Sync on Dumb Storage)
    pub multi_user_sync_mode: bool,

    /// Install pg_duckdb and keep a columnar view of every synced collection.
    ///
    /// Off by default, and deliberately: pg_duckdb takes the resident set from
    /// about 128 MB to 250 MB, and a database nobody runs an aggregate against
    /// should not pay that. When it is on, the extension is put in place before
    /// PostgreSQL starts, so there is no restart to absorb.
    pub columnar: bool,
}

impl Default for DBCenterConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("./dbcenter_data"),
            persist: true,
            daemon_mode: false,
            host: None,
            port: None,
            dbname: "postgres".to_string(),
            user: "postgres".to_string(),
            password: None,
            initdb_path: None,
            postgres_path: None,
            postgres_version: DEFAULT_POSTGRES_VERSION.to_string(),
            multi_user_sync_mode: false,
            columnar: false,
        }
    }
}
