use dbcenter_core::DBCenter;
use libc::{c_char, c_void};
use std::ffi::{CStr, CString};
use std::sync::Arc;
use std::ptr;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::fs;
use serde_json;

#[cfg(target_os = "windows")]
const WATCHER_BASENAME: &str = "dbcenter-watcher.exe";
#[cfg(not(target_os = "windows"))]
const WATCHER_BASENAME: &str = "dbcenter-watcher";

const WATCHER_BYTES: &[u8] = include_bytes!(env!("DBCENTER_WATCHER_EMBED"));
static EMBEDDED_WATCHER_PATH: OnceLock<PathBuf> = OnceLock::new();

// Opaque pointer types for C
#[repr(C)]
pub struct CDBCenter {
    inner: Arc<DBCenter>,
    watcher: Option<std::process::Child>, // Watcher process handle
}

#[repr(C)]
pub struct CDBCenterResult {
    ptr: *mut c_void,   // Pointer to result (e.g., CDBCenter*) or NULL
    error: *mut c_char, // Error message or NULL
}

/// Helper to convert C strings to Rust strings
unsafe fn c_str_to_str<'a>(ptr: *const c_char) -> Result<&'a str, String> {
    if ptr.is_null() {
        return Err("Null pointer".to_string());
    }
    CStr::from_ptr(ptr)
        .to_str()
        .map_err(|e| format!("Invalid UTF-8: {}", e))
}

/// Helper to convert C strings to Rust Option<String>
unsafe fn c_str_to_opt_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        None
    } else {
        Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
    }
}

/// Ensure embedded watcher binary is extracted and return its path
fn ensure_embedded_watcher_path() -> Result<PathBuf, String> {
    if let Some(path) = EMBEDDED_WATCHER_PATH.get() {
        return Ok(path.clone());
    }

    let mut base_dir = std::env::temp_dir();
    base_dir.push("dbcenter-ffi");
    fs::create_dir_all(&base_dir).map_err(|e| format!("Failed to create temp dir: {}", e))?;

    let watcher_path = base_dir.join(WATCHER_BASENAME);
    if !watcher_path.exists() {
        fs::write(&watcher_path, WATCHER_BYTES)
            .map_err(|e| format!("Failed to write watcher binary: {}", e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o755);
            fs::set_permissions(&watcher_path, perms)
                .map_err(|e| format!("Failed to set watcher permissions: {}", e))?;
        }
    }

    let _ = EMBEDDED_WATCHER_PATH.set(watcher_path.clone());
    Ok(watcher_path)
}

fn resolve_watcher_path() -> Result<std::path::PathBuf, String> {
    // 1. Check environment variable first
    if let Ok(path) = std::env::var("DBCENTER_WATCHER_PATH") {
        let pb = std::path::PathBuf::from(&path);
        if pb.exists() { 
            eprintln!("[DBCenter-FFI] Found watcher via env var: {:?}", pb);
            return Ok(pb); 
        }
    }

    // 2. Check next to the current executable (where .dll/.so is)
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            let candidate = parent.join(WATCHER_BASENAME);
            if candidate.exists() {
                eprintln!("[DBCenter-FFI] Found watcher next to executable: {:?}", candidate);
                return Ok(candidate);
            }
        }
    }

    // 3. Check current working directory
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join(WATCHER_BASENAME);
        if candidate.exists() {
            eprintln!("[DBCenter-FFI] Found watcher in cwd: {:?}", candidate);
            return Ok(candidate);
        }
    }

    // 4. Extract embedded watcher as fallback
    ensure_embedded_watcher_path()
}

fn spawn_watcher(
    data_dir: &std::path::Path,
    persist: bool,
    service: bool,
    instance_id: &str,
    postgres_pid: Option<u32>
) -> Result<std::process::Child, String> {
    
    let watcher_path = resolve_watcher_path()?;
    
    let mut cmd = std::process::Command::new(&watcher_path);
    cmd.stdin(std::process::Stdio::piped())
       .stdout(std::process::Stdio::null());
       
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW
        cmd.creation_flags(0x08000000);
        cmd.stderr(std::process::Stdio::inherit()); 
    }
    #[cfg(not(windows))]
    {
        cmd.stderr(std::process::Stdio::null()); // Silence stderr too unless debug
    }

    let mut child = cmd.spawn().map_err(|e| format!("Failed to spawn watcher at {:?}: {}", watcher_path, e))?;
    
    let config_json = serde_json::json!({
        "data_dir": data_dir,
        "persist": persist,
        "service": service,
        "postgres_pid": postgres_pid,
        "instance_id": instance_id,
    });
    
    if let Some(mut stdin) = child.stdin.take() {
        writeln!(stdin, "{}", config_json.to_string()).map_err(|e| e.to_string())?;
        stdin.flush().map_err(|e| e.to_string())?;
        // Restore stdin to keep pipe open
        child.stdin = Some(stdin); 
    }
    
    Ok(child)
}

/// Create a new DBCenter instance.
/// 
/// # Arguments
/// * `path` - Path to database directory (nullable for default)
/// * `persist` - 1 for persistent, 0 for volatile
/// 
/// # Returns
/// CDBCenterResult with ptr to CDBCenter instance or error message.
#[no_mangle]
pub extern "C" fn dbcenter_new(
    path: *const c_char,
    persist: u8,
) -> CDBCenterResult {
    let path_str = unsafe { c_str_to_opt_string(path) };
    let is_persist = persist != 0;
    let is_service = false; // FFI currently only supports standard mode

    let result = DBCenter::new(
            path_str,
            is_persist,
            is_service, 
            None, None, None, None // other options
        );

    match result {
        Ok(db) => {
            // Spawn watcher
            let watcher_child = match spawn_watcher(
                db.get_path(),
                is_persist,
                is_service,
                db.instance_id(),
                db.postgres_pid()
            ) {
                Ok(child) => Some(child),
                Err(e) => {
                     // Log error but continue? Or fail?
                     // Let's fail safety first
                     return CDBCenterResult { ptr: ptr::null_mut(), error: CString::new(e).unwrap().into_raw() };
                }
            };

            let container = Box::new(CDBCenter {
                inner: db,
                watcher: watcher_child,
            });
            CDBCenterResult {
                ptr: Box::into_raw(container) as *mut c_void,
                error: ptr::null_mut(),
            }
        }
        Err(e) => {
             CDBCenterResult {
                ptr: ptr::null_mut(),
                error: CString::new(e.to_string()).unwrap().into_raw(),
            }
        }
    }
}

/// Free the DBCenter instance.
#[no_mangle]
pub extern "C" fn dbcenter_free(ptr: *mut CDBCenter) {
    if !ptr.is_null() {
        unsafe {
            // Dropping CDBCenter will drop Child, closing stdin, triggering watcher cleanup
            let _ = Box::from_raw(ptr);
        }
    }
}

/// Free a string returned by library.
#[no_mangle]
pub extern "C" fn dbcenter_free_string(s: *mut c_char) {
    if !s.is_null() {
        unsafe {
            let _ = CString::from_raw(s);
        }
    }
}

/// Shutdown the DBCenter instance explicitly.
#[no_mangle]
pub extern "C" fn dbcenter_shutdown(ptr: *mut CDBCenter) -> *mut c_char {
    if ptr.is_null() {
        return CString::new("Null pointer").unwrap().into_raw();
    }
    
    let db = unsafe { &mut (*ptr) };
    
    // Explicit shutdown: notify watcher "SUCCESS" to stop it gracefully without wiping if persist=true?
    // Actually, watcher logic: if stdin closes (EOF) or reads "SUCCESS", it cleans up.
    // If we call inner.shutdown(), we do cleanup ourselves.
    // We should coordinate with watcher.
    // If we want to manually shutdown, we should tell watcher to exit.
    
    // Send "SUCCESS" to watcher
    if let Some(mut child) = db.watcher.take() {
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = writeln!(stdin, "SUCCESS");
            let _ = stdin.flush();
        }
        let _ = child.wait();
    }
    
    match db.inner.shutdown() {
        Ok(_) => ptr::null_mut(),
        Err(e) => CString::new(e.to_string()).unwrap().into_raw()
    }
}

/// Install an extension (e.g., "pgvector").
#[no_mangle]
pub extern "C" fn dbcenter_install_extension(
    ptr: *mut CDBCenter,
    name: *const c_char
) -> *mut c_char {
    if ptr.is_null() {
        return CString::new("Null pointer").unwrap().into_raw();
    }
    
    let db = unsafe { &(*ptr) };
    let name_str = match unsafe { c_str_to_str(name) } {
        Ok(s) => s,
        Err(e) => return CString::new(e).unwrap().into_raw(),
    };

    match db.inner.install_extension(name_str) {
        Ok(_) => ptr::null_mut(),
        Err(e) => CString::new(e.to_string()).unwrap().into_raw()
    }
}

// Opaque VectorStore pointer
pub struct CVectorStore {
    // VectorStore holds a reference to DBCenter. 
    // This is tricky in C because of lifetimes.
    // For FFI, we often treat VectorStore as "owning" or check validity at runtime,
    // or simply unsafe semantics that C user must keep DB alive.
    // However, VectorStore<'a> needs a reference.
    // Solution: CVectorStore will hold `DBCenter` arc and recreate VectorStore on fly OR
    // we change VectorStore to use Arc<DBCenter> internally in Core (refactor) OR
    // we use unsafe transmute to extend lifetime, putting onus on C user.
    // safest: store Arc<DBCenter> and params, creates VectorStore ephemeral? No, heavyweight.
    
    // Actually, Core `VectorStore` holds `&'a DBCenter`. 
    // For FFI, we can box `VectorStore<'static>` by cheating lifetimes if we ensure
    // DB outlives Store.
    // A simpler way for FFI wrapper: CDBCenter owns the DB. CVectorStore can hold a 
    // raw pointer to CDBCenter and we ensure via documentation.
    store: dbcenter_core::vector_store::VectorStore<'static>,
    // We keep a strong reference to parent to prevent deallocation while store exists?
    _parent: Arc<DBCenter>, 
    dimension: usize,
}

#[repr(C)]
pub struct CVectorStoreResult {
    ptr: *mut c_void,   
    error: *mut c_char, 
}

#[repr(C)]
pub struct CQueryResult {
    ids: *mut *mut c_char,       // Array of strings
    num_results: usize,
    distances: *mut f64,         // Array of doubles
    // skipping metadata for basic C API for now or returning complex JSON string
    metadatas_json: *mut *mut c_char, 
    error: *mut c_char,
}

/// Get a VectorStore instance.
#[no_mangle]
pub extern "C" fn dbcenter_get_vector_store(
    ptr: *mut CDBCenter,
    name: *const c_char,
    dimension: usize
) -> CVectorStoreResult {
    if ptr.is_null() {
         return CVectorStoreResult { ptr: ptr::null_mut(), error: CString::new("Null pointer").unwrap().into_raw() };
    }
    
    let db = unsafe { &(*ptr) };
    let name_str = match unsafe { c_str_to_str(name) } {
        Ok(s) => s,
        Err(e) => return CVectorStoreResult { ptr: ptr::null_mut(), error: CString::new(e).unwrap().into_raw() },
    };

    // We must extend the lifetime. This IS unsafe. The C user must not free CDBCenter
    // while using CVectorStore.
    let db_ref: &'static DBCenter = unsafe { std::mem::transmute(db.inner.as_ref()) };

    match db_ref.vector_store(name_str, dimension) {
        Ok(store) => {
            let container = Box::new(CVectorStore {
                store,
                _parent: db.inner.clone(), // Keep DB alive at least
                dimension,
            });
            CVectorStoreResult { ptr: Box::into_raw(container) as *mut c_void, error: ptr::null_mut() }
        }
        Err(e) => CVectorStoreResult { ptr: ptr::null_mut(), error: CString::new(e.to_string()).unwrap().into_raw() }
    }
}

/// Free VectorStore
#[no_mangle]
pub extern "C" fn dbcenter_vector_store_free(ptr: *mut CVectorStore) {
    if !ptr.is_null() {
        unsafe { let _ = Box::from_raw(ptr); }
    }
}

/// Add vector to store.
/// ids: char** (array of strings)
/// embeddings: float* (flat array of floats, size = count * dimension)
/// count: number of items
#[no_mangle]
pub extern "C" fn dbcenter_vector_store_add(
    ptr: *mut CVectorStore,
    ids: *const *const c_char,
    embeddings: *const f32, // Flat array
    count: usize
) -> *mut c_char {
    if ptr.is_null() { return CString::new("Null pointer").unwrap().into_raw(); }
    let container = unsafe { &(*ptr) };
    
    // Convert IDs
    let mut id_vec = Vec::with_capacity(count);
    for i in 0..count {
        let c_str = unsafe { *ids.add(i) };
        match unsafe { c_str_to_str(c_str) } {
            Ok(s) => id_vec.push(s.to_string()),
            Err(e) => return CString::new(e).unwrap().into_raw(),
        }
    }

    // Convert Embeddings (copy from flat array)
    let dim = container.dimension; // Need to expose dimension in core or save it? 
    // Core VectorStore doesn't expose dimension as pub field? checking outline... 
    // It is not pub. We passed it in, we should save it in CVectorStore wrapper struct.
    // Wait, let's peek standard implementation - usually dimension is immutable.
    // We can assume the passed dimension in creation matches? 
    // Actually, we can read the raw pointer with stride.
    
    let mut embed_vec = Vec::with_capacity(count);
    for i in 0..count {
        let start = i * dim;
        let slice = unsafe { std::slice::from_raw_parts(embeddings.add(start), dim) };
        embed_vec.push(slice.to_vec());
    }

    // Call core ADD
    // Note: Core 'add' is async or sync? Outline said "pub fn add(...)" returning Result.
    // Let's verify if DBCenter functions are async.
    // Looking at lib.rs: `pub fn collection` returns Collection. `VectorStore::new` is Sync.
    // Core uses `tokio-postgres` which IS async, effectively hidden behind `block_on` in Core methods?
    // Wait, Core `VectorStore::add` gets a client. `get_client` returns `MutexGuard<Client>`. 
    // Are the calls on Client async? `client.query`... in tokio-postgres, it is async.
    // But `postgres` crate is synchronous wrapper? 
    // The dependencies list "tokio-postgres" AND "postgres".
    // If Core uses `postgres` (sync) crate, then no runtime needed.
    // If Core uses `tokio-postgres`, we need runtime.
    // Outline implies `db.get_client()` returns a client.
    // Let's assume for FFI we might need blocking.
    // Checking previous steps: dependencies include `postgres` AND `tokio-postgres`.
    // The FFI `new` function creates a Runtime. This implies Core IS async (DBCenter::new is async in snippet `runtime.block_on`).
    // If Core methods are sync wrappers around async (using internal runtime? no, usually bad practice).
    // Or if Core exposes ASYNC methods.
    // Let's re-verify Core API for async-ness.
    
    // Assuming sync for now or we wrap in runtime entry.
    // Wait, `dbcenter_new` creates a runtime. But it drops it? 
    // No, `CDBCenter` stores the `Runtime`. So we can use it!
    
    // We need access to the runtime used by the DB.
    // CVectorStore should access `container._parent` but that's just the DB Arc.
    // We need the runtime from CDBCenter? 
    // Or does DBCenter manage its own threadpool? 
    // The FFI `CDBCenter` struct holds the `Runtime`.
    // But `CVectorStore` holds the `VectorStore` struct.
    // `VectorStore` holds `&DBCenter`.
    
    // Problem: `CVectorStore` doesn't have access to the `Runtime` stored in `CDBCenter`.
    // We should pass the runtime ref to CVectorStore or make Runtime global/static?
    // Making runtime static is easiest for FFI.
    
    // For this step, I'll attempt to call `add`. If it's async, I need `block_on`.
    // If I don't have the runtime handle, I can't `block_on`.
    // I will modify `dbcenter_new` to use a helper or global runtime if needed?
    // Or I'll wrap `store.add()` in a temporary `Runtime::new().unwrap().block_on()`?
    // Creating runtimes per call is expensive but safe for FFI simple calls.
    // Or, since `CDBCenter` holds runtime, maybe standard calls go through CDBCenter?
    
    // Assuming `vector_store.add` is synchronous (blocking) based on signature `-> Result<()>`.
    // If it were async it would be `async fn` or return `Future`.
    // The outline showed `pub fn add(...) -> Result<()>`.
    // This implies it IS blocking (synchronous).
    // So I can just call it!
    
    match container.store.add(&id_vec, &embed_vec, None) {
        Ok(_) => ptr::null_mut(),
        Err(e) => CString::new(e.to_string()).unwrap().into_raw()
    }
}

/// Query vector store.
/// query_embeddings: flat float array (usually just 1 vector)
#[no_mangle]
pub extern "C" fn dbcenter_vector_store_query(
    ptr: *mut CVectorStore,
    query_embeddings: *const f32, // Flat array
    n_results: i64
) -> CQueryResult {
    if ptr.is_null() {
        return CQueryResult {
             ids: ptr::null_mut(), num_results: 0, distances: ptr::null_mut(), metadatas_json: ptr::null_mut(),
             error: CString::new("Null pointer").unwrap().into_raw() 
        };
    }
    
    let container = unsafe { &(*ptr) };
    let dim = container.dimension;
    
    // Support 1 query vector for simplicity in C API v1
    let slice = unsafe { std::slice::from_raw_parts(query_embeddings, dim) };
    let query_vecs = vec![slice.to_vec()];

    match container.store.query(&query_vecs, n_results, None) {
        Ok(results) => {
            let count = results.len();
            
            // Allocate C arrays (pointers to strings, doubles, etc.)
            let mut ids_ptrs = Vec::with_capacity(count);
            let mut dists = Vec::with_capacity(count);
            let mut meta_ptrs = Vec::with_capacity(count);

            for res in results {
                let id_str = res["id"].as_str().unwrap_or("").to_string();
                let dist = res["distance"].as_f64().unwrap_or(0.0);
                let meta = res["metadata"].to_string(); 

                ids_ptrs.push(CString::new(id_str).unwrap().into_raw());
                dists.push(dist);
                meta_ptrs.push(CString::new(meta).unwrap().into_raw());
            }
            
            // Leak vectors to C
            let ids_array = ids_ptrs.into_boxed_slice();
            let ids_final = ids_array.as_ptr() as *mut *mut c_char;
            std::mem::forget(ids_array); 

            let dists_array = dists.into_boxed_slice();
            let dists_final = dists_array.as_ptr() as *mut f64;
            std::mem::forget(dists_array);

            let meta_array = meta_ptrs.into_boxed_slice();
            let meta_final = meta_array.as_ptr() as *mut *mut c_char;
            std::mem::forget(meta_array);

            CQueryResult {
                ids: ids_final,
                num_results: count,
                distances: dists_final,
                metadatas_json: meta_final,
                error: ptr::null_mut()
            }
        }
        Err(e) => {
             CQueryResult {
                ids: ptr::null_mut(), num_results: 0, distances: ptr::null_mut(), metadatas_json: ptr::null_mut(),
                error: CString::new(e.to_string()).unwrap().into_raw()
            }
        }
    }
}

/// Free QueryResult
#[no_mangle]
pub extern "C" fn dbcenter_free_query_result(res: CQueryResult) {
    if !res.error.is_null() {
        unsafe { let _ = CString::from_raw(res.error); }
    }
    
    if res.num_results > 0 {
        unsafe {
            if !res.ids.is_null() {
                let slice = std::slice::from_raw_parts_mut(res.ids, res.num_results);
                for &mut s in slice {
                    let _ = CString::from_raw(s);
                }
                let _ = Box::from_raw(res.ids); 
            }
            
            if !res.metadatas_json.is_null() {
                let slice = std::slice::from_raw_parts_mut(res.metadatas_json, res.num_results);
                for &mut s in slice {
                    let _ = CString::from_raw(s);
                }
                 let _ = Box::from_raw(res.metadatas_json);
            }
            
            if !res.distances.is_null() {
                let _ = Box::from_raw(res.distances);
            }
        }
    }
}

/// Force remove a database directory.
#[no_mangle]
pub extern "C" fn dbcenter_force_remove(path: *const c_char) -> *mut c_char {
    let path_str = match unsafe { c_str_to_str(path) } {
        Ok(s) => s,
        Err(e) => return CString::new(e).unwrap().into_raw(),
    };

    match DBCenter::force_remove(path_str) {
        Ok(_) => ptr::null_mut(),
        Err(e) => CString::new(e.to_string()).unwrap().into_raw(),
    }
}

// Opaque Collection pointer
pub struct CCollection {
    inner: dbcenter_core::collection::Collection<'static>,
    _parent: Arc<DBCenter>,
}

#[repr(C)]
pub struct CCollectionResult {
    ptr: *mut c_void,   
    error: *mut c_char, 
}

/// Get a Collection instance.
#[no_mangle]
pub extern "C" fn dbcenter_get_collection(
    ptr: *mut CDBCenter,
    name: *const c_char
) -> CCollectionResult {
    if ptr.is_null() {
         return CCollectionResult { ptr: ptr::null_mut(), error: CString::new("Null pointer").unwrap().into_raw() };
    }
    
    let db = unsafe { &(*ptr) };
    let name_str = match unsafe { c_str_to_str(name) } {
        Ok(s) => s,
        Err(e) => return CCollectionResult { ptr: ptr::null_mut(), error: CString::new(e).unwrap().into_raw() },
    };

    // We must extend the lifetime. This IS unsafe. The C user must not free CDBCenter
    // while using CCollection unless we keep a ref. We keep a ref in _parent.
    let db_ref: &'static DBCenter = unsafe { std::mem::transmute(db.inner.as_ref()) };
    
    let collection = db_ref.collection(name_str);
    
    let container = Box::new(CCollection {
        inner: collection,
        _parent: db.inner.clone(),
    });

    CCollectionResult { 
        ptr: Box::into_raw(container) as *mut c_void, 
        error: ptr::null_mut() 
    }
}


/// Free Collection
#[no_mangle]
pub extern "C" fn dbcenter_collection_free(ptr: *mut CCollection) {
    if !ptr.is_null() {
        unsafe { let _ = Box::from_raw(ptr); }
    }
}

/// Insert a document (JSON string).
#[no_mangle]
pub extern "C" fn dbcenter_collection_insert_one(
    ptr: *mut CCollection,
    json_doc: *const c_char
) -> *mut c_char {
    if ptr.is_null() { return CString::new("Null pointer").unwrap().into_raw(); }
    let coll = unsafe { &(*ptr) };
    
    let json_str = match unsafe { c_str_to_str(json_doc) } {
        Ok(s) => s,
        Err(e) => return CString::new(e).unwrap().into_raw(),
    };

    let value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => return CString::new(format!("Invalid JSON: {}", e)).unwrap().into_raw(),
    };

    match coll.inner.insert_one(&value) {
        Ok(_) => ptr::null_mut(), // Success, returns ID usually but for simplicity C API returns NULL on success
        Err(e) => CString::new(e.to_string()).unwrap().into_raw()
    }
}
#[repr(C)]
pub struct CJsonResult {
    json: *mut c_char,
    error: *mut c_char,
}

/// Execute a raw SQL query and return results as JSON.
#[no_mangle]
pub extern "C" fn dbcenter_sql_query(
    ptr: *mut CDBCenter,
    sql: *const c_char,
) -> CJsonResult {
    if ptr.is_null() {
         return CJsonResult { json: ptr::null_mut(), error: CString::new("Null pointer").unwrap().into_raw() };
    }
    
    let db = unsafe { &(*ptr) };
    let sql_str = match unsafe { c_str_to_str(sql) } {
        Ok(s) => s,
        Err(e) => return CJsonResult { json: ptr::null_mut(), error: CString::new(e).unwrap().into_raw() },
    };

    match db.inner.query(sql_str, &[]) {
        Ok(rows) => {
             // rows is Vec<serde_json::Value>
             match serde_json::to_string(&rows) {
                 Ok(json_str) => CJsonResult { 
                    json: CString::new(json_str).unwrap().into_raw(), 
                    error: ptr::null_mut() 
                 },
                 Err(e) => CJsonResult { 
                    json: ptr::null_mut(), 
                    error: CString::new(format!("Serialization error: {}", e)).unwrap().into_raw() 
                 }
             }
        },
        Err(e) => CJsonResult { json: ptr::null_mut(), error: CString::new(e.to_string()).unwrap().into_raw() },
    }
}
