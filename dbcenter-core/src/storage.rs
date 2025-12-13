use crate::error::Result;
use std::path::PathBuf;
use std::fs;

/// Trait for "Dumb Storage Adapter"
/// Supports uploading blobs (files) and listing/downloading.
pub trait StorageAdapter {
    fn upload(&self, key: &str, data: &[u8]) -> Result<()>;
    fn download(&self, key: &str) -> Result<Vec<u8>>;
    fn list(&self, prefix: &str) -> Result<Vec<String>>;
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
        // Simple recursive list or just flat list if simple. 
        // For this task, we assume keys are flat filenames or simple paths.
        // We walk dir.
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
}
