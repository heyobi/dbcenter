use crate::error::Result;
use std::path::PathBuf;
use std::fs;
use serde::{Deserialize, Serialize};

/// Lock file structure for concurrency control
#[derive(Serialize, Deserialize, Debug)]
pub struct LockFile {
    pub expires_at: u64, // Unix timestamp in milliseconds
    pub worker_id: String,
}

/// Trait for "Dumb Storage Adapter"
/// Supports uploading blobs (files) and listing/downloading.
/// Now supports locking for concurrency control.
pub trait StorageAdapter {
    fn upload(&self, key: &str, data: &[u8]) -> Result<()>;
    fn download(&self, key: &str) -> Result<Vec<u8>>;
    fn list(&self, prefix: &str) -> Result<Vec<String>>;
    fn download_range(&self, key: &str, start: u64, length: u64) -> Result<Vec<u8>>;
    fn delete(&self, key: &str) -> Result<()>;
    
    // Phase 4: Locking
    fn lock(&self, key: &str, ttl_ms: u64, worker_id: &str) -> Result<bool>;
    fn refresh_lock(&self, key: &str, ttl_ms: u64, worker_id: &str) -> Result<bool>;
    fn unlock(&self, key: &str, worker_id: &str) -> Result<()>;
    fn rename(&self, src: &str, dest: &str) -> Result<()>;
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

    fn download_range(&self, key: &str, start: u64, length: u64) -> Result<Vec<u8>> {
        use std::io::{Read, Seek};
        let path = self.root_path.join(key);
        let mut file = fs::File::open(&path).map_err(crate::error::DBCenterError::IoError)?;
        file.seek(std::io::SeekFrom::Start(start)).map_err(crate::error::DBCenterError::IoError)?;
        let mut buffer = vec![0u8; length as usize];
        file.read_exact(&mut buffer).map_err(crate::error::DBCenterError::IoError)?;
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
    
        // Check if lock exists and is valid
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(lock) = serde_json::from_str::<LockFile>(&content) {
                    if lock.expires_at > now {
                        // Locked by someone else (or us from before)
                         return Ok(false);
                    }
                    // Else: Expired. We can recover it (overwrite).
                    tracing::warn!("Recovering stale lock: {}", key);
                }
            }
        }

        // Write new lock (Create or Overwrite if expired)
        let lock = LockFile {
            expires_at: now + ttl_ms,
            worker_id: worker_id.to_string(),
        };
        let json = serde_json::to_string(&lock).map_err(crate::error::DBCenterError::SerializationError)?;
        
        // Ensure parent dir
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(crate::error::DBCenterError::IoError)?;
        }
        
        // Write atomic-ish (for local fs, direct write is fine for now, real systems use Exclusive Create)
        // Ideally: open with create_new=true. If fails, read check expiry.
        // Simplified for this task: Check above handles "Stale Lock Recovery".
        // A race condition exists here between "Check" and "Write" but for local test it's acceptable.
        // For distinct correctness:
        // 1. Try CreateNew. If success -> OK.
        // 2. If fail -> Read. If expired -> Overwrite. Else -> Fail.
        
        // Better implementation:
        let file_res = fs::OpenOptions::new().write(true).create_new(true).open(&path);
        if let Ok(_) = file_res {
            // Created successfully
             fs::write(&path, json).map_err(crate::error::DBCenterError::IoError)?;
             return Ok(true);
        } else {
            // Exists. Check expiry.
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(existing) = serde_json::from_str::<LockFile>(&content) {
                    if existing.expires_at > now {
                        return Ok(false); // Valid lock held
                    }
                }
            }
            // Expired or corrupt -> Overwrite
            fs::write(&path, json).map_err(crate::error::DBCenterError::IoError)?;
            return Ok(true);
        }
    }

    fn refresh_lock(&self, key: &str, ttl_ms: u64, worker_id: &str) -> Result<bool> {
        let path = self.root_path.join(key);
        if !path.exists() { return Ok(false); }

        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(lock) = serde_json::from_str::<LockFile>(&content) {
                if lock.worker_id == worker_id {
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
                    let new_lock = LockFile {
                        expires_at: now + ttl_ms,
                        worker_id: worker_id.to_string(),
                    };
                    let json = serde_json::to_string(&new_lock).map_err(crate::error::DBCenterError::SerializationError)?;
                    fs::write(&path, json).map_err(crate::error::DBCenterError::IoError)?;
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn unlock(&self, key: &str, worker_id: &str) -> Result<()> {
        let path = self.root_path.join(key);
        if !path.exists() { return Ok(()); }

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
}
