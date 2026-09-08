use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Lock file structure for concurrency control
#[derive(Serialize, Deserialize, Debug)]
pub struct LockFile {
    pub expires_at: u64, // Unix timestamp in milliseconds
    pub worker_id: String,
}

/// Trait for "Dumb Storage Adapter"
/// Supports uploading blobs (files) and listing/downloading.
/// Now supports locking for concurrency control.
/// What DuckDB needs to read this store itself.
///
/// The whole point of routing analytical reads through DuckDB is that the read
/// happens where the data is, instead of being pulled through DBCenter and
/// materialised into PostgreSQL first. So beyond bytes, an adapter has to be
/// able to say two things: the name DuckDB knows an object by, and how to
/// authenticate for it. An adapter that cannot - because it is backed by
/// something DuckDB has no filesystem for - returns `None` and the caller falls
/// back to reading through the adapter.
#[derive(Debug, Clone)]
pub struct DuckDbAccess {
    /// Prefix an object key hangs off, with no trailing slash. Either an
    /// `s3://bucket/prefix` URL or an absolute local directory.
    pub base_url: String,
    /// Absent for a local directory, which needs no credentials.
    pub credentials: Option<S3Credentials>,
}

impl DuckDbAccess {
    /// The URL DuckDB should be given for one object key.
    pub fn url_for(&self, key: &str) -> String {
        format!("{}/{}", self.base_url, key.trim_start_matches('/'))
    }
}

/// S3 credentials in the shape DuckDB's secret manager takes them.
#[derive(Debug, Clone)]
pub struct S3Credentials {
    pub key_id: String,
    pub secret: String,
    pub region: String,
    pub endpoint: String,
    pub session_token: Option<String>,
    /// `path` for MinIO and anything else that wants `endpoint/bucket/key`;
    /// `vhost` for the bucket-in-the-hostname form.
    pub url_style: &'static str,
    pub use_ssl: bool,
}

pub trait StorageAdapter {
    fn upload(&self, key: &str, data: &[u8]) -> Result<()>;
    fn download(&self, key: &str) -> Result<Vec<u8>>;
    fn list(&self, prefix: &str) -> Result<Vec<String>>;
    fn size(&self, key: &str) -> Result<u64>;
    fn download_range(&self, key: &str, start: u64, length: u64) -> Result<Vec<u8>>;
    fn delete(&self, key: &str) -> Result<()>;

    // Phase 4: Locking
    fn lock(&self, key: &str, ttl_ms: u64, worker_id: &str) -> Result<bool>;
    fn refresh_lock(&self, key: &str, ttl_ms: u64, worker_id: &str) -> Result<bool>;
    fn unlock(&self, key: &str, worker_id: &str) -> Result<()>;
    fn rename(&self, src: &str, dest: &str) -> Result<()>;

    /// How DuckDB can reach this store without going through the adapter.
    ///
    /// Defaults to `None`, which means "read it through me" - correct but slow,
    /// and the reason it is worth overriding.
    fn duckdb_access(&self) -> Option<DuckDbAccess> {
        None
    }
}

/// Wraps another adapter and counts the bytes it actually pulls down.
///
/// The point of reading Parquet over a range-capable store is that a query
/// touches a fraction of the file. That is a claim about bytes, so it needs to
/// be measured rather than asserted: `CountingStorage` is what lets a test say
/// "this query read 40 KB of a 12 MB object" instead of "it seemed fast".
pub struct CountingStorage {
    inner: Arc<Box<dyn StorageAdapter + Send + Sync>>,
    downloaded: Arc<AtomicU64>,
    requests: Arc<AtomicU64>,
}

impl CountingStorage {
    pub fn new(inner: Arc<Box<dyn StorageAdapter + Send + Sync>>) -> Self {
        Self {
            inner,
            downloaded: Arc::new(AtomicU64::new(0)),
            requests: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Bytes pulled down so far, across whole and ranged reads.
    pub fn bytes_downloaded(&self) -> u64 {
        self.downloaded.load(Ordering::Relaxed)
    }

    /// How many read requests were issued. A high count with few bytes means
    /// the reader is chatty, which costs latency even when it costs no traffic.
    pub fn read_requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    pub fn reset(&self) {
        self.downloaded.store(0, Ordering::Relaxed);
        self.requests.store(0, Ordering::Relaxed);
    }
}

impl StorageAdapter for CountingStorage {
    fn upload(&self, key: &str, data: &[u8]) -> Result<()> {
        self.inner.upload(key, data)
    }

    fn download(&self, key: &str) -> Result<Vec<u8>> {
        let data = self.inner.download(key)?;
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.downloaded
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        Ok(data)
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.inner.list(prefix)
    }

    fn size(&self, key: &str) -> Result<u64> {
        self.inner.size(key)
    }

    fn download_range(&self, key: &str, start: u64, length: u64) -> Result<Vec<u8>> {
        let data = self.inner.download_range(key, start, length)?;
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.downloaded
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        Ok(data)
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.inner.delete(key)
    }

    fn lock(&self, key: &str, ttl_ms: u64, worker_id: &str) -> Result<bool> {
        self.inner.lock(key, ttl_ms, worker_id)
    }

    fn refresh_lock(&self, key: &str, ttl_ms: u64, worker_id: &str) -> Result<bool> {
        self.inner.refresh_lock(key, ttl_ms, worker_id)
    }

    fn unlock(&self, key: &str, worker_id: &str) -> Result<()> {
        self.inner.unlock(key, worker_id)
    }

    fn rename(&self, src: &str, dest: &str) -> Result<()> {
        self.inner.rename(src, dest)
    }

    fn duckdb_access(&self) -> Option<DuckDbAccess> {
        // Deliberately none. This wrapper exists to count the bytes a read
        // pulls down, and a read DuckDB performs for itself goes nowhere near
        // the counter - reporting an access here would silently turn the
        // measurement into zero.
        None
    }
}

/// Local Filesystem Implementation of StorageAdapter
pub struct LocalStorageAdapter {
    root_path: PathBuf,
}

impl LocalStorageAdapter {
    pub fn new(path: PathBuf) -> Self {
        if !path.exists() {
            let _ = fs::create_dir_all(&path);
        }
        Self { root_path: path }
    }
}

impl StorageAdapter for LocalStorageAdapter {
    fn upload(&self, key: &str, data: &[u8]) -> Result<()> {
        let path = self.root_path.join(key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(crate::error::DBCenterError::IoError)?;
        }
        fs::write(&path, data).map_err(crate::error::DBCenterError::IoError)?;
        Ok(())
    }

    fn download(&self, key: &str) -> Result<Vec<u8>> {
        let path = self.root_path.join(key);
        fs::read(&path).map_err(crate::error::DBCenterError::IoError)
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let mut results = Vec::new();
        if self.root_path.exists() {
            for entry in walkdir::WalkDir::new(&self.root_path) {
                let entry = entry.map_err(|e| crate::error::DBCenterError::IoError(e.into()))?;
                if entry.file_type().is_file() {
                    let path = entry.path();
                    let relative = path.strip_prefix(&self.root_path).unwrap();
                    let key = relative.to_string_lossy().to_string().replace("\\", "/");
                    if key.starts_with(prefix) {
                        results.push(key);
                    }
                }
            }
        }
        Ok(results)
    }

    fn size(&self, key: &str) -> Result<u64> {
        let path = self.root_path.join(key);
        let metadata = std::fs::metadata(path)
            .map_err(|e| crate::error::DBCenterError::StorageError(e.to_string()))?;
        Ok(metadata.len())
    }

    fn download_range(&self, key: &str, start: u64, length: u64) -> Result<Vec<u8>> {
        use std::io::{Read, Seek};
        let path = self.root_path.join(key);
        let mut file = fs::File::open(&path).map_err(crate::error::DBCenterError::IoError)?;
        file.seek(std::io::SeekFrom::Start(start))
            .map_err(crate::error::DBCenterError::IoError)?;
        let mut buffer = vec![0u8; length as usize];
        file.read_exact(&mut buffer)
            .map_err(crate::error::DBCenterError::IoError)?;
        Ok(buffer)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let path = self.root_path.join(key);
        if path.exists() {
            fs::remove_file(path).map_err(crate::error::DBCenterError::IoError)?;
        }
        Ok(())
    }

    // --- Phase 4 Locking Implementation ---

    fn lock(&self, key: &str, ttl_ms: u64, worker_id: &str) -> Result<bool> {
        let path = self.root_path.join(key);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        // 1. Prepare new lock content
        let new_lock = LockFile {
            expires_at: now + ttl_ms,
            worker_id: worker_id.to_string(),
        };
        let json_content = serde_json::to_string(&new_lock)
            .map_err(crate::error::DBCenterError::SerializationError)?;

        // 2. Ensure parent structure
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(crate::error::DBCenterError::IoError)?;
        }

        // 3. Try to create atomically (Atomic Create)
        let open_result = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true) // Crucial: fails if exists
            .open(&path);

        match open_result {
            Ok(_) => {
                // created successfully, now write content
                // Note: file is open, but create_new doesn't write content.
                // We should technically use the file handle, but fs::write is easier and robust enough here since we own it.
                // Or better: write to the handle.
                fs::write(&path, json_content).map_err(crate::error::DBCenterError::IoError)?;
                tracing::debug!("Lock acquired (created new): {}", key);
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // File exists. Check validity/expiry.
                match fs::read_to_string(&path) {
                    Ok(content) => {
                        match serde_json::from_str::<LockFile>(&content) {
                            Ok(existing_lock) => {
                                if existing_lock.expires_at > now {
                                    // Valid lock exists.
                                    // logging::debug!("Lock collision: {} held by {} until {}", key, existing_lock.worker_id, existing_lock.expires_at);
                                    Ok(false)
                                } else {
                                    // Expired. Recover.
                                    tracing::warn!(
                                        "Recovering stale lock: {} (expired at {})",
                                        key,
                                        existing_lock.expires_at
                                    );
                                    // Overwrite
                                    fs::write(&path, json_content)
                                        .map_err(crate::error::DBCenterError::IoError)?;
                                    Ok(true)
                                }
                            }
                            Err(e) => {
                                // Corrupt JSON. Recover/Overwrite.
                                tracing::error!(
                                    "Lock file {} corrupted (json error: {}). Overwriting.",
                                    key,
                                    e
                                );
                                fs::write(&path, json_content)
                                    .map_err(crate::error::DBCenterError::IoError)?;
                                Ok(true)
                            }
                        }
                    }
                    Err(read_err) => {
                        // Could not read (maybe partial write or permission?).
                        // If we can't read it to verify, we assume it's valid to be safe?
                        // Or we assume it's broken?
                        // "Dumb Storage" philosophy: If we can't verify it, we back off?
                        // But if it's a transient IO error, backing off is good.
                        tracing::error!("Failed to read existing lock file {}: {}", key, read_err);
                        Ok(false)
                    }
                }
            }
            Err(e) => {
                // Other IO error
                tracing::error!("IO Error trying to create lock {}: {}", key, e);
                Err(crate::error::DBCenterError::IoError(e))
            }
        }
    }

    fn refresh_lock(&self, key: &str, ttl_ms: u64, worker_id: &str) -> Result<bool> {
        let path = self.root_path.join(key);
        if !path.exists() {
            return Ok(false);
        }

        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(lock) = serde_json::from_str::<LockFile>(&content) {
                if lock.worker_id == worker_id {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;
                    let new_lock = LockFile {
                        expires_at: now + ttl_ms,
                        worker_id: worker_id.to_string(),
                    };
                    let json = serde_json::to_string(&new_lock)
                        .map_err(crate::error::DBCenterError::SerializationError)?;
                    fs::write(&path, json).map_err(crate::error::DBCenterError::IoError)?;
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn unlock(&self, key: &str, worker_id: &str) -> Result<()> {
        let path = self.root_path.join(key);
        if !path.exists() {
            return Ok(());
        }

        // Only delete if we own it
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(lock) = serde_json::from_str::<LockFile>(&content) {
                if lock.worker_id == worker_id {
                    fs::remove_file(&path).map_err(crate::error::DBCenterError::IoError)?;
                }
            }
        }
        Ok(())
    }

    fn rename(&self, src: &str, dest: &str) -> Result<()> {
        let src_path = self.root_path.join(src);
        let dest_path = self.root_path.join(dest);
        fs::rename(src_path, dest_path).map_err(crate::error::DBCenterError::IoError)?;
        Ok(())
    }

    fn duckdb_access(&self) -> Option<DuckDbAccess> {
        // DuckDB opens local paths directly, so there is nothing to authenticate.
        Some(DuckDbAccess {
            base_url: self
                .root_path
                .to_string_lossy()
                .trim_end_matches('/')
                .to_string(),
            credentials: None,
        })
    }
}
