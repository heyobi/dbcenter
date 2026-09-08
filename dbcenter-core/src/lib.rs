pub mod binary;
pub mod collection;
pub mod config;
pub mod delta_manager;
pub mod error;
pub mod extensions;
pub mod geo_collection;
pub mod lazy_frame;
pub mod process;
pub mod query;
pub mod storage;
pub mod sync_engine;
pub mod utils;
pub mod vector_store;
pub mod vfs;

pub use config::DBCenterConfig;
pub use error::{DBCenterError, Result};

// Re-exported so the binding crates bind query parameters against exactly the
// postgres types this crate was built with, instead of a second copy of the
// crate pulled in at a slightly different version.
pub use bytes;
pub use postgres;

use once_cell::sync::Lazy;
use postgres::{Client, NoTls};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use uuid::Uuid;

// Global instance registry for singleton pattern
static INSTANCES: Lazy<Mutex<HashMap<PathBuf, Arc<DBCenter>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// A small pool of connections to the embedded server.
///
/// There used to be one connection behind a mutex, so every caller queued behind
/// every other one and the library ran at single-connection throughput no matter
/// how many cores were free. Measured on a point-select loop: 6,108 queries a
/// second on one thread and 5,485 on eight - no scaling at all, while pgbench
/// against the same server went from 8,881 to 24,924 by opening a connection per
/// client.
///
/// Connections are created up front and handed out; a caller with none free
/// waits. Nothing here depends on session state surviving between calls - the
/// one `SET` in the codebase and every transaction are held on a single guard -
/// so which connection a caller gets does not matter.
/// How many connections the pool holds, and opening that many.
fn open_pool_connections(conn_str: &str) -> Result<Vec<Client>> {
    let pool_size = std::env::var("DBCENTER_POOL_SIZE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        })
        .clamp(1, 32);

    let mut clients = Vec::with_capacity(pool_size);
    for _ in 0..pool_size {
        clients.push(
            Client::connect(conn_str, NoTls)
                .map_err(|e| DBCenterError::ConnectionError(e.to_string()))?,
        );
    }
    tracing::debug!("Opened {} connections", clients.len());
    Ok(clients)
}

pub(crate) struct ClientPool {
    idle: Mutex<Vec<Client>>,
    available: Condvar,
}

impl ClientPool {
    fn new(clients: Vec<Client>) -> Self {
        Self {
            idle: Mutex::new(clients),
            available: Condvar::new(),
        }
    }

    /// Throw away every pooled connection and put these in their place.
    ///
    /// Restarting the server leaves every connection in the pool pointing at a
    /// process that no longer exists, and a pooled connection is only found to
    /// be dead by the query that fails on it. Handing the pool a fresh set is
    /// the only way a restart stays invisible to callers.
    fn replace_all(&self, clients: Vec<Client>) {
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        idle.clear();
        idle.extend(clients);
        self.available.notify_all();
    }

    fn get(&self) -> PooledClient<'_> {
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        while idle.is_empty() {
            idle = self.available.wait(idle).unwrap_or_else(|e| e.into_inner());
        }
        let client = idle.pop().expect("waited until one was there");
        PooledClient {
            pool: self,
            client: Some(client),
        }
    }
}

/// A connection borrowed from the pool, returned when it goes out of scope.
pub(crate) struct PooledClient<'a> {
    pool: &'a ClientPool,
    client: Option<Client>,
}

impl std::ops::Deref for PooledClient<'_> {
    type Target = Client;
    fn deref(&self) -> &Client {
        self.client.as_ref().expect("held until dropped")
    }
}

impl std::ops::DerefMut for PooledClient<'_> {
    fn deref_mut(&mut self) -> &mut Client {
        self.client.as_mut().expect("held until dropped")
    }
}

impl Drop for PooledClient<'_> {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            // A connection that broke is dropped rather than handed to the next
            // caller, who would only meet the same error.
            if client.is_closed() {
                return;
            }
            self.pool
                .idle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(client);
            self.pool.available.notify_one();
        }
    }
}

/// Main DBCenter struct - Embedded PostgreSQL database
pub struct DBCenter {
    config: DBCenterConfig,
    pool: ClientPool,
    /// Columns each table is known to have.
    ///
    /// Every insert used to ask information_schema which columns existed, so it
    /// could add any the document introduced. That view joins several catalogs
    /// and costs about 3ms - seventeen times what the insert itself costs inside
    /// a transaction, paid on every call.
    ///
    /// A stale entry is safe in the direction that matters: the statement it
    /// guards is ADD COLUMN IF NOT EXISTS, so believing a column is missing when
    /// it is not costs one redundant ALTER. Believing a column exists when it has
    /// been dropped would not be safe, and nothing in this API drops columns.
    schema_cache: Mutex<HashMap<String, std::collections::HashSet<String>>>,
    process: Arc<Mutex<process::PostgresProcess>>,
    path: PathBuf,
    instance_id: String,
}

/// Builder for creating DBCenter instances with a fluent API
///
/// # Example
/// ```no_run
/// use dbcenter_core::DBCenter;
///
/// let db = DBCenter::builder()
///     .path("./my_database")
///     .persist(true)
///     .build()
///     .expect("Failed to create database");
/// ```
// use crate::query::QueryBuilder;
#[derive(Default)]
pub struct DBCenterBuilder {
    path: Option<PathBuf>,
    // socket_dir: Option<PathBuf>,
    persist: bool,
    daemon_mode: bool,
    host: Option<String>,
    port: Option<u16>,
    password: Option<String>,
    postgres_version: Option<String>,
    multi_user_sync_mode: bool,
}

impl DBCenterBuilder {
    /// Create a new builder with default values
    pub fn new() -> Self {
        Self {
            path: None,
            persist: true, // Default to persistent
            daemon_mode: false,
            host: None,
            port: None,
            password: None,
            postgres_version: None,
            multi_user_sync_mode: false,
        }
    }

    /// Set the database path
    pub fn path<P: AsRef<Path>>(mut self, path: P) -> Self {
        self.path = Some(path.as_ref().to_path_buf());
        self
    }

    /// Set whether to persist the database (default: true)
    pub fn persist(mut self, persist: bool) -> Self {
        self.persist = persist;
        self
    }

    /// Set daemon mode - keeps PostgreSQL running after program exits
    pub fn daemon_mode(mut self, daemon: bool) -> Self {
        self.daemon_mode = daemon;
        self
    }

    /// Set the host address
    pub fn host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    /// Set the port number
    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// Set the database password
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Set the PostgreSQL version
    pub fn postgres_version(mut self, version: impl Into<String>) -> Self {
        self.postgres_version = Some(version.into());
        self
    }

    /// Enable Phase 2 Delta Layer (Multi-User Sync)
    pub fn multi_user_sync_mode(mut self, enabled: bool) -> Self {
        self.multi_user_sync_mode = enabled;
        self
    }

    /// Build the DBCenter instance
    pub fn build(self) -> Result<Arc<DBCenter>> {
        DBCenter::new(
            self.path,
            self.persist,
            self.daemon_mode,
            self.host,
            self.port,
            self.password,
            self.postgres_version,
            self.multi_user_sync_mode,
        )
    }
}

impl DBCenter {
    /// Create a new DBCenterBuilder for fluent configuration
    ///
    /// # Example
    /// ```no_run
    /// use dbcenter_core::DBCenter;
    ///
    /// let db = DBCenter::builder()
    ///     .path("./data")
    ///     .build()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn builder() -> DBCenterBuilder {
        DBCenterBuilder::new()
    }

    /// Create a new DBCenter instance
    ///
    /// Prefer [`DBCenter::builder`] for anything but the simplest call; this
    /// positional form exists because the C, Python, Go, Java and Node bindings
    /// all cross the FFI boundary with a flat argument list.
    #[allow(clippy::too_many_arguments)]
    pub fn new<P: AsRef<Path>>(
        path: Option<P>,
        persist: bool,
        daemon_mode: bool,
        host: Option<String>,
        port: Option<u16>,
        password: Option<String>,
        postgres_version: Option<String>,
        multi_user_sync_mode: bool,
    ) -> Result<Arc<Self>> {
        // Validation: service mode requires persist
        if daemon_mode && !persist {
            return Err(DBCenterError::ConfigError(
                "Invalid configuration: service=True requires persist=True. \
                Service mode keeps the database running, so files must be persisted."
                    .to_string(),
            ));
        }

        let (path_buf, path_hint) = match path {
            Some(p) => {
                let requested = p.as_ref().to_path_buf();
                // Create directory first if it doesn't exist, so canonicalize works consistently
                if !requested.exists() {
                    std::fs::create_dir_all(&requested).map_err(DBCenterError::IoError)?;
                }
                let canonical = requested
                    .canonicalize()
                    .unwrap_or_else(|_| requested.clone());
                (canonical, Some(requested))
            }
            None => {
                let temp = tempfile::tempdir().map_err(DBCenterError::IoError)?;
                let temp_path = temp.path().to_path_buf();
                (temp_path.clone(), Some(temp_path))
            }
        };

        // Check if database directory exists and has a running PostgreSQL
        if path_buf.exists() {
            let postmaster_pid = path_buf.join("postmaster.pid");
            if postmaster_pid.exists() {
                // Read PID from file
                if let Ok(pid_content) = std::fs::read_to_string(&postmaster_pid) {
                    if let Some(pid_line) = pid_content.lines().next() {
                        if let Ok(pid) = pid_line.trim().parse::<u32>() {
                            // Check if process is actually running
                            let _is_running = process::is_process_running_global(pid);

                            if _is_running {
                                let hint_path = path_hint.as_ref().unwrap_or(&path_buf);
                                let shutdown_hint = format!(
                                    "DBCenter.force_shutdown(\"{}\")",
                                    escape_quotes(hint_path)
                                );
                                return Err(DBCenterError::ProcessError(format!(
                                    "PostgreSQL is already running at {:?} (PID: {}). Please shut it down first using {}.",
                                    path_buf,
                                    pid,
                                    shutdown_hint
                                )));
                            } else {
                                // Process not running, clean up stale PID file
                                tracing::info!("Removing stale postmaster.pid file");
                                let _ = std::fs::remove_file(&postmaster_pid);
                            }
                        }
                    }
                }
            }
        }

        let existing_major = binary::detect_existing_major_version(&path_buf);
        let requested_version = match postgres_version {
            Some(version) => Some(binary::normalize_postgres_version(&version)?),
            None => None,
        };

        // Create new instance
        let mut config = DBCenterConfig {
            data_dir: path_buf.clone(),
            persist,
            daemon_mode,
            host,
            port,
            password,
            multi_user_sync_mode,
            ..DBCenterConfig::default()
        };

        let (resolved_version, warning) =
            binary::resolve_artifact_version(requested_version.as_deref(), existing_major)?;
        config.postgres_version = resolved_version;

        if let Some(message) = warning {
            tracing::warn!(target: "dbcenter", "{}", message);
        }

        // Ensure binaries are available
        binary::ensure_binaries(&config)?;

        // Initialize database if needed
        process::initialize_db(&config)?;

        // Generate and persist a unique instance identifier for watcher coordination
        let instance_id = Uuid::new_v4().to_string();
        let marker_path = config.data_dir.join("dbcenter.instance");
        std::fs::write(&marker_path, &instance_id).map_err(DBCenterError::IoError)?;

        // Start PostgreSQL process
        let pg_process = process::PostgresProcess::start(&config)?;

        // Connect to database. Several connections, so concurrent callers do not
        // queue behind one another; the engine happily serves more than one and
        // the library was the thing keeping it to a single core.
        let conn_str = pg_process.connection_string(&config);
        let clients = open_pool_connections(&conn_str)?;
        tracing::debug!("Opened {} connections", clients.len());

        let db_center = Arc::new(DBCenter {
            config,
            pool: ClientPool::new(clients),
            schema_cache: Mutex::new(HashMap::new()),
            process: Arc::new(Mutex::new(pg_process)),
            path: path_buf.clone(),
            instance_id: instance_id.clone(),
        });

        // Register instance in global registry for force_shutdown capability
        let mut instances = INSTANCES.lock().map_err(|_| DBCenterError::LockError)?;
        instances.insert(path_buf, Arc::clone(&db_center));
        drop(instances);

        Ok(db_center)
    }

    /// Get the database path
    pub fn get_path(&self) -> &Path {
        &self.path
    }

    /// Get the configuration
    pub fn config(&self) -> &DBCenterConfig {
        &self.config
    }

    /// Get the unique instance identifier for this PostgreSQL run
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Get configured PostgreSQL distribution version string
    pub fn postgres_version(&self) -> &str {
        &self.config.postgres_version
    }

    /// Get the underlying PostgreSQL process ID, if running
    pub fn postgres_pid(&self) -> Option<u32> {
        self.process.lock().ok().and_then(|process| process.pid())
    }

    /// Get a collection (MongoDB-style)
    pub fn collection(&self, name: &str) -> collection::Collection<'_> {
        collection::Collection::new(self, name)
    }

    /// Get a specific GeoCollection (Geospatial API)
    pub fn geo_collection(&self, name: &str) -> geo_collection::GeoCollection<'_> {
        geo_collection::GeoCollection::new(self, name)
    }

    /// Get a VectorStore (pgvector API)
    pub fn vector_store(
        &self,
        name: &str,
        dimension: usize,
    ) -> Result<vector_store::VectorStore<'_>> {
        vector_store::VectorStore::new(self, name, dimension)
    }

    /// Execute raw SQL query
    pub fn query(
        &self,
        sql: &str,
        params: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<Vec<serde_json::Value>> {
        let mut client = self.get_client();

        let rows = client
            .query(sql, params)
            .map_err(|e| DBCenterError::QueryError(utils::format_postgres_error(&e)))?;

        let mut results = Vec::new();
        for row in rows {
            let mut map = serde_json::Map::new();
            for (idx, column) in row.columns().iter().enumerate() {
                let col_name = column.name();
                let value = utils::row_value_to_json(&row, idx);
                map.insert(col_name.to_string(), value);
            }
            results.push(serde_json::Value::Object(map));
        }

        Ok(results)
    }

    /// List all collections (tables)
    pub fn list_collections(&self) -> Result<Vec<String>> {
        let sql = "SELECT tablename FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename";
        let mut client = self.get_client();

        let rows = client
            .query(sql, &[])
            .map_err(|e| DBCenterError::QueryError(utils::format_postgres_error(&e)))?;

        Ok(rows.iter().map(|row| row.get(0)).collect())
    }

    /// Drop a collection (table)
    pub fn drop_collection(&self, name: &str) -> Result<()> {
        let sql = format!("DROP TABLE IF EXISTS \"{}\" CASCADE", name);
        let mut client = self.get_client();

        client
            .execute(&sql, &[])
            .map_err(|e| DBCenterError::QueryError(utils::format_postgres_error(&e)))?;

        tracing::info!("Dropped collection: {}", name);
        Ok(())
    }

    /// Install PostgreSQL extension (e.g., PostGIS)
    pub fn install_extension(&self, name: &str) -> Result<()> {
        extensions::install_extension(self, name)
    }

    /// Install PostgreSQL extension with optional version
    pub fn install_extension_with_version(&self, name: &str, version: Option<&str>) -> Result<()> {
        extensions::install_extension_with_version(self, name, version)
    }

    /// Add entry to pg_hba.conf and reload configuration
    ///
    /// # Arguments
    /// * `entry_type` - Type of connection: "local", "host", "hostssl", "hostnossl"
    /// * `database` - Database name or "all"
    /// * `user` - User name or "all"
    /// * `address` - CIDR address (e.g., "0.0.0.0/0", "192.168.1.0/24") - use None for local
    /// * `auth_method` - Authentication method: "trust", "md5", "scram-sha-256", "password", etc.
    pub fn add_pg_hba_entry(
        &self,
        entry_type: &str,
        database: &str,
        user: &str,
        address: Option<&str>,
        auth_method: &str,
    ) -> Result<()> {
        let pg_hba_path = self.config.data_dir.join("pg_hba.conf");

        // Build the entry line
        let entry = if entry_type == "local" {
            format!(
                "{:<7} {:<15} {:<15} {}\n",
                entry_type, database, user, auth_method
            )
        } else {
            let addr = address.unwrap_or("127.0.0.1/32");
            format!(
                "{:<7} {:<15} {:<15} {:<18} {}\n",
                entry_type, database, user, addr, auth_method
            )
        };

        // Append to pg_hba.conf
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&pg_hba_path)
            .map_err(DBCenterError::IoError)?;

        file.write_all(entry.as_bytes())
            .map_err(DBCenterError::IoError)?;

        tracing::info!("Added pg_hba.conf entry: {}", entry.trim());

        // Reload configuration
        self.reload_config()?;

        Ok(())
    }

    /// Reload PostgreSQL configuration (pg_hba.conf, postgresql.conf)
    pub fn reload_config(&self) -> Result<()> {
        let mut client = self.get_client();

        client
            .execute("SELECT pg_reload_conf()", &[])
            .map_err(|e| DBCenterError::QueryError(utils::format_postgres_error(&e)))?;

        tracing::info!("PostgreSQL configuration reloaded");
        Ok(())
    }

    /// Set or update user password
    pub fn set_user_password(&self, username: &str, password: &str) -> Result<()> {
        let mut client = self.get_client();

        // ALTER USER takes no bind parameters, so both halves have to be
        // escaped by hand. A password holding an apostrophe is ordinary, and
        // without this it would either fail or run as SQL.
        let sql = format!(
            "ALTER USER {} WITH PASSWORD {}",
            query::quote_ident(username),
            query::QueryBuilder::json_to_sql_literal(&serde_json::Value::String(
                password.to_string()
            ))
        );
        client
            .execute(&sql, &[])
            .map_err(|e| DBCenterError::QueryError(utils::format_postgres_error(&e)))?;

        tracing::info!("Password set for user: {}", username);
        Ok(())
    }

    /// Shutdown the database
    pub fn shutdown(&self) -> Result<()> {
        tracing::info!("Shutting down DBCenter instance for {:?}", self.path);

        let process = self.process.lock().map_err(|_| DBCenterError::LockError)?;

        process.shutdown(&self.config)?;
        drop(process); // Release lock before removing from registry

        // Remove from registry
        let mut instances = INSTANCES.lock().map_err(|_| DBCenterError::LockError)?;
        instances.remove(&self.path);
        drop(instances); // Release lock before potential file deletion

        tracing::info!("DBCenter instance removed from registry");

        // If persist is false, delete the database directory
        if !self.config.persist {
            tracing::info!(
                "persist=false, deleting database directory: {:?}",
                self.path
            );
            if self.path.exists() {
                std::fs::remove_dir_all(&self.path).map_err(DBCenterError::IoError)?;
                tracing::info!("Database directory deleted successfully");
            }
        } else {
            tracing::info!("persist=true, keeping database directory: {:?}", self.path);
        }

        Ok(())
    }

    /// Shutdown database by path (for atexit cleanup)
    pub fn shutdown_by_path<P: AsRef<Path>>(path: P) -> Result<()> {
        let path_buf = path
            .as_ref()
            .to_path_buf()
            .canonicalize()
            .unwrap_or_else(|_| path.as_ref().to_path_buf());

        let instances = INSTANCES.lock().map_err(|_| DBCenterError::LockError)?;

        if let Some(instance) = instances.get(&path_buf).cloned() {
            drop(instances); // Release lock before calling shutdown
            instance.shutdown()
        } else {
            Ok(()) // Already shut down or doesn't exist
        }
    }

    /// Force shutdown a specific path without deleting files
    pub fn force_shutdown<P: AsRef<Path>>(path: P) -> Result<()> {
        let raw_path = path.as_ref();
        let path_buf = raw_path
            .canonicalize()
            .unwrap_or_else(|_| raw_path.to_path_buf());

        // Remove from registry first if exists
        // CRITICAL: We must drop the instance OUTSIDE the lock to avoid deadlock
        // (since drop() also tries to acquire the lock)
        let _removed_instance = {
            let mut instances = INSTANCES.lock().map_err(|_| DBCenterError::LockError)?;
            instances.remove(&path_buf)
        };
        // _removed_instance drops here, referencing INSTANCES is safe now

        // Always use pg_ctl stop -w for reliable shutdown (blocks until complete)
        let postmaster_pid = path_buf.join("postmaster.pid");
        if !postmaster_pid.exists() {
            tracing::info!(
                "No postmaster.pid found at {:?}, nothing to shutdown",
                path_buf
            );
            return Ok(());
        }

        // Try pg_ctl stop -w first - this is the most reliable method
        let pg_ctl_path = binary::find_pg_ctl_for_data_dir(&path_buf);

        if let Some(pg_ctl) = pg_ctl_path {
            tracing::info!("Using pg_ctl stop -w for shutdown at {:?}", path_buf);
            let output = std::process::Command::new(&pg_ctl)
                .arg("stop")
                .arg("-D")
                .arg(&path_buf)
                .arg("-m")
                .arg("immediate") // immediate mode for force shutdown
                .arg("-w") // wait for shutdown to complete
                .arg("-t")
                .arg("10") // Timeout after 10 seconds
                .output();

            match output {
                Ok(result) if result.status.success() => {
                    tracing::info!("PostgreSQL shut down successfully via pg_ctl");
                    return Ok(());
                }
                Ok(result) => {
                    tracing::warn!(
                        "pg_ctl stop failed: {}",
                        String::from_utf8_lossy(&result.stderr)
                    );
                }
                Err(e) => {
                    tracing::warn!("Failed to execute pg_ctl: {}", e);
                }
            }
        }

        // Fallback: kill process directly
        if let Ok(pid_content) = std::fs::read_to_string(&postmaster_pid) {
            if let Some(pid_line) = pid_content.lines().next() {
                if let Ok(pid) = pid_line.trim().parse::<u32>() {
                    tracing::info!("Fallback: killing PostgreSQL process {}", pid);

                    #[cfg(windows)]
                    {
                        let _ = std::process::Command::new("taskkill")
                            .args(&["/F", "/T", "/PID", &pid.to_string()])
                            .output();
                    }

                    #[cfg(unix)]
                    {
                        let _ = std::process::Command::new("kill")
                            .args(["-9", &pid.to_string()])
                            .output();
                    }

                    // Wait for process to actually terminate
                    if process::wait_for_pid(pid, 10) {
                        tracing::info!("Process {} terminated confirmed.", pid);
                    } else {
                        tracing::warn!("Process {} still running after force kill attempt.", pid);
                    }

                    // Clean up postmaster.pid
                    let _ = std::fs::remove_file(&postmaster_pid);
                }
            }
        }

        tracing::info!("Force shutdown completed for {:?}", path_buf);
        Ok(())
    }

    /// Forcefully remove a database directory after ensuring the instance is shut down
    pub fn force_remove<P: AsRef<Path>>(path: P) -> Result<()> {
        let raw_path = path.as_ref();
        let path_buf = raw_path
            .canonicalize()
            .unwrap_or_else(|_| raw_path.to_path_buf());

        tracing::info!("Force remove requested for {:?}", path_buf);

        // pg_ctl stop -w blocks until PostgreSQL is fully stopped
        Self::force_shutdown(&path_buf)?;

        if path_buf.exists() {
            tracing::info!("Force remove: deleting database directory {:?}", path_buf);
            utils::remove_dir_all_with_retry(&path_buf)?;
            tracing::info!("Successfully deleted {:?}", path_buf);
        } else {
            tracing::info!("Directory {:?} does not exist, nothing to remove", path_buf);
        }

        Ok(())
    }

    /// Get the port the database is listening on
    pub fn get_port(&self) -> u16 {
        self.process.lock().unwrap().get_port()
    }

    /// Get internal client for advanced operations
    /// Columns this table is known to have, if we have looked before.
    pub(crate) fn cached_columns(&self, table: &str) -> Option<std::collections::HashSet<String>> {
        self.schema_cache.lock().ok()?.get(table).cloned()
    }

    pub(crate) fn remember_columns(&self, table: &str, columns: std::collections::HashSet<String>) {
        if let Ok(mut cache) = self.schema_cache.lock() {
            cache.insert(table.to_string(), columns);
        }
    }

    pub(crate) fn forget_columns(&self, table: &str) {
        if let Ok(mut cache) = self.schema_cache.lock() {
            cache.remove(table);
        }
    }

    /// Stop PostgreSQL and start it again with the configuration on disk.
    ///
    /// `shared_preload_libraries` is only read at startup, so an extension that
    /// needs it - pg_duckdb is the one DBCenter installs - cannot be turned on
    /// by any amount of reloading. Rather than hand the user a "now restart
    /// your database" instruction for something they never started by hand,
    /// the install does the restart itself.
    ///
    /// The port is preserved, so a client that was given a connection string
    /// before the restart can use the same one after it.
    pub fn restart(&self) -> Result<()> {
        let mut process = self.process.lock().map_err(|_| DBCenterError::LockError)?;

        tracing::info!("Restarting PostgreSQL at {:?}", self.path);
        process.shutdown(&self.config)?;

        // Reconnecting before the old postmaster has released the socket gets a
        // connection to a server that is on its way out.
        let restarted = process::PostgresProcess::start(&self.config)?;
        let conn_str = restarted.connection_string(&self.config);
        *process = restarted;
        drop(process);

        self.pool.replace_all(open_pool_connections(&conn_str)?);
        self.schema_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();

        tracing::info!("PostgreSQL restarted");
        Ok(())
    }

    pub(crate) fn get_client(&self) -> PooledClient<'_> {
        self.pool.get()
    }
}

impl Drop for DBCenter {
    fn drop(&mut self) {
        tracing::debug!("DBCenter instance dropped for {:?}", self.path);

        // If daemon_mode is disabled, shutdown automatically
        if !self.config.daemon_mode {
            // Check if this instance still owns the database by comparing instance_id
            let marker_path = self.path.join("dbcenter.instance");
            let should_shutdown = match std::fs::read_to_string(&marker_path) {
                Ok(current_id) => {
                    let matches = current_id.trim() == self.instance_id;
                    if !matches {
                        tracing::info!(
                            "Drop: instance_id mismatch (ours: {}, current: {}), skipping shutdown for {:?}",
                            self.instance_id,
                            current_id.trim(),
                            self.path
                        );
                    }
                    matches
                }
                Err(_) => {
                    // Marker file doesn't exist (directory was deleted), skip shutdown
                    tracing::info!(
                        "Drop: instance marker not found, skipping shutdown for {:?}",
                        self.path
                    );
                    false
                }
            };

            if should_shutdown {
                tracing::info!(
                    "Auto-shutting down DBCenter (daemon_mode=false) for {:?}",
                    self.path
                );

                if let Ok(process) = self.process.lock() {
                    let _ = process.shutdown(&self.config);
                }

                // Remove from registry
                if let Ok(mut instances) = INSTANCES.lock() {
                    instances.remove(&self.path);
                }

                // If persist is false, delete the database directory
                if !self.config.persist {
                    tracing::info!(
                        "Drop: persist=false, deleting database directory: {:?}",
                        self.path
                    );
                    if self.path.exists() {
                        if let Err(e) = std::fs::remove_dir_all(&self.path) {
                            tracing::error!("Failed to delete database directory in Drop: {}", e);
                        } else {
                            tracing::info!("Database directory deleted successfully in Drop");
                        }
                    }
                }
            }
        } else {
            tracing::info!(
                "DBCenter in daemon mode - keeping PostgreSQL running for {:?}",
                self.path
            );
        }
    }
}

fn escape_quotes(path: &Path) -> String {
    let mut escaped = String::new();
    for ch in path.to_string_lossy().chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            _ => escaped.push(ch),
        }
    }
    escaped
}
