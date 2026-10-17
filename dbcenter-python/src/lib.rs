use dbcenter_core::bytes::BytesMut;
use dbcenter_core::lazy_frame::LazyFrame as CoreLazyFrame;
use dbcenter_core::postgres::types::{IsNull, ToSql, Type as PgType};
use dbcenter_core::storage::{LocalStorageAdapter as CoreLocalStorageAdapter, StorageAdapter};
use dbcenter_core::sync_engine::SyncEngine as CoreSyncEngine;
use dbcenter_core::vfs::google_drive::GoogleDriveAdapter as CoreGoogleDriveAdapter;
use dbcenter_core::vfs::s3::{S3Config, S3StorageAdapter as CoreS3StorageAdapter};
use dbcenter_core::DBCenter as CoreDBCenter;
use pyo3::exceptions::{PyAttributeError, PyException, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyDict, PyDictMethods, PyList, PyListMethods, PyModule};
use pyo3::wrap_pyfunction;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(target_os = "windows")]
const WATCHER_BASENAME: &str = "dbcenter-watcher.exe";
#[cfg(not(target_os = "windows"))]
const WATCHER_BASENAME: &str = "dbcenter-watcher";

const WATCHER_BYTES: &[u8] = include_bytes!(env!("DBCENTER_WATCHER_EMBED"));
static EMBEDDED_WATCHER_PATH: OnceLock<PathBuf> = OnceLock::new();
const MAX_WATCHER_LOG_LINES: usize = 200;

type WatcherHandle = Arc<Mutex<Option<WatcherProcess>>>;
type WeakWatcherHandle = Weak<Mutex<Option<WatcherProcess>>>;

type WatcherEntry = (String, WeakWatcherHandle);

static WATCHER_REGISTRY: OnceLock<Mutex<HashMap<String, Vec<WatcherEntry>>>> = OnceLock::new();

fn watcher_registry() -> &'static Mutex<HashMap<String, Vec<WatcherEntry>>> {
    WATCHER_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn normalize_path_for_registry(path: &str) -> String {
    let candidate = Path::new(path);
    if let Ok(canonical) = candidate.canonicalize() {
        canonical.to_string_lossy().to_string()
    } else if candidate.is_absolute() {
        candidate.to_string_lossy().to_string()
    } else {
        match env::current_dir() {
            Ok(cwd) => cwd.join(candidate).to_string_lossy().to_string(),
            Err(_) => path.to_string(),
        }
    }
}

fn register_watcher_for_path(path_key: &str, instance_id: &str, watcher: &WatcherHandle) {
    let mut registry = watcher_registry().lock().unwrap();
    let entry = registry.entry(path_key.to_string()).or_default();
    entry.retain(|(_, weak)| weak.upgrade().is_some());
    entry.push((instance_id.to_string(), Arc::downgrade(watcher)));
}

fn take_watcher_for_instance(path_key: &str, instance_id: &str) -> Option<WatcherHandle> {
    let mut registry = watcher_registry().lock().unwrap();
    if let Some(entries) = registry.get_mut(path_key) {
        let mut index = None;
        for (idx, (stored_id, _)) in entries.iter().enumerate() {
            if stored_id == instance_id {
                index = Some(idx);
                break;
            }
        }

        if let Some(idx) = index {
            let (_, weak) = entries.remove(idx);
            if entries.is_empty() {
                registry.remove(path_key);
            }
            drop(registry);
            return weak.upgrade();
        }

        entries.retain(|(_, weak)| weak.upgrade().is_some());
        if entries.is_empty() {
            registry.remove(path_key);
        }
    }
    None
}

fn take_all_watchers_for_path(path_key: &str) -> Vec<WatcherHandle> {
    let mut registry = watcher_registry().lock().unwrap();
    let entries = registry.remove(path_key);
    drop(registry);

    entries
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, weak)| weak.upgrade())
        .collect()
}

struct WatcherProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    logs: Arc<Mutex<Vec<String>>>,
}

impl WatcherProcess {
    fn buffered_logs(&self) -> Vec<String> {
        self.logs
            .lock()
            .map(|lines| lines.clone())
            .unwrap_or_default()
    }
}

/// Python wrapper for DBCenter
#[pyclass]
struct DBCenter {
    inner: Arc<CoreDBCenter>,
    path: String,
    path_key: String,
    instance_id: String,
    persist: bool,
    service: bool,
    closed: Arc<Mutex<bool>>,
    watcher: WatcherHandle, // Watcher process handle
}

impl DBCenter {
    fn notify_instance_watcher(&self, status: &str) -> PyResult<()> {
        if let Some(handle) = take_watcher_for_instance(&self.path_key, &self.instance_id) {
            notify_watcher(&handle, status)
        } else {
            notify_watcher(&self.watcher, status)
        }
    }
}

#[pymethods]
impl DBCenter {
    /// Create a new DBCenter instance
    ///
    /// Args:
    ///     path: Database path (None for temporary)
    ///     persist: Whether to persist data after shutdown
    ///     service: Keep service running after program ends (daemon mode)
    ///     host: PostgreSQL host (default: localhost/127.0.0.1)
    ///     port: PostgreSQL port (None = auto-assign)
    ///     password: Password for authentication (None = trust)
    ///     postgresql: PostgreSQL major or artifact version (e.g., 17 or "17.0.0")
    ///     multi_user_sync: Enable Delta Layer (Multi-User Sync) mode (default: False)
    ///     columnar: Install pg_duckdb and keep a `<name>_columnar` view of every
    ///         synced collection (default: False). Aggregates against it run
    ///         about 19x faster than against the collection, because they read
    ///         columns off object storage instead of rows out of PostgreSQL. Not
    ///         the default because pg_duckdb takes the resident set from about
    ///         128 MB to 250 MB.
    #[new]
    #[pyo3(signature = (path=None, persist=true, service=false, host=None, port=None, password=None, postgresql=None, multi_user_sync=false, columnar=false))]
    fn new(
        py: Python,
        path: Option<String>,
        persist: bool,
        service: bool,
        host: Option<String>,
        port: Option<u16>,
        password: Option<String>,
        postgresql: Option<&Bound<'_, PyAny>>,
        multi_user_sync: bool,
        columnar: bool,
    ) -> PyResult<Self> {
        let pg_version = if let Some(value) = postgresql {
            if value.is_none() {
                None
            } else if let Ok(num) = value.extract::<u16>() {
                Some(num.to_string())
            } else {
                Some(value.extract::<String>().map_err(|_| {
                    PyValueError::new_err(
                        "postgresql must be an int (major version) or string (artifact version)",
                    )
                })?)
            }
        } else {
            None
        };

        let mut builder = CoreDBCenter::builder()
            .persist(persist)
            .daemon_mode(service)
            .multi_user_sync_mode(multi_user_sync)
            .columnar(columnar);
        if let Some(path) = path.as_deref() {
            builder = builder.path(path);
        }
        if let Some(host) = host {
            builder = builder.host(host);
        }
        if let Some(port) = port {
            builder = builder.port(port);
        }
        if let Some(password) = password {
            builder = builder.password(password);
        }
        if let Some(version) = pg_version {
            builder = builder.postgres_version(version);
        }
        let db = builder
            .build()
            .map_err(|e| PyException::new_err(e.to_string()))?;

        let db_path = db.get_path().to_string_lossy().to_string();
        let path_key = normalize_path_for_registry(&db_path);
        let instance_id = db.instance_id().to_string();

        // Start watcher process
        let postgres_pid = db.postgres_pid();
        let watcher_handle = start_watcher_process(
            py,
            &db_path,
            persist,
            service,
            postgres_pid,
            instance_id.clone(),
        )?;
        let watcher_arc: WatcherHandle = Arc::new(Mutex::new(Some(watcher_handle)));
        if !service {
            register_watcher_for_path(&path_key, &instance_id, &watcher_arc);
        }

        let instance = DBCenter {
            inner: db,
            path: db_path.clone(),
            path_key: path_key.clone(),
            instance_id: instance_id.clone(),
            persist,
            service,
            closed: Arc::new(Mutex::new(false)),
            watcher: watcher_arc,
        };

        Ok(instance)
    }

    /// Get a collection (MongoDB-style)
    ///
    /// Args:
    ///     name: Collection name
    ///
    /// Returns:
    ///     Collection object
    fn collection(&self, name: &str) -> Collection {
        Collection {
            db: self.inner.clone(),
            name: name.to_string(),
        }
    }

    /// Get a GeoCollection (Geospatial API)
    ///
    /// Args:
    ///     name: Collection name
    ///
    /// Returns:
    ///     GeoCollection object
    fn geo_collection(&self, name: &str) -> GeoCollection {
        GeoCollection {
            db: self.inner.clone(),
            name: name.to_string(),
        }
    }

    /// Get a VectorStore (pgvector API)
    fn vector_store(&self, name: &str, dimension: usize) -> PyResult<VectorStore> {
        let store = self
            .inner
            .vector_store(name, dimension)
            .map_err(|e| PyException::new_err(e.to_string()))?;

        Ok(VectorStore {
            db: self.inner.clone(),
            name: name.to_string(),
            dimension,
        })
    }

    /// Execute raw SQL query
    ///
    /// Args:
    ///     sql: SQL query string
    ///     params: Query parameters (optional), bound to $1, $2, ...
    ///     read_only: Run in a READ ONLY transaction that is rolled back, so
    ///         PostgreSQL refuses any write however the SQL is written
    ///
    /// Returns:
    ///     List of results
    #[pyo3(signature = (sql, params=None, read_only=false))]
    fn query(
        &self,
        sql: &str,
        params: Option<Vec<PyObject>>,
        read_only: bool,
    ) -> PyResult<Vec<PyObject>> {
        Python::with_gil(|py| {
            // Bind the parameters rather than ignoring them. Silently dropping
            // them left callers writing "WHERE id = $1" with a value that never
            // arrived, believing the query was parameterised.
            let bound: Vec<SqlParam> = match params {
                Some(values) => values
                    .iter()
                    .map(|v| SqlParam::from_py(v.bind(py)))
                    .collect::<PyResult<_>>()?,
                None => Vec::new(),
            };
            let refs: Vec<&(dyn ToSql + Sync)> =
                bound.iter().map(|p| p as &(dyn ToSql + Sync)).collect();

            // read_only: PostgreSQL refuses any write, in a transaction that
            // is rolled back afterwards. See DBCenter::query_read_only.
            let results = if read_only {
                self.inner.query_read_only(sql, &refs)
            } else {
                self.inner.query(sql, &refs)
            }
            .map_err(|e| PyException::new_err(e.to_string()))?;

            let py_results: Vec<PyObject> = results.iter().map(|v| json_to_py(py, v)).collect();

            Ok(py_results)
        })
    }

    /// List all collections
    fn list_collections(&self) -> PyResult<Vec<String>> {
        self.inner
            .list_collections()
            .map_err(|e| PyException::new_err(e.to_string()))
    }

    /// Drop a collection
    fn drop_collection(&self, name: &str) -> PyResult<()> {
        self.inner
            .drop_collection(name)
            .map_err(|e| PyException::new_err(e.to_string()))
    }

    /// Install extension (e.g., postgis)
    ///
    /// Args:
    ///     name: Extension name (e.g., "postgis")
    ///     version: Optional specific version (e.g., "3.6.1"). If None, auto-selects best version.
    #[pyo3(signature = (name, version=None))]
    fn install_extension(&self, name: &str, version: Option<&str>) -> PyResult<()> {
        self.inner
            .install_extension_with_version(name, version)
            .map_err(|e| PyException::new_err(e.to_string()))
    }

    /// Add entry to pg_hba.conf and reload configuration
    ///
    /// Args:
    ///     entry_type: Type of connection ("local", "host", "hostssl", "hostnossl")
    ///     database: Database name or "all"
    ///     user: User name or "all"
    ///     address: CIDR address (e.g., "0.0.0.0/0") - None for local connections
    ///     auth_method: Authentication method ("trust", "md5", "scram-sha-256")
    #[pyo3(signature = (entry_type, database, user, address=None, auth_method="scram-sha-256"))]
    fn add_pg_hba_entry(
        &self,
        entry_type: &str,
        database: &str,
        user: &str,
        address: Option<&str>,
        auth_method: &str,
    ) -> PyResult<()> {
        self.inner
            .add_pg_hba_entry(entry_type, database, user, address, auth_method)
            .map_err(|e| PyException::new_err(e.to_string()))
    }

    /// Reload PostgreSQL configuration
    fn reload_config(&self) -> PyResult<()> {
        self.inner
            .reload_config()
            .map_err(|e| PyException::new_err(e.to_string()))
    }

    /// Set or update user password
    fn set_user_password(&self, username: &str, password: &str) -> PyResult<()> {
        self.inner
            .set_user_password(username, password)
            .map_err(|e| PyException::new_err(e.to_string()))
    }

    /// Shutdown the database
    fn shutdown(&self) -> PyResult<()> {
        // Check if already closed
        let mut closed = self.closed.lock().unwrap();
        if *closed {
            tracing::info!("DBCenter already shut down, skipping");
            return Ok(());
        }

        // Don't call inner.shutdown() - let watcher handle it
        // Just notify watcher that we're done
        self.notify_instance_watcher("SUCCESS")?;

        *closed = true;
        Ok(())
    }

    /// Force shutdown a specific path (class method)
    #[staticmethod]
    fn force_shutdown(path: &str) -> PyResult<()> {
        CoreDBCenter::force_shutdown(path).map_err(|e| PyException::new_err(e.to_string()))
    }

    /// Force remove a database directory after shutdown
    #[staticmethod]
    fn force_remove(path: &str) -> PyResult<()> {
        let path_obj = Path::new(path);

        // 1. Kill potential Watcher process first!
        // This prevents the "Old Watcher executes delayed cleanup on New DB" race condition.
        let watcher_pid_file = path_obj.join("watcher.pid");
        if watcher_pid_file.exists() {
            if let Ok(content) = fs::read_to_string(&watcher_pid_file) {
                if let Ok(pid) = content.trim().parse::<u32>() {
                    tracing::info!("Found old watcher.pid: {}. Killing it...", pid);
                    #[cfg(windows)]
                    let _ = Command::new("taskkill")
                        .args(&["/F", "/PID", &pid.to_string()])
                        .output();
                    #[cfg(unix)]
                    let _ = Command::new("kill")
                        .args(&["-9", &pid.to_string()])
                        .output();
                }
            }
            let _ = fs::remove_file(watcher_pid_file);
        }

        let path_key = normalize_path_for_registry(path);
        for handle in take_all_watchers_for_path(&path_key) {
            if let Err(err) = notify_watcher(&handle, "FORCE_REMOVE") {
                tracing::warn!(
                    target: "dbcenter::watcher",
                    "Watcher notification for force_remove failed: {}",
                    err
                );
            }
        }
        CoreDBCenter::force_remove(path).map_err(|e| PyException::new_err(e.to_string()))
    }

    /// Get the database path
    #[getter]
    fn path(&self) -> String {
        self.path.clone()
    }

    /// Get the database port
    ///
    /// This used to ask the server with `inet_server_port()`, which returns NULL
    /// over a Unix socket - so it raised "Could not get port information" on
    /// every default instance, which is exactly the case where the caller has no
    /// other way to find the port. The process knows the number it was started
    /// with, so ask that instead.
    #[getter]
    fn port(&self) -> PyResult<u16> {
        Ok(self.inner.get_port())
    }

    /// A libpq connection string ("host=... port=... dbname=... user=..."),
    /// which psycopg, psql and most drivers take as it is.
    #[getter]
    fn dsn(&self) -> String {
        self.inner.connection_string()
    }

    /// The same as a postgresql:// URL, which SQLAlchemy and most ORMs want.
    /// Over a Unix socket the host goes in the query string, the form libpq
    /// and psycopg read.
    #[getter]
    fn url(&self) -> String {
        let mut fields = HashMap::new();
        for part in self.inner.connection_string().split_whitespace() {
            if let Some((k, v)) = part.split_once('=') {
                fields.insert(k.to_string(), v.to_string());
            }
        }
        let get = |k: &str| fields.get(k).cloned().unwrap_or_default();
        let enc = |v: &str| -> String {
            v.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{:02X}", b),
                })
                .collect()
        };
        let auth = match fields.get("password") {
            Some(p) => format!("{}:{}", enc(&get("user")), enc(p)),
            None => enc(&get("user")),
        };
        let host = get("host");
        if host.starts_with('/') {
            format!(
                "postgresql://{}@/{}?host={}&port={}",
                auth,
                enc(&get("dbname")),
                enc(&host),
                get("port")
            )
        } else {
            format!(
                "postgresql://{}@{}:{}/{}",
                auth,
                host,
                get("port"),
                enc(&get("dbname"))
            )
        }
    }

    /// Support attribute access: db.users -> Collection
    /// But don't override special attributes like 'path'
    fn __getattr__(&self, name: &str) -> PyResult<PyObject> {
        Python::with_gil(|py| {
            // Don't override getter properties
            if name == "path" {
                return Err(PyException::new_err("Use db.path directly"));
            }
            Ok(self.collection(name).into_py(py))
        })
    }

    /// String representation
    fn __repr__(&self) -> String {
        format!("<DBCenter path='{}'>", self.path)
    }

    /// Context manager support: __enter__
    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Context manager support: __exit__
    fn __exit__(
        &mut self,
        _exc_type: Option<PyObject>,
        _exc_value: Option<PyObject>,
        _traceback: Option<PyObject>,
    ) -> PyResult<bool> {
        // Check if already closed
        let mut closed = self.closed.lock().unwrap();
        if *closed {
            tracing::info!("DBCenter already shut down in __exit__, skipping");
            return Ok(false);
        }

        // If not in service mode, notify watcher when exiting context
        if !self.service {
            tracing::info!(
                "Context manager __exit__ called for DBCenter at {}, notifying watcher...",
                self.path
            );

            // Watcher will handle PostgreSQL shutdown and cleanup
            self.notify_instance_watcher("SUCCESS")?;

            *closed = true;
        } else {
            tracing::info!("Context manager __exit__ called for DBCenter at {} (service mode, keeping running)", self.path);
        }
        Ok(false) // Don't suppress exceptions
    }

    /// Python __del__ method - ensures proper cleanup
    fn __del__(&mut self) -> PyResult<()> {
        // Check if already closed
        let mut closed = self.closed.lock().unwrap();
        if *closed {
            tracing::debug!("DBCenter already shut down in __del__, skipping");
            return Ok(());
        }

        // Notify watcher when Python object is destroyed
        if !self.service {
            tracing::info!(
                "Python __del__ called for DBCenter at {}, notifying watcher...",
                self.path
            );
            // Watcher will handle PostgreSQL shutdown and cleanup
            let _ = self.notify_instance_watcher("SUCCESS");
            *closed = true;
        } else {
            tracing::info!(
                "Python __del__ called for DBCenter at {} (service mode, keeping running)",
                self.path
            );
        }
        Ok(())
    }
    /// Create a builder for configuring DBCenter
    #[staticmethod]
    fn builder() -> DBCenterBuilder {
        DBCenterBuilder::default()
    }
}

/// Python Builder for DBCenter
#[pyclass]
#[derive(Default)]
struct DBCenterBuilder {
    path: Option<String>,
    persist: bool,
    service: bool,
    host: Option<String>,
    port: Option<u16>,
    password: Option<String>,
    postgres_version: Option<String>,
    multi_user_sync_mode: bool,
    columnar: bool,
}

#[pymethods]
impl DBCenterBuilder {
    #[new]
    fn new() -> Self {
        Self {
            persist: true, // Default true
            ..Default::default()
        }
    }

    fn path(mut self_: PyRefMut<Self>, path: String) -> PyRefMut<Self> {
        self_.path = Some(path);
        self_
    }

    fn persist(mut self_: PyRefMut<Self>, persist: bool) -> PyRefMut<Self> {
        self_.persist = persist;
        self_
    }

    fn service(mut self_: PyRefMut<Self>, service: bool) -> PyRefMut<Self> {
        self_.service = service;
        self_
    }

    fn host(mut self_: PyRefMut<Self>, host: String) -> PyRefMut<Self> {
        self_.host = Some(host);
        self_
    }

    fn port(mut self_: PyRefMut<Self>, port: u16) -> PyRefMut<Self> {
        self_.port = Some(port);
        self_
    }

    fn password(mut self_: PyRefMut<Self>, password: String) -> PyRefMut<Self> {
        self_.password = Some(password);
        self_
    }

    fn postgres_version(mut self_: PyRefMut<Self>, version: String) -> PyRefMut<Self> {
        self_.postgres_version = Some(version);
        self_
    }

    fn multi_user_sync_mode(mut self_: PyRefMut<Self>, enabled: bool) -> PyRefMut<Self> {
        self_.multi_user_sync_mode = enabled;
        self_
    }

    /// Install pg_duckdb and keep a `<name>_columnar` view of every synced
    /// collection. See `DBCenter(columnar=...)`.
    fn columnar(mut self_: PyRefMut<Self>, enabled: bool) -> PyRefMut<Self> {
        self_.columnar = enabled;
        self_
    }

    fn build(self_: PyRef<Self>, py: Python<'_>) -> PyResult<DBCenter> {
        let pg_ver_bound = if let Some(ver) = &self_.postgres_version {
            Some(ver.clone().into_py(py).bind(py).clone())
        } else {
            None
        };

        let pg_ver_ref = pg_ver_bound.as_ref();

        DBCenter::new(
            py,
            self_.path.clone(),
            self_.persist,
            self_.service,
            self_.host.clone(),
            self_.port,
            self_.password.clone(),
            pg_ver_ref,
            self_.multi_user_sync_mode,
            self_.columnar,
        )
    }
}

// ============================================================================
// Watcher Process Management
// ============================================================================

fn start_watcher_process(
    py: Python<'_>,
    data_dir: &str,
    persist: bool,
    service: bool,
    postgres_pid: Option<u32>,
    instance_id: String,
) -> PyResult<WatcherProcess> {
    let watcher_path = resolve_watcher_path(py)?;

    let mut cmd = Command::new(&watcher_path);
    cmd.stdin(Stdio::piped()).stdout(Stdio::null());

    // On Windows, create watcher without visible window
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW (0x08000000) - prevents console window from appearing
        cmd.creation_flags(0x08000000);
        cmd.stderr(Stdio::null()); // No stderr pipe on Windows
    }

    #[cfg(not(windows))]
    {
        cmd.stderr(Stdio::piped());
    }

    let mut child = cmd.spawn().map_err(|e| {
        PyException::new_err(format!(
            "Watcher başlatma hatası: {}. Binary yolu: {}",
            e,
            watcher_path.display()
        ))
    })?;

    let mut stdin_handle = child.stdin.take().ok_or_else(|| {
        PyException::new_err("Watcher stdin alınamadı (piped stdin yapılandırılamadı)")
    })?;

    // Send configuration to watcher
    let config = serde_json::json!({
        "data_dir": data_dir,
        "persist": persist,
        "service": service,
        "postgres_pid": postgres_pid,
        "instance_id": instance_id,
    });
    writeln!(stdin_handle, "{}", config.to_string())
        .map_err(|e| PyException::new_err(format!("Watcher config gönderme hatası: {}", e)))?;
    stdin_handle
        .flush()
        .map_err(|e| PyException::new_err(format!("Watcher stdin flush hatası: {}", e)))?;

    let logs = Arc::new(Mutex::new(Vec::new()));
    if let Some(stderr) = child.stderr.take() {
        let logs_sink = Arc::clone(&logs);
        let show_realtime = watcher_logs_enabled();
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        let line = line.trim_end().to_string();
                        if show_realtime {
                            eprintln!("{}", line);
                        }
                        tracing::debug!(target: "dbcenter::watcher", "{}", line);
                        if let Ok(mut buffer) = logs_sink.lock() {
                            if buffer.len() < MAX_WATCHER_LOG_LINES {
                                buffer.push(line);
                            } else if buffer.len() == MAX_WATCHER_LOG_LINES {
                                buffer.push("... watcher output truncated".to_string());
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    if watcher_logs_enabled() {
        eprintln!(
            "[DBCENTER-PYTHON] Watcher process started (PID: {:?})",
            child.id()
        );
    } else {
        tracing::info!(
            target: "dbcenter::watcher",
            "Watcher process started (PID: {:?})",
            child.id()
        );
    }

    // Write watcher.pid
    let watcher_pid_path = Path::new(data_dir).join("watcher.pid");
    if let Err(e) = std::fs::write(&watcher_pid_path, child.id().to_string()) {
        tracing::warn!("Failed to write watcher.pid: {}", e);
    }

    Ok(WatcherProcess {
        child,
        stdin: Some(stdin_handle),
        logs,
    })
}

fn resolve_watcher_path(py: Python<'_>) -> PyResult<PathBuf> {
    let mut attempted: Vec<String> = Vec::new();

    if let Ok(explicit) = std::env::var("DBCENTER_WATCHER_PATH") {
        let explicit_path = PathBuf::from(explicit);
        if explicit_path.is_file() {
            return Ok(explicit_path);
        }
        attempted.push(explicit_path.display().to_string());
    }

    if let Ok(module) = PyModule::import_bound(py, "dbcenter") {
        if let Ok(file_attr) = module.getattr("__file__") {
            if let Ok(path_str) = file_attr.extract::<String>() {
                if let Some(parent) = Path::new(&path_str).parent() {
                    let candidate = parent.join(WATCHER_BASENAME);
                    if candidate.is_file() {
                        return Ok(candidate);
                    }
                    attempted.push(candidate.display().to_string());
                }
            }
        }
    }

    let dev_candidates = [
        PathBuf::from(format!("target/release/{}", WATCHER_BASENAME)),
        PathBuf::from(format!("../target/release/{}", WATCHER_BASENAME)),
        PathBuf::from(format!(
            "dbcenter-python/target/release/{}",
            WATCHER_BASENAME
        )),
        PathBuf::from(format!(
            "../dbcenter-watcher/target/release/{}",
            WATCHER_BASENAME
        )),
        PathBuf::from(format!("rust-dbcenter/target/release/{}", WATCHER_BASENAME)),
        PathBuf::from(WATCHER_BASENAME),
    ];

    // is_file, not exists: `dbcenter-watcher` is also the name of a directory in
    // this workspace, and exists() is true for one. Anyone running from the repo
    // root - or from any project with a folder of that name - got the directory
    // back as the watcher path and then a bare "No such file or directory" from
    // the spawn, which points nowhere near the cause.
    for candidate in dev_candidates {
        if candidate.is_file() {
            return Ok(candidate);
        }
        attempted.push(candidate.display().to_string());
    }

    match ensure_embedded_watcher_path() {
        Ok(path) => Ok(path),
        Err(err) => {
            attempted.push("<gömülü watcher>".to_string());
            let err_msg = err.to_string();
            let attempted_list = if attempted.is_empty() {
                "(yok)".to_string()
            } else {
                attempted.join(", ")
            };
            Err(PyException::new_err(format!(
                "Watcher binary bulunamadı. Denenen yollar: {}. Gömülü watcher oluşturma hatası: {}",
                attempted_list,
                err_msg
            )))
        }
    }
}

fn ensure_embedded_watcher_path() -> PyResult<PathBuf> {
    if let Some(path) = EMBEDDED_WATCHER_PATH.get() {
        return Ok(path.clone());
    }

    let mut base_dir = std::env::temp_dir();
    base_dir.push("dbcenter-python");
    fs::create_dir_all(&base_dir).map_err(|e| {
        PyException::new_err(format!("Gömülü watcher dizini oluşturma hatası: {}", e))
    })?;

    let watcher_path = base_dir.join(WATCHER_BASENAME);
    if !watcher_path.exists() {
        fs::write(&watcher_path, WATCHER_BYTES)
            .map_err(|e| PyException::new_err(format!("Gömülü watcher yazma hatası: {}", e)))?;
        #[cfg(unix)]
        {
            let perms = fs::Permissions::from_mode(0o755);
            fs::set_permissions(&watcher_path, perms).map_err(|e| {
                PyException::new_err(format!("Watcher izin ayarlama hatası: {}", e))
            })?;
        }
    }

    let _ = EMBEDDED_WATCHER_PATH.set(watcher_path.clone());
    Ok(watcher_path)
}

fn watcher_logs_enabled() -> bool {
    match std::env::var("DBCENTER_SHOW_WATCHER_LOGS") {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "" => false,
            "1" | "true" | "yes" | "on" => true,
            _ => false,
        },
        Err(_) => false,
    }
}

fn notify_watcher(watcher: &WatcherHandle, status: &str) -> PyResult<()> {
    let mut watcher_guard = watcher.lock().unwrap();
    if let Some(mut handle) = watcher_guard.take() {
        if let Some(mut stdin) = handle.stdin.take() {
            writeln!(stdin, "{}", status)
                .map_err(|e| PyException::new_err(format!("Watcher notify hatası: {}", e)))?;
            stdin
                .flush()
                .map_err(|e| PyException::new_err(format!("Watcher stdin flush hatası: {}", e)))?;
        } else {
            tracing::warn!(
                target: "dbcenter::watcher",
                "Watcher stdin handle already closed before notify"
            );
        }

        let exit_status = handle
            .child
            .wait()
            .map_err(|e| PyException::new_err(format!("Watcher bekleme hatası: {}", e)))?;

        let logs_snapshot = handle.buffered_logs();

        if !exit_status.success() {
            let status_desc = exit_status
                .code()
                .map(|code| format!("exit code {}", code))
                .unwrap_or_else(|| "terminated by signal".to_string());

            let mut message = format!(
                "Watcher beklenmeyen şekilde kapandı ({}) during cleanup.",
                status_desc
            );

            if !logs_snapshot.is_empty() {
                message.push_str("\nWatcher output:\n");
                message.push_str(&logs_snapshot.join("\n"));
            }

            return Err(PyException::new_err(message));
        }

        if !logs_snapshot.is_empty() && !watcher_logs_enabled() {
            if let Some(summary) = logs_snapshot
                .iter()
                .rev()
                .find(|line| line.contains("Temizlik tamamlandı"))
            {
                tracing::info!(target: "dbcenter::watcher", "{}", summary);
            } else {
                tracing::debug!(
                    target: "dbcenter::watcher",
                    "Watcher output ({} lines)\n{}",
                    logs_snapshot.len(),
                    logs_snapshot.join("\n")
                );
            }
        }
    }
    Ok(())
}

/// Python wrapper for Collection
#[pyclass]
struct Collection {
    db: Arc<CoreDBCenter>,
    name: String,
}

#[pymethods]
impl Collection {
    /// Insert one document
    ///
    /// Args:
    ///     document: Python dict
    ///
    /// Returns:
    ///     Document ID
    fn insert_one(&self, document: &Bound<'_, PyDict>) -> PyResult<String> {
        Python::with_gil(|py| {
            let json_doc = py_to_json(py, document.as_any())?;

            let coll = self.db.collection(&self.name);
            coll.insert_one(&json_doc)
                .map_err(|e| PyException::new_err(e.to_string()))
        })
    }

    /// Insert many documents
    fn insert_many(&self, documents: &Bound<'_, PyList>) -> PyResult<Vec<String>> {
        Python::with_gil(|py| {
            let json_docs: Vec<JsonValue> = documents
                .iter()
                .map(|item| py_to_json(py, item.as_any()))
                .collect::<PyResult<_>>()?;

            let coll = self.db.collection(&self.name);
            coll.insert_many(&json_docs)
                .map_err(|e| PyException::new_err(e.to_string()))
        })
    }

    /// Find documents
    ///
    /// Args:
    ///     query: Query dict (None for all)
    ///     limit: Maximum number of results
    ///     skip: Number of results to skip
    ///     sort: Sort specification
    ///
    /// Returns:
    ///     List of documents
    #[pyo3(signature = (query=None, _limit=None, _skip=None, _sort=None))]
    fn find(
        &self,
        query: Option<&Bound<'_, PyDict>>,
        _limit: Option<usize>,
        _skip: Option<usize>,
        _sort: Option<Vec<(String, i32)>>,
    ) -> PyResult<Vec<PyObject>> {
        Python::with_gil(|py| {
            let query_map = if let Some(q) = query {
                Some(py_dict_to_hashmap(py, q)?)
            } else {
                None
            };

            let coll = self.db.collection(&self.name);
            let results = coll
                .find(query_map.as_ref())
                .map_err(|e| PyException::new_err(e.to_string()))?;

            let py_results: Vec<PyObject> = results.iter().map(|v| json_to_py(py, v)).collect();

            Ok(py_results)
        })
    }

    /// Find one document
    #[pyo3(signature = (query=None))]
    fn find_one(&self, query: Option<&Bound<'_, PyDict>>) -> PyResult<Option<PyObject>> {
        Python::with_gil(|py| {
            let query_map = if let Some(q) = query {
                Some(py_dict_to_hashmap(py, q)?)
            } else {
                None
            };

            let coll = self.db.collection(&self.name);
            let result = coll
                .find_one(query_map.as_ref())
                .map_err(|e| PyException::new_err(e.to_string()))?;

            Ok(result.map(|v| json_to_py(py, &v)))
        })
    }

    /// Count documents
    #[pyo3(signature = (query=None))]
    fn count(&self, query: Option<&Bound<'_, PyDict>>) -> PyResult<i64> {
        Python::with_gil(|py| {
            let query_map = if let Some(q) = query {
                Some(py_dict_to_hashmap(py, q)?)
            } else {
                None
            };

            let coll = self.db.collection(&self.name);
            coll.count(query_map.as_ref())
                .map_err(|e| PyException::new_err(e.to_string()))
        })
    }

    /// Update one document
    fn update_one(&self, query: &Bound<'_, PyDict>, update: &Bound<'_, PyDict>) -> PyResult<i32> {
        Python::with_gil(|py| {
            let query_map = py_dict_to_hashmap(py, query)?;
            let update_map = py_dict_to_hashmap(py, update)?;

            let coll = self.db.collection(&self.name);
            coll.update_one(&query_map, &update_map)
                .map_err(|e| PyException::new_err(e.to_string()))
        })
    }

    /// Update many documents
    fn update_many(&self, query: &Bound<'_, PyDict>, update: &Bound<'_, PyDict>) -> PyResult<u64> {
        Python::with_gil(|py| {
            let query_map = py_dict_to_hashmap(py, query)?;
            let update_map = py_dict_to_hashmap(py, update)?;

            let coll = self.db.collection(&self.name);
            coll.update_many(&query_map, &update_map)
                .map_err(|e| PyException::new_err(e.to_string()))
        })
    }

    /// Delete one document
    fn delete_one(&self, query: &Bound<'_, PyDict>) -> PyResult<i32> {
        Python::with_gil(|py| {
            let query_map = py_dict_to_hashmap(py, query)?;

            let coll = self.db.collection(&self.name);
            coll.delete_one(&query_map)
                .map_err(|e| PyException::new_err(e.to_string()))
        })
    }

    /// Delete many documents
    #[pyo3(signature = (query=None))]
    fn delete_many(&self, query: Option<&Bound<'_, PyDict>>) -> PyResult<u64> {
        Python::with_gil(|py| {
            let query_map = if let Some(q) = query {
                Some(py_dict_to_hashmap(py, q)?)
            } else {
                None
            };

            let coll = self.db.collection(&self.name);
            coll.delete_many(query_map.as_ref())
                .map_err(|e| PyException::new_err(e.to_string()))
        })
    }

    /// Fuzzy search (typo-tolerant)
    ///
    /// Args:
    ///     query: Search string
    ///     field: Field to search in
    ///     mode: Search mode ("substring" or "exact", default "substring")
    ///     threshold: Similarity threshold (0.0 - 1.0, default 0.3)
    #[pyo3(signature = (query, field, mode=None, threshold=None))]
    fn fuzzy_search(
        &self,
        query: &str,
        field: &str,
        mode: Option<&str>,
        threshold: Option<f32>,
    ) -> PyResult<Vec<PyObject>> {
        Python::with_gil(|py| {
            let coll = self.db.collection(&self.name);
            let results = coll
                .fuzzy_search(query, field, mode, threshold)
                .map_err(|e| PyException::new_err(e.to_string()))?;

            let py_results: Vec<PyObject> = results.iter().map(|v| json_to_py(py, v)).collect();
            Ok(py_results)
        })
    }

    /// Calculate similarity between two strings
    fn similarity(&self, s1: &str, s2: &str) -> PyResult<f32> {
        let coll = self.db.collection(&self.name);
        coll.similarity(s1, s2)
            .map_err(|e| PyException::new_err(e.to_string()))
    }

    /// Create index on a field
    fn create_index(&self, field: &str, index_type: &str) -> PyResult<()> {
        let coll = self.db.collection(&self.name);
        coll.create_index(field, index_type)
            .map_err(|e| PyException::new_err(e.to_string()))
    }

    /// Attribute access for columns (e.g. coll.age) or lazy chaining
    fn __getattr__(&self, name: &str) -> PyResult<PyObject> {
        // If attribute is a valid column name, return its values (eagerly)?
        // Or return a lazy expression?
        // User log showed `db.test.abc` expecting something.
        // Let's implement simple eager column fetch for now to match intuitive interactive use.

        Python::with_gil(|py| {
            // Avoid recursion for special methods
            if name.starts_with("__") {
                return Err(PyAttributeError::new_err(format!(
                    "'Collection' object has no attribute '{}'",
                    name
                )));
            }

            // Try to query just this column
            // We can use our existing infrastructure
            let sql = format!("SELECT \"{}\" FROM \"{}\" LIMIT 100", name, self.name);
            let res = self.db.query(&sql, &[]);

            match res {
                Ok(rows) => {
                    // Extract just the values
                    let mut values = Vec::new();
                    for row in rows {
                        // Row is {"col": val}
                        if let Some(val) = row.as_object().and_then(|m| m.get(name)) {
                            values.push(val.clone());
                        }
                    }
                    Ok(json_to_py(py, &JsonValue::Array(values)))
                }
                Err(_) => {
                    // Not a column or query failed, assume it's not an attribute
                    Err(PyAttributeError::new_err(format!(
                        "'Collection' object has no attribute '{}'",
                        name
                    )))
                }
            }
        })
    }

    /// String representation - Show preview of data
    fn __repr__(&self) -> String {
        // Fetch first 5 rows for preview
        let preview_sql = format!("SELECT * FROM \"{}\" LIMIT 5", self.name);
        match self.db.query(&preview_sql, &[]) {
            Ok(rows) => {
                if rows.is_empty() {
                    return format!("<Collection '{}': (empty)>", self.name);
                }

                // Simple formatting
                let count = self
                    .db
                    .query(&format!("SELECT count(*) FROM \"{}\"", self.name), &[])
                    .map(|r| {
                        r.first()
                            .and_then(|row| row.get("count"))
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0)
                    })
                    .unwrap_or(0);

                format!("<Collection '{}': {} rows>\n{:?}", self.name, count, rows)
            }
            Err(_) => format!("<Collection '{}' (error loading preview)>", self.name),
        }
    }

    fn __len__(&self) -> PyResult<usize> {
        self.count(None).map(|c| c as usize)
    }

    // --- Arrow / LazyFrame API ---

    /// Begin a lazy query (builds Query Plan)
    fn lazy(&self) -> LazyFrame {
        LazyFrame {
            inner: Some(CoreLazyFrame::new(self.db.clone(), &self.name)),
        }
    }

    /// Convert collection to Arrow Table (via IPC)
    fn to_arrow(&self, py: Python<'_>) -> PyResult<PyObject> {
        self.lazy().to_arrow(py)
    }

    /// Convert collection to Polars DataFrame
    fn to_polars(&self, py: Python<'_>) -> PyResult<PyObject> {
        self.lazy().to_polars(py)
    }

    /// Convert collection to Pandas DataFrame
    fn to_pandas(&self, py: Python<'_>) -> PyResult<PyObject> {
        self.lazy().to_pandas(py)
    }
}

/// Python wrapper for GeoCollection
#[pyclass]
struct GeoCollection {
    db: Arc<CoreDBCenter>,
    name: String,
}

#[pymethods]
impl GeoCollection {
    /// Insert many documents (delegates to standard collection)
    fn insert_many(&self, documents: &Bound<'_, PyList>) -> PyResult<Vec<String>> {
        Python::with_gil(|py| {
            let json_docs: Vec<JsonValue> = documents
                .iter()
                .map(|item| py_to_json(py, item.as_any()))
                .collect::<PyResult<_>>()?;

            // We create a temporary core GeoCollection to leverage its logic (installation, indexing)
            let coll = self.db.geo_collection(&self.name);
            coll.insert_many(&json_docs)
                .map_err(|e| PyException::new_err(e.to_string()))
        })
    }

    /// Find points within radius (km)
    #[pyo3(signature = (lat, lon, radius_km))]
    fn find_within_radius(&self, lat: f64, lon: f64, radius_km: f64) -> PyResult<Vec<PyObject>> {
        Python::with_gil(|py| {
            let coll = self.db.geo_collection(&self.name);
            let results = coll
                .find_within_radius(lat, lon, radius_km)
                .map_err(|e| PyException::new_err(e.to_string()))?;

            let py_results: Vec<PyObject> = results.iter().map(|v| json_to_py(py, v)).collect();
            Ok(py_results)
        })
    }

    /// Calculate distance between two documents
    fn distance_between(&self, from_id: PyObject, to_id: PyObject) -> PyResult<Option<f64>> {
        Python::with_gil(|py| {
            let val1 = py_to_json(py, from_id.bind(py))?;
            let val2 = py_to_json(py, to_id.bind(py))?;

            let coll = self.db.geo_collection(&self.name);
            coll.distance_between(&val1, &val2)
                .map_err(|e| PyException::new_err(e.to_string()))
        })
    }

    /// Check if point is inside a polygon (identified by ID)
    fn contains_point(&self, polygon_id: PyObject, lat: f64, lon: f64) -> PyResult<bool> {
        Python::with_gil(|py| {
            let val = py_to_json(py, polygon_id.bind(py))?;

            let coll = self.db.geo_collection(&self.name);
            coll.contains_point(&val, lat, lon)
                .map_err(|e| PyException::new_err(e.to_string()))
        })
    }
}

/// Python wrapper for VectorStore
#[pyclass]
struct VectorStore {
    db: Arc<CoreDBCenter>,
    name: String,
    dimension: usize,
}

#[pymethods]
impl VectorStore {
    /// Add embeddings
    /// ids: List[str]
    /// embeddings: List[List[float]]
    /// metadatas: Optional[List[Dict]]
    #[pyo3(signature = (ids, embeddings, metadatas=None))]
    fn add(
        &self,
        ids: Vec<String>,
        embeddings: Vec<Vec<f32>>,
        metadatas: Option<Vec<HashMap<String, PyObject>>>,
    ) -> PyResult<()> {
        Python::with_gil(|py| {
            // Convert metadatas from PyObject to JsonValue
            let json_metas = if let Some(ms) = metadatas {
                let mut converted = Vec::new();
                for m in ms {
                    let mut json_map = HashMap::new();
                    for (k, v) in m {
                        json_map.insert(k, py_to_json(py, v.bind(py))?);
                    }
                    converted.push(json_map);
                }
                Some(converted)
            } else {
                None
            };

            let store = self
                .db
                .vector_store(&self.name, self.dimension)
                .map_err(|e| PyException::new_err(e.to_string()))?;
            store
                .add(&ids, &embeddings, json_metas.as_deref())
                .map_err(|e| PyException::new_err(e.to_string()))
        })
    }

    /// Query embeddings
    /// query_embeddings: List[List[float]] (list of vectors)
    /// n_results: int
    /// where: Optional[Dict]
    #[pyo3(signature = (query_embeddings, n_results=10, r#where=None))]
    fn query(
        &self,
        query_embeddings: Vec<Vec<f32>>,
        n_results: i64,
        r#where: Option<HashMap<String, PyObject>>,
    ) -> PyResult<Vec<PyObject>> {
        Python::with_gil(|py| {
            // where is a dict, convert to json map
            let filter = if let Some(w) = r#where {
                let mut map = HashMap::new();
                for (k, v) in w {
                    map.insert(k, py_to_json(py, v.bind(py))?);
                }
                Some(map)
            } else {
                None
            };

            let store = self
                .db
                .vector_store(&self.name, self.dimension)
                .map_err(|e| PyException::new_err(e.to_string()))?;
            // Pass the vector list directly
            let results = store
                .query(&query_embeddings, n_results, filter.as_ref())
                .map_err(|e| PyException::new_err(e.to_string()))?;

            let py_results: Vec<PyObject> = results.iter().map(|v| json_to_py(py, v)).collect();
            Ok(py_results)
        })
    }
}

// ============================================================================
// Helper Functions: Python <-> JSON conversion
// ============================================================================

/// A Python value bound as a PostgreSQL query parameter.
///
/// Only the scalar types PostgreSQL can infer on its own are accepted. Anything
/// else is rejected with a clear message rather than being coerced into a shape
/// the query did not ask for.
#[derive(Debug)]
enum SqlParam {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Json(JsonValue),
}

impl SqlParam {
    fn from_py(obj: &Bound<'_, PyAny>) -> PyResult<Self> {
        if obj.is_none() {
            return Ok(SqlParam::Null);
        }
        // bool before int: Python's bool is a subclass of int.
        if let Ok(v) = obj.extract::<bool>() {
            return Ok(SqlParam::Bool(v));
        }
        if let Ok(v) = obj.extract::<i64>() {
            return Ok(SqlParam::Int(v));
        }
        if let Ok(v) = obj.extract::<f64>() {
            return Ok(SqlParam::Float(v));
        }
        if let Ok(v) = obj.extract::<String>() {
            return Ok(SqlParam::Text(v));
        }
        if obj.is_instance_of::<PyDict>() || obj.is_instance_of::<PyList>() {
            let py = obj.py();
            return Ok(SqlParam::Json(py_to_json(py, obj)?));
        }
        Err(PyException::new_err(format!(
            "Unsupported query parameter of type '{}'. \
             Supported: None, bool, int, float, str, dict, list",
            obj.get_type().name()?
        )))
    }
}

/// Encode a decimal string in PostgreSQL's binary NUMERIC format.
///
/// The inverse of the decoder in dbcenter-core: `ndigits`, `weight`, `sign`,
/// `dscale`, then base-10000 groups. Needed because rust-postgres maps NUMERIC
/// only when built against a decimal crate, and `postgres` 0.19 has no such
/// feature - so `SELECT $1::numeric` with a Python number had nothing to encode
/// it with.
fn encode_numeric(text: &str, out: &mut BytesMut) -> Result<(), String> {
    let text = text.trim();
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };

    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(format!("'{}' is not a number", text));
    }
    if !int_part
        .bytes()
        .chain(frac_part.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return Err(format!("'{}' is not a plain decimal", text));
    }

    let dscale = frac_part.len();

    // Group into base-10000 digits: the integer part padded on the left to a
    // multiple of four, the fraction padded on the right.
    let int_pad = (4 - int_part.len() % 4) % 4;
    let padded_int = format!("{}{}", "0".repeat(int_pad), int_part);
    let frac_pad = (4 - frac_part.len() % 4) % 4;
    let padded_frac = format!("{}{}", frac_part, "0".repeat(frac_pad));

    let mut digits: Vec<i16> = Vec::new();
    for chunk in padded_int.as_bytes().chunks(4) {
        digits.push(std::str::from_utf8(chunk).unwrap().parse().unwrap());
    }
    let int_groups = digits.len() as i32;
    for chunk in padded_frac.as_bytes().chunks(4) {
        digits.push(std::str::from_utf8(chunk).unwrap().parse().unwrap());
    }

    // weight counts groups before the point, less one. Leading zero groups are
    // dropped and weight follows them down.
    let mut weight = int_groups - 1;
    while digits.first() == Some(&0) && weight >= 0 {
        digits.remove(0);
        weight -= 1;
    }
    while digits.last() == Some(&0) {
        digits.pop();
    }
    if digits.is_empty() {
        weight = 0;
    }

    out.extend_from_slice(&(digits.len() as i16).to_be_bytes());
    out.extend_from_slice(&(weight as i16).to_be_bytes());
    out.extend_from_slice(&(if negative { 0x4000u16 } else { 0x0000u16 }).to_be_bytes());
    out.extend_from_slice(&(dscale as i16).to_be_bytes());
    for d in digits {
        out.extend_from_slice(&d.to_be_bytes());
    }
    Ok(())
}

/// Send an integer as a narrower PostgreSQL type, or refuse if it does not fit.
fn narrow<T>(
    value: i64,
    ty: &PgType,
    out: &mut BytesMut,
) -> std::result::Result<IsNull, Box<dyn std::error::Error + Sync + Send>>
where
    T: TryFrom<i64> + ToSql,
{
    match T::try_from(value) {
        Ok(v) => v.to_sql_checked(ty, out),
        Err(_) => Err(format!("{} does not fit in a {}", value, ty.name()).into()),
    }
}

impl ToSql for SqlParam {
    fn to_sql(
        &self,
        ty: &PgType,
        out: &mut BytesMut,
    ) -> std::result::Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        // NUMERIC first: PostgreSQL infers it for `$1::numeric`, and writing an
        // eight-byte float into that field produced "invalid sign in external
        // numeric value" - or, worse, could have produced a plausible wrong
        // number.
        if *ty == PgType::NUMERIC {
            let text = match self {
                SqlParam::Null => return Ok(IsNull::Yes),
                SqlParam::Int(v) => v.to_string(),
                SqlParam::Float(v) => {
                    if !v.is_finite() {
                        return Err(format!("{} cannot be stored as numeric", v).into());
                    }
                    format!("{}", v)
                }
                SqlParam::Text(v) => v.clone(),
                other => {
                    return Err(format!("{:?} cannot be bound to a numeric column", other).into())
                }
            };
            encode_numeric(&text, out)
                .map_err(|e| -> Box<dyn std::error::Error + Sync + Send> { e.into() })?;
            return Ok(IsNull::No);
        }

        // Python has one integer type and one float type; PostgreSQL has several
        // widths, and rust-postgres will not encode an i64 into an int4 field.
        // Narrow where the value fits and say so plainly where it does not,
        // rather than letting `$1::int` fail with "error serializing parameter".
        match self {
            SqlParam::Null => Ok(IsNull::Yes),
            SqlParam::Bool(v) => v.to_sql_checked(ty, out),

            SqlParam::Int(v) => match *ty {
                PgType::INT2 => narrow::<i16>(*v, ty, out),
                PgType::INT4 => narrow::<i32>(*v, ty, out),
                PgType::FLOAT4 => (*v as f32).to_sql_checked(ty, out),
                PgType::FLOAT8 => (*v as f64).to_sql_checked(ty, out),
                _ => v.to_sql_checked(ty, out),
            },

            SqlParam::Float(v) => match *ty {
                PgType::FLOAT4 => (*v as f32).to_sql_checked(ty, out),
                PgType::INT2 | PgType::INT4 | PgType::INT8 => Err(format!(
                    "{} is a float and the column is {}; round it first",
                    v,
                    ty.name()
                )
                .into()),
                _ => v.to_sql_checked(ty, out),
            },

            SqlParam::Text(v) => v.to_sql_checked(ty, out),
            SqlParam::Json(v) => v.to_sql_checked(ty, out),
        }
    }

    // Whether a value can be sent as this type is decided per value, in to_sql,
    // where the value is in hand. Claiming to accept everything and then
    // encoding regardless of the requested type is how a float ended up in a
    // numeric field.
    fn accepts(_ty: &PgType) -> bool {
        true
    }

    fn to_sql_checked(
        &self,
        ty: &PgType,
        out: &mut BytesMut,
    ) -> std::result::Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        self.to_sql(ty, out)
    }
}

fn py_to_json(py: Python, obj: &Bound<'_, PyAny>) -> PyResult<JsonValue> {
    if obj.is_none() {
        Ok(JsonValue::Null)
    } else if let Ok(b) = obj.extract::<bool>() {
        Ok(JsonValue::Bool(b))
    } else if let Ok(i) = obj.extract::<i64>() {
        Ok(JsonValue::Number(i.into()))
    } else if let Ok(f) = obj.extract::<f64>() {
        Ok(serde_json::Number::from_f64(f)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null))
    } else if let Ok(s) = obj.extract::<String>() {
        Ok(JsonValue::String(s))
    } else if let Ok(dict) = obj.downcast::<PyDict>() {
        let mut map = serde_json::Map::new();
        for (key, value) in dict.iter() {
            let key_str = key.extract::<String>()?;
            let json_value = py_to_json(py, &value)?;
            map.insert(key_str, json_value);
        }
        Ok(JsonValue::Object(map))
    } else if let Ok(list) = obj.downcast::<PyList>() {
        let json_list: Vec<JsonValue> = list
            .iter()
            .map(|item| py_to_json(py, &item))
            .collect::<PyResult<_>>()?;
        Ok(JsonValue::Array(json_list))
    } else {
        Err(PyValueError::new_err("Unsupported Python type"))
    }
}

fn json_to_py(py: Python, value: &JsonValue) -> PyObject {
    match value {
        JsonValue::Null => py.None(),
        JsonValue::Bool(b) => b.to_object(py),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_object(py)
            } else if let Some(f) = n.as_f64() {
                f.to_object(py)
            } else {
                py.None()
            }
        }
        JsonValue::String(s) => s.to_object(py),
        JsonValue::Array(arr) => {
            let list = PyList::new_bound(py, arr.iter().map(|v| json_to_py(py, v)));
            list.to_object(py)
        }
        JsonValue::Object(map) => {
            let dict = PyDict::new_bound(py);
            for (k, v) in map {
                dict.set_item(k, json_to_py(py, v)).unwrap();
            }
            dict.to_object(py)
        }
    }
}

fn py_dict_to_hashmap(
    py: Python,
    dict: &Bound<'_, PyDict>,
) -> PyResult<HashMap<String, JsonValue>> {
    let mut map = HashMap::new();
    for (key, value) in dict.iter() {
        let key_str = key.extract::<String>()?;
        let json_value = py_to_json(py, &value)?;
        map.insert(key_str, json_value);
    }
    Ok(map)
}

/// Python module definition
#[pymodule]
fn dbcenter(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<DBCenter>()?;
    m.add_class::<Collection>()?;
    m.add_class::<GeoCollection>()?;
    m.add_class::<VectorStore>()?;
    m.add_class::<LocalStorageAdapter>()?;
    m.add_class::<GoogleDriveAdapter>()?;
    m.add_class::<S3StorageAdapter>()?;
    m.add_class::<SyncEngine>()?;
    m.add_class::<LazyFrame>()?;

    #[pyfunction]
    fn init_logging() -> PyResult<()> {
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
        Ok(())
    }
    m.add_function(wrap_pyfunction!(init_logging, m)?)?;

    Ok(())
}

/// Python wrapper for LocalStorageAdapter
#[pyclass]
struct LocalStorageAdapter {
    inner: Arc<Box<dyn StorageAdapter + Send + Sync>>,
    path: String,
}

#[pymethods]
impl LocalStorageAdapter {
    #[new]
    fn new(path: String) -> Self {
        let adapter = CoreLocalStorageAdapter::new(PathBuf::from(&path));
        LocalStorageAdapter {
            inner: Arc::new(Box::new(adapter)),
            path,
        }
    }

    fn list(&self, prefix: &str) -> PyResult<Vec<String>> {
        self.inner
            .list(prefix)
            .map_err(|e| PyException::new_err(e.to_string()))
    }

    fn lock(&self, key: &str, ttl_ms: u64, worker_id: &str) -> PyResult<bool> {
        self.inner
            .lock(key, ttl_ms, worker_id)
            .map_err(|e| PyException::new_err(e.to_string()))
    }

    fn refresh_lock(&self, key: &str, ttl_ms: u64, worker_id: &str) -> PyResult<bool> {
        self.inner
            .refresh_lock(key, ttl_ms, worker_id)
            .map_err(|e| PyException::new_err(e.to_string()))
    }

    fn unlock(&self, key: &str, worker_id: &str) -> PyResult<()> {
        self.inner
            .unlock(key, worker_id)
            .map_err(|e| PyException::new_err(e.to_string()))
    }
}

/// Python wrapper for GoogleDriveAdapter
#[pyclass]
struct GoogleDriveAdapter {
    inner: Arc<Box<dyn StorageAdapter + Send + Sync>>,
    root_folder_name: String,
}

#[pymethods]
impl GoogleDriveAdapter {
    #[new]
    fn new(credential_path: String, root_folder_name: String) -> PyResult<Self> {
        let adapter = CoreGoogleDriveAdapter::new(&credential_path, &root_folder_name)
            .map_err(|e| PyException::new_err(e.to_string()))?;
        Ok(GoogleDriveAdapter {
            inner: Arc::new(Box::new(adapter)),
            root_folder_name,
        })
    }

    fn list(&self, prefix: &str) -> PyResult<Vec<String>> {
        self.inner
            .list(prefix)
            .map_err(|e| PyException::new_err(e.to_string()))
    }
}

/// Any S3-compatible bucket: AWS S3, Cloudflare R2, Tigris, MinIO.
///
/// Pass the keys, or leave both out to read the standard environment
/// variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`,
/// `AWS_ENDPOINT_URL_S3`). `prefix` keeps several databases apart in one
/// bucket.
#[pyclass]
struct S3StorageAdapter {
    inner: Arc<Box<dyn StorageAdapter + Send + Sync>>,
}

#[pymethods]
impl S3StorageAdapter {
    #[new]
    #[pyo3(signature = (bucket, endpoint=None, access_key=None, secret_key=None, region=None, prefix="", path_style=None, session_token=None))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        bucket: String,
        endpoint: Option<String>,
        access_key: Option<String>,
        secret_key: Option<String>,
        region: Option<String>,
        prefix: &str,
        path_style: Option<bool>,
        session_token: Option<String>,
    ) -> PyResult<Self> {
        let mut config = match (access_key, secret_key) {
            (Some(access_key), Some(secret_key)) => {
                let endpoint = endpoint
                    .clone()
                    .unwrap_or_else(|| "https://s3.amazonaws.com".to_string());
                S3Config {
                    bucket,
                    region: "auto".to_string(),
                    // A plain http:// endpoint is a local MinIO, and MinIO
                    // wants the bucket in the path.
                    path_style: endpoint.starts_with("http://"),
                    endpoint,
                    access_key,
                    secret_key,
                    session_token: None,
                }
            }
            (None, None) => S3Config::from_env(Some(&bucket))
                .map_err(|e| PyValueError::new_err(e.to_string()))?,
            _ => {
                return Err(PyValueError::new_err(
                    "pass both access_key and secret_key, or neither to read them \
                     from AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY",
                ))
            }
        };
        if let Some(endpoint) = endpoint {
            config.path_style = endpoint.starts_with("http://");
            config.endpoint = endpoint;
        }
        if let Some(region) = region {
            config.region = region;
        }
        if let Some(path_style) = path_style {
            config.path_style = path_style;
        }
        if session_token.is_some() {
            config.session_token = session_token;
        }
        let adapter = CoreS3StorageAdapter::with_prefix(config, prefix)
            .map_err(|e| PyException::new_err(e.to_string()))?;
        Ok(S3StorageAdapter {
            inner: Arc::new(Box::new(adapter)),
        })
    }

    fn list(&self, prefix: &str) -> PyResult<Vec<String>> {
        self.inner
            .list(prefix)
            .map_err(|e| PyException::new_err(e.to_string()))
    }
}

/// Python wrapper for SyncEngine
#[pyclass]
struct SyncEngine {
    inner: CoreSyncEngine,
}

#[pymethods]
impl SyncEngine {
    #[new]
    fn new(db: &DBCenter, storage: &Bound<'_, PyAny>) -> PyResult<Self> {
        let adapter: Arc<Box<dyn StorageAdapter + Send + Sync>> =
            if let Ok(local) = storage.extract::<PyRef<LocalStorageAdapter>>() {
                local.inner.clone()
            } else if let Ok(drive) = storage.extract::<PyRef<GoogleDriveAdapter>>() {
                drive.inner.clone()
            } else if let Ok(s3) = storage.extract::<PyRef<S3StorageAdapter>>() {
                s3.inner.clone()
            } else {
                return Err(PyValueError::new_err(
                    "storage must be LocalStorageAdapter, S3StorageAdapter or GoogleDriveAdapter",
                ));
            };

        Ok(SyncEngine {
            inner: CoreSyncEngine::new(db.inner.clone(), adapter),
        })
    }

    fn sync_table(&self, table_name: &str, limit: usize) -> PyResult<usize> {
        Python::with_gil(|py| {
            py.allow_threads(|| {
                self.inner
                    .sync_table(table_name, limit)
                    .map_err(|e| PyException::new_err(e.to_string()))
            })
        })
    }

    #[pyo3(signature = (table_name, partial=false))]
    fn pull_and_apply(&self, table_name: &str, partial: bool) -> PyResult<()> {
        let mode = if partial {
            dbcenter_core::sync_engine::SyncMode::Partial
        } else {
            dbcenter_core::sync_engine::SyncMode::Full
        };

        Python::with_gil(|py| {
            py.allow_threads(|| {
                self.inner
                    .pull_and_apply(table_name, mode)
                    .map_err(|e| PyException::new_err(e.to_string()))
            })
        })
    }

    fn vacuum(&self, table_name: &str) -> PyResult<()> {
        Python::with_gil(|py| {
            py.allow_threads(|| {
                self.inner
                    .vacuum(table_name)
                    .map_err(|e| PyException::new_err(e.to_string()))
            })
        })
    }

    /// Publish `<table>_columnar`: the table's Parquet on object storage, as an
    /// ordinary PostgreSQL view.
    ///
    /// Aggregates against it run about 45x faster than against the collection,
    /// because they read columns off object storage instead of rows out of
    /// PostgreSQL. Point lookups should still go to the collection, which is
    /// why this is a second view rather than a replacement.
    ///
    /// The view is as current as the last `sync_table`: it reads the Parquet
    /// on object storage, so rows written since then are in the collection and
    /// not in it. Aggregate over the collection when the answer has to include
    /// this second's writes.
    ///
    /// Needs the pg_duckdb extension: `db.install_extension("pg_duckdb")`.
    fn ensure_columnar_view(&self, table_name: &str) -> PyResult<()> {
        Python::with_gil(|py| {
            py.allow_threads(|| {
                self.inner
                    .ensure_columnar_view(table_name)
                    .map_err(|e| PyException::new_err(e.to_string()))
            })
        })
    }

    fn smart_query(&self, table_name: &str, id: &str) -> PyResult<Vec<String>> {
        Python::with_gil(|py| {
            py.allow_threads(|| {
                self.inner
                    .scan_smart(table_name, id)
                    .map(|rows| rows.into_iter().map(|v| v.to_string()).collect())
                    .map_err(|e| PyException::new_err(e.to_string()))
            })
        })
    }
}

/// Python wrapper for LazyFrame
#[pyclass]
struct LazyFrame {
    inner: Option<CoreLazyFrame>,
}

#[pymethods]
impl LazyFrame {
    fn filter(mut slf: PyRefMut<Self>, condition: String) -> PyResult<Self> {
        let inner = slf
            .inner
            .take()
            .ok_or_else(|| PyException::new_err("LazyFrame consumed"))?;
        Ok(LazyFrame {
            inner: Some(inner.filter(condition)),
        })
    }

    fn select(mut slf: PyRefMut<Self>, columns: Vec<String>) -> PyResult<Self> {
        let inner = slf
            .inner
            .take()
            .ok_or_else(|| PyException::new_err("LazyFrame consumed"))?;
        Ok(LazyFrame {
            inner: Some(inner.select(columns)),
        })
    }

    fn limit(mut slf: PyRefMut<Self>, n: usize) -> PyResult<Self> {
        let inner = slf
            .inner
            .take()
            .ok_or_else(|| PyException::new_err("LazyFrame consumed"))?;
        Ok(LazyFrame {
            inner: Some(inner.limit(n)),
        })
    }

    fn offset(mut slf: PyRefMut<Self>, n: usize) -> PyResult<Self> {
        let inner = slf
            .inner
            .take()
            .ok_or_else(|| PyException::new_err("LazyFrame consumed"))?;
        Ok(LazyFrame {
            inner: Some(inner.offset(n)),
        })
    }

    fn order_by(mut slf: PyRefMut<Self>, order: String) -> PyResult<Self> {
        let inner = slf
            .inner
            .take()
            .ok_or_else(|| PyException::new_err("LazyFrame consumed"))?;
        Ok(LazyFrame {
            inner: Some(inner.order_by(order)),
        })
    }

    fn collect(&self, py: Python<'_>) -> PyResult<Vec<PyObject>> {
        if let Some(inner) = &self.inner {
            let results = inner
                .collect()
                .map_err(|e| PyException::new_err(e.to_string()))?;
            Ok(results.iter().map(|v| json_to_py(py, v)).collect())
        } else {
            Err(PyException::new_err("LazyFrame consumed"))
        }
    }

    fn to_arrow(&self, py: Python<'_>) -> PyResult<PyObject> {
        if let Some(inner) = &self.inner {
            let ipc_bytes = inner
                .to_arrow_ipc()
                .map_err(|e| PyException::new_err(e.to_string()))?;
            let py_bytes = pyo3::types::PyBytes::new_bound(py, &ipc_bytes);

            // Import pyarrow.ipc and open stream
            let pa_ipc = py.import_bound("pyarrow.ipc")?;
            let reader = pa_ipc.call_method1("open_stream", (py_bytes,))?;
            let table = reader.call_method0("read_all")?;
            Ok(table.to_object(py))
        } else {
            Err(PyException::new_err("LazyFrame consumed"))
        }
    }

    fn to_polars(&self, py: Python<'_>) -> PyResult<PyObject> {
        // Return Arrow Table wrapped in Polars
        let arrow_table = self.to_arrow(py)?;
        let pl = py.import_bound("polars")?;
        let df = pl.call_method1("from_arrow", (arrow_table,))?;
        Ok(df.to_object(py))
    }

    fn to_pandas(&self, py: Python<'_>) -> PyResult<PyObject> {
        // Return Arrow Table -> to_pandas
        let arrow_table_obj = self.to_arrow(py)?;
        let arrow_table = arrow_table_obj.bind(py);
        let df = arrow_table.call_method0("to_pandas")?;
        Ok(df.to_object(py))
    }
}
