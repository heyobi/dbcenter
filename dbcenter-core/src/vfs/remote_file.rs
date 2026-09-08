use crate::storage::StorageAdapter;
use bytes::Bytes;
use lru::LruCache;
use parquet::file::reader::{ChunkReader, Length};
use std::io::Read;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// How much is pulled per request when streaming a range.
///
/// Small enough that a reader which stops early has not paid for the rest of
/// the file; large enough that sequential reading is not one request per page.
const STREAM_BLOCK: u64 = 512 * 1024;

/// Streams part of a remote object, fetching a block at a time.
///
/// `ChunkReader::get_read` is handed an offset and no length: the caller reads
/// until it has what it wants and then stops. Serving that by downloading from
/// the offset to the end of the file - which is what this used to do - means a
/// reader that wanted one page of one column pulled down the entire object, and
/// the row-group pruning above it saved nothing at all.
pub(crate) struct StreamingRange {
    storage: Arc<Box<dyn StorageAdapter + Send + Sync>>,
    key: String,
    /// Next byte to hand out.
    pos: u64,
    /// One past the last byte available.
    end: u64,
    buf: Vec<u8>,
    /// Offset in the object that `buf[0]` corresponds to.
    buf_start: u64,
}

impl StreamingRange {
    pub(crate) fn new(
        storage: Arc<Box<dyn StorageAdapter + Send + Sync>>,
        key: String,
        start: u64,
        end: u64,
    ) -> Self {
        Self {
            storage,
            key,
            pos: start,
            end,
            buf: Vec::new(),
            buf_start: 0,
        }
    }

    fn buffered(&self) -> Option<&[u8]> {
        let offset = self.pos.checked_sub(self.buf_start)? as usize;
        self.buf.get(offset..).filter(|s| !s.is_empty())
    }
}

impl Read for StreamingRange {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.end || out.is_empty() {
            return Ok(0);
        }

        if self.buffered().is_none() {
            let want = STREAM_BLOCK.min(self.end - self.pos);
            let block = self
                .storage
                .download_range(&self.key, self.pos, want)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            if block.is_empty() {
                return Ok(0);
            }
            self.buf_start = self.pos;
            self.buf = block;
        }

        let available = self.buffered().unwrap_or_default();
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.pos += n as u64;
        Ok(n)
    }
}

/// RemoteFile that implements ChunkReader with LRU Caching and Metadata Persistence.
pub struct RemoteFile {
    storage: Arc<Box<dyn StorageAdapter + Send + Sync>>,
    key: String,
    length: u64,
    cache: Arc<Mutex<LruCache<u64, Vec<u8>>>>, // Offset -> Data chunk
    metadata_cache_path: Option<PathBuf>,
}

impl RemoteFile {
    pub fn new(
        storage: Arc<Box<dyn StorageAdapter + Send + Sync>>,
        key: String,
        length: u64,
        cache_capacity: usize,
        metadata_cache_path: Option<PathBuf>,
    ) -> Self {
        let cap = NonZeroUsize::new(cache_capacity).unwrap_or(NonZeroUsize::new(10).unwrap());
        Self {
            storage,
            key,
            length,
            cache: Arc::new(Mutex::new(LruCache::new(cap))),
            metadata_cache_path,
        }
    }

    /// Try to read from local metadata cache if the read is for the footer
    fn try_read_local_metadata(&self, start: u64, length: usize) -> Option<Vec<u8>> {
        if let Some(path) = &self.metadata_cache_path {
            // Heuristic: If we are reading the END of the file, it's likely metadata.
            // Parquet footer is at the end.
            if start + length as u64 == self.length && path.exists() {
                // Check file size match? Or just try read.
                // The .meta file should contain the Footer bytes.
                // But we don't know if .meta is exactly 'length' bytes or full footer.
                // For simplicity: If .meta exists, read it ALL and see if it covers the request?
                // Or just read EXACTLY what is requested from .meta if feasible.

                // Implementation: .meta stores the LAST N bytes of the file.
                // If existing .meta size >= length, we can serve it.
                if let Ok(meta_bytes) = std::fs::read(path) {
                    if meta_bytes.len() >= length {
                        // Get the last 'length' bytes
                        let offset = meta_bytes.len() - length;
                        return Some(meta_bytes[offset..].to_vec());
                    }
                }
            }
        }
        None
    }

    fn save_to_local_metadata(&self, data: &[u8], start: u64) {
        if let Some(path) = &self.metadata_cache_path {
            // Heuristic: If we downloaded the end of the file, save it as .meta
            if start + data.len() as u64 == self.length {
                // Only save if it looks like a substantial footer (e.g. > 4 bytes)
                if data.len() > 8 {
                    // Overwrite .meta
                    if let Err(e) = std::fs::write(path, data) {
                        tracing::warn!("Failed to save metadata cache: {}", e);
                    } else {
                        tracing::debug!("Saved metadata cache to {:?}", path);
                    }
                }
            }
        }
    }
}

impl Length for RemoteFile {
    fn len(&self) -> u64 {
        self.length
    }
}

impl ChunkReader for RemoteFile {
    type T = Box<dyn Read + Send>;

    fn get_read(&self, start: u64) -> parquet::errors::Result<Self::T> {
        Ok(Box::new(StreamingRange::new(
            self.storage.clone(),
            self.key.clone(),
            start,
            self.length,
        )))
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<Bytes> {
        // 1. Check Metadata Cache (Disk)
        if let Some(data) = self.try_read_local_metadata(start, length) {
            tracing::debug!(
                "RemoteFile: Served {} bytes from disk metadata cache",
                length
            );
            return Ok(Bytes::from(data));
        }

        // 2. Check LRU Cache (Memory)
        // Simple cache key: (start). Matches exact chunks only for now.
        // For better caching, we'd align to pages, but Parquet reads differ.
        // We'll rely on "exact match" for repeated reads of same metadata structures.
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(data) = cache.get(&start) {
                if data.len() == length {
                    tracing::debug!("RemoteFile: Served {} bytes from LRU cache", length);
                    return Ok(Bytes::from(data.clone()));
                }
            }
        }

        // 3. Download
        tracing::debug!(
            "RemoteFile: Downloading {} bytes from offset {}",
            length,
            start
        );
        let data = self
            .storage
            .download_range(&self.key, start, length as u64)
            .map_err(|e| parquet::errors::ParquetError::External(Box::new(e)))?;

        // 4. Update Caches
        // Save to Metadata Cache if applicable
        self.save_to_local_metadata(&data, start);

        // Save to LRU
        {
            let mut cache = self.cache.lock().unwrap();
            cache.put(start, data.clone());
        }

        Ok(Bytes::from(data))
    }
}
