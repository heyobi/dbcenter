use crate::storage::StorageAdapter;
use bytes::Bytes;
use lru::LruCache;
use parquet::file::reader::{ChunkReader, Length};
use std::io::Read;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The most pulled in one request when streaming a range.
///
/// Small enough that a reader which stops early has not paid for the rest of
/// the file; large enough that sequential reading is not one request per page.
const STREAM_BLOCK: u64 = 512 * 1024;

/// The first request of a stream, which then doubles up to `STREAM_BLOCK`.
///
/// Every stream used to start at the full 512 KB. That was invisible while
/// files were uncompressed and column chunks were megabytes, but a compressed
/// column chunk is tens of kilobytes: a lookup that wanted one page of each of
/// a handful of columns pulled 512 KB per column, the same bytes over again
/// for neighbouring chunks, and read more than twice the size of the file.
/// Starting small costs one page's worth for a reader that stops early, and a
/// reader that keeps going reaches full-size blocks within a few requests.
const FIRST_BLOCK: u64 = 64 * 1024;

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
    /// How much the next request asks for.
    block: u64,
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
            block: FIRST_BLOCK,
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
            let want = self.block.min(self.end - self.pos);
            self.block = (self.block * 2).min(STREAM_BLOCK);
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

/// A byte range fetched ahead of the reads that will want it.
type Prefetched = Arc<Mutex<Option<(u64, Arc<Vec<u8>>)>>>;

/// Reads out of a prefetched range, and from storage past its end.
struct PrefetchedRead {
    data: Arc<Vec<u8>>,
    pos: usize,
    rest: StreamingRange,
}

impl Read for PrefetchedRead {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos < self.data.len() {
            let n = (self.data.len() - self.pos).min(out.len());
            out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        self.rest.read(out)
    }
}

/// RemoteFile that implements ChunkReader with LRU Caching and Metadata Persistence.
///
/// Clones share their caches and the prefetched range, so a reader can keep a
/// handle to prefetch through while the Parquet reader owns another.
#[derive(Clone)]
pub struct RemoteFile {
    storage: Arc<Box<dyn StorageAdapter + Send + Sync>>,
    key: String,
    length: u64,
    cache: Arc<Mutex<LruCache<u64, Vec<u8>>>>, // Offset -> Data chunk
    metadata_cache_path: Option<PathBuf>,
    prefetched: Prefetched,
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
            prefetched: Arc::new(Mutex::new(None)),
        }
    }

    /// Fetch `[start, start + length)` in one request, for the reads to come.
    ///
    /// The Parquet reader asks for a column chunk a page at a time, through a
    /// stream with no length, so it cannot say up front how much it wants -
    /// and guessing costs either bytes (large blocks past the chunk's end) or
    /// requests (small ones). The caller can say: a row group's column chunks
    /// are listed in the footer with their exact byte ranges.
    pub fn prefetch(&self, start: u64, length: u64) -> crate::error::Result<()> {
        let data = self.storage.download_range(&self.key, start, length)?;
        *self.prefetched.lock().unwrap() = Some((start, Arc::new(data)));
        Ok(())
    }

    /// The prefetched bytes from `start` on, if the prefetch covers it.
    fn prefetched_from(&self, start: u64) -> Option<(u64, Arc<Vec<u8>>)> {
        let guard = self.prefetched.lock().unwrap();
        let (p_start, data) = guard.as_ref()?;
        let p_end = p_start + data.len() as u64;
        (start >= *p_start && start < p_end).then(|| (*p_start, Arc::clone(data)))
    }

    /// Serve a read out of the on-disk footer cache, if it covers the range.
    ///
    /// The cache file is the byte range it holds: eight bytes of little-endian
    /// start offset, then the bytes themselves. Storing the offset is what makes
    /// this work at all - Parquet's metadata block ends eight bytes before the
    /// end of the file, not at the end, so a cache that could only describe
    /// itself as "the last N bytes" could never hold it.
    fn try_read_local_metadata(&self, start: u64, length: usize) -> Option<Vec<u8>> {
        let path = self.metadata_cache_path.as_ref()?;
        let raw = std::fs::read(path).ok()?;
        if raw.len() < 8 {
            return None;
        }
        let cached_start = u64::from_le_bytes(raw[..8].try_into().ok()?);
        let body = &raw[8..];
        let cached_end = cached_start + body.len() as u64;

        if start < cached_start || start + length as u64 > cached_end {
            return None;
        }
        let offset = (start - cached_start) as usize;
        Some(body[offset..offset + length].to_vec())
    }

    /// Keep a downloaded footer so the next open does not fetch it again.
    ///
    /// Parquet reads its metadata in two steps: eight trailing bytes giving the
    /// length, then the metadata block that ends just before them. Only the
    /// second one is worth caching, and it is the one the old heuristic - "the
    /// read ends at the end of the file, and is longer than eight bytes" - could
    /// never match, so nothing was ever written here.
    fn save_to_local_metadata(&self, data: &[u8], start: u64) {
        let Some(path) = &self.metadata_cache_path else {
            return;
        };
        // Anything ending in the last eight bytes of the file is the footer.
        let end = start + data.len() as u64;
        if data.len() <= 8 || end + 8 < self.length || end > self.length {
            return;
        }

        let mut buffer = Vec::with_capacity(8 + data.len());
        buffer.extend_from_slice(&start.to_le_bytes());
        buffer.extend_from_slice(data);
        if let Err(e) = std::fs::write(path, &buffer) {
            tracing::warn!("Failed to save metadata cache: {}", e);
        } else {
            tracing::debug!(
                "Cached {} footer bytes from offset {} to {:?}",
                data.len(),
                start,
                path
            );
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
        if let Some((p_start, data)) = self.prefetched_from(start) {
            let p_end = p_start + data.len() as u64;
            return Ok(Box::new(PrefetchedRead {
                data,
                pos: (start - p_start) as usize,
                rest: StreamingRange::new(
                    self.storage.clone(),
                    self.key.clone(),
                    p_end,
                    self.length,
                ),
            }));
        }
        Ok(Box::new(StreamingRange::new(
            self.storage.clone(),
            self.key.clone(),
            start,
            self.length,
        )))
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<Bytes> {
        // 0. A prefetched range that covers it.
        if let Some((p_start, data)) = self.prefetched_from(start) {
            let offset = (start - p_start) as usize;
            if offset + length <= data.len() {
                return Ok(Bytes::copy_from_slice(&data[offset..offset + length]));
            }
        }

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
