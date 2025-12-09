use dbcenter_core::DBCenter as CoreDBCenter;
use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyDict, PyDictMethods, PyList, PyListMethods, PyModule};
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
    #[new]
    #[pyo3(signature = (path=None, persist=true, service=false, host=None, port=None, password=None, postgresql=None))]
    fn new(
        py: Python,
        path: Option<String>,
        persist: bool,
        service: bool,
        host: Option<String>,
        port: Option<u16>,
        password: Option<String>,
        postgresql: Option<&Bound<'_, PyAny>>,
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

        let db = CoreDBCenter::new(
            path.as_deref(),
            persist,
            service,
            host,
            port,
            password,
            pg_version,
        )
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
        let store = self.inner.vector_store(name, dimension)
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
    ///     params: Query parameters (optional)
    ///
    /// Returns:
    ///     List of results
    fn query(&self, sql: &str, _params: Option<Vec<PyObject>>) -> PyResult<Vec<PyObject>> {
        Python::with_gil(|py| {
            // For simplicity, we'll support no-param queries first
            let results = self
                .inner
                .query(sql, &[])
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
    #[getter]
    fn port(&self) -> PyResult<u16> {
        // Query the database for its port
        Python::with_gil(|py| {
            let results = self
                .inner
                .query("SELECT inet_server_port() as port", &[])
                .map_err(|e| PyException::new_err(e.to_string()))?;

            if let Some(row) = results.first() {
                if let Some(port_value) = row.get("port") {
                    if let Some(port) = port_value.as_i64() {
                        return Ok(port as u16);
                    }
                }
            }

            Err(PyException::new_err("Could not get port information"))
        })
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
        if explicit_path.exists() {
            return Ok(explicit_path);
        }
        attempted.push(explicit_path.display().to_string());
    }

    if let Ok(module) = PyModule::import_bound(py, "dbcenter") {
        if let Ok(file_attr) = module.getattr("__file__") {
            if let Ok(path_str) = file_attr.extract::<String>() {
                if let Some(parent) = Path::new(&path_str).parent() {
                    let candidate = parent.join(WATCHER_BASENAME);
                    if candidate.exists() {
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

    for candidate in dev_candidates {
        if candidate.exists() {
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
    fn insert_one(&self, document: &Bound<'_, PyDict>) -> PyResult<i32> {
        Python::with_gil(|py| {
            let json_doc = py_to_json(py, document.as_any())?;

            let coll = self.db.collection(&self.name);
            coll.insert_one(&json_doc)
                .map_err(|e| PyException::new_err(e.to_string()))
        })
    }

    /// Insert many documents
    fn insert_many(&self, documents: &Bound<'_, PyList>) -> PyResult<Vec<i32>> {
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
    fn fuzzy_search(&self, query: &str, field: &str, mode: Option<&str>, threshold: Option<f32>) -> PyResult<Vec<PyObject>> {
        Python::with_gil(|py| {
            let coll = self.db.collection(&self.name);
            let results = coll.fuzzy_search(query, field, mode, threshold)
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

    /// String representation
    fn __repr__(&self) -> String {
        format!("<Collection '{}'>", self.name)
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
    fn insert_many(&self, documents: &Bound<'_, PyList>) -> PyResult<Vec<i32>> {
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
        metadatas: Option<Vec<HashMap<String, PyObject>>>
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

            let store = self.db.vector_store(&self.name, self.dimension)
                .map_err(|e| PyException::new_err(e.to_string()))?;
            store.add(&ids, &embeddings, json_metas.as_deref())
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
        r#where: Option<HashMap<String, PyObject>>
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

            let store = self.db.vector_store(&self.name, self.dimension)
                .map_err(|e| PyException::new_err(e.to_string()))?;
            // Pass the vector list directly
            let results = store.query(&query_embeddings, n_results, filter.as_ref())
                 .map_err(|e| PyException::new_err(e.to_string()))?;

            let py_results: Vec<PyObject> = results.iter().map(|v| json_to_py(py, v)).collect();
             Ok(py_results)
        })
    }
}

// ============================================================================
// Helper Functions: Python <-> JSON conversion
// ============================================================================

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
    Ok(())
}
