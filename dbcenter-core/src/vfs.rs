// use crate::error::Result;
use crate::storage::StorageAdapter;
use bytes::Bytes;
use parquet::file::reader::{ChunkReader, Length};
use std::io::Read;
use std::sync::Arc;

/// A remote file reader that implements Parquet's ChunkReader trait.
/// It uses the StorageAdapter to fetch ranges of data on demand.
/// This fulfills Task 3.4: "Implement a Rust VFS adapter for the x_base file".
pub struct RemoteParquetReader {
    storage: Arc<Box<dyn StorageAdapter + Send + Sync>>,
    key: String,
    length: u64,
}

impl RemoteParquetReader {
    pub fn new(
        storage: Arc<Box<dyn StorageAdapter + Send + Sync>>,
        key: String,
        length: u64,
    ) -> Self {
        Self {
            storage,
            key,
            length,
        }
    }
}

impl Length for RemoteParquetReader {
    fn len(&self) -> u64 {
        self.length
    }
}

impl ChunkReader for RemoteParquetReader {
    type T = Box<dyn Read + Send>;

    fn get_read(&self, start: u64) -> parquet::errors::Result<Self::T> {
        // Lazily, a block at a time. Fetching from the offset to the end of the
        // file means a caller after one page pays for the whole object.
        Ok(Box::new(crate::vfs::remote_file::StreamingRange::new(
            self.storage.clone(),
            self.key.clone(),
            start,
            self.length,
        )))
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<Bytes> {
        tracing::debug!(
            "RemoteParquetReader: getting bytes {} from offset {}",
            length,
            start
        );
        let data = self
            .storage
            .download_range(&self.key, start, length as u64)
            .map_err(|e| parquet::errors::ParquetError::External(Box::new(e)))?;

        Ok(Bytes::from(data))
    }
}

pub mod google_drive;
pub mod remote_file;
pub mod s3;
pub mod smart_reader;
