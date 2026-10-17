use crate::error::Result;
use crate::vfs::remote_file::RemoteFile;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::file::statistics::Statistics;
use parquet::record::Row;

/// Smart Reader demonstrating Predicate Pushdown on Remote VFS
pub struct SmartParquetReader {
    reader: SerializedFileReader<RemoteFile>,
    /// A handle on the same file, to prefetch a row group through.
    file: RemoteFile,
}

impl SmartParquetReader {
    pub fn new(remote_file: RemoteFile) -> Result<Self> {
        // SerializedFileReader automatically reads the Footer (Metadata)
        // using the underlying RemoteFile which caches it to disk/LRU.
        let file = remote_file.clone();
        let reader = SerializedFileReader::new(remote_file).map_err(|e| {
            crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(
                e.to_string(),
            ))
        })?;

        Ok(Self { reader, file })
    }

    /// Return list of Row Group indices that MIGHT contain the given ID.
    pub fn filter_row_groups_by_id(&self, target_id: &str) -> Vec<usize> {
        let metadata = self.reader.metadata();
        let mut matching = Vec::new();

        for (i, rg) in metadata.row_groups().iter().enumerate() {
            let mut found_stat = false;

            for col in rg.columns() {
                if col.column_path().string() == "id" {
                    if let Some(stats) = col.statistics() {
                        found_stat = true;
                        match stats {
                            Statistics::ByteArray(s) => {
                                let min = s
                                    .min_opt()
                                    .map(|b| std::str::from_utf8(b.data()).unwrap_or(""))
                                    .unwrap_or("");
                                let max = s
                                    .max_opt()
                                    .map(|b| std::str::from_utf8(b.data()).unwrap_or(""))
                                    .unwrap_or("");

                                if target_id >= min && target_id <= max {
                                    matching.push(i);
                                }
                            }
                            _ => {
                                matching.push(i);
                            }
                        }
                    }
                }
            }

            if !found_stat {
                matching.push(i);
            }
        }

        matching
    }

    /// Execute a smart scan, downloading ONLY necessary row groups
    pub fn scan_for_id(&self, target_id: &str) -> Result<Vec<Row>> {
        let matching_groups = self.filter_row_groups_by_id(target_id);
        let mut results = Vec::new();

        // tracing::debug!("SmartScan: Matched {}/{} row groups for id={}",
        //    matching_groups.len(), self.reader.metadata().num_row_groups(), target_id);

        for rg_idx in matching_groups {
            // The whole row group in one request: the footer says exactly where
            // its column chunks are. Read page by page instead, a lookup either
            // made a request per page or fetched well past each chunk's end -
            // with compressed columns, more than the file's own size.
            let row_group = self.reader.metadata().row_group(rg_idx);
            let (mut low, mut high) = (u64::MAX, 0u64);
            for column in row_group.columns() {
                let (start, length) = column.byte_range();
                low = low.min(start);
                high = high.max(start + length);
            }
            if low < high {
                self.file.prefetch(low, high - low)?;
            }

            // This triggers download ONLY for this RG's columns due to RemoteFile
            let row_group_reader = self
                .reader
                .get_row_group(rg_idx)
                .map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;

            // Read rows (Iterate)
            // Note: get_row_iter might trigger bulk reads for columns
            if let Ok(iter) = row_group_reader.get_row_iter(None) {
                for record in iter {
                    let row = record
                        .map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;
                    // Double check predicate (since stats are just min/max)
                    // Find 'id' column explicitly
                    let mut id_val = String::new();
                    for (col_name, field) in row.get_column_iter() {
                        if col_name == "id" {
                            id_val = field.to_string();
                            break;
                        }
                    }

                    // Removes quotes if string
                    let id_clean = id_val.trim_matches('"');

                    if id_clean == target_id {
                        results.push(row);
                    }
                }
            }
        }

        Ok(results)
    }
}

/// One-request lookups by id against a table's objects in storage.
///
/// A lookup used to rediscover everything each time: it listed the bucket,
/// asked every object its size, read every footer and then fetched every row
/// group whose id range could hold the id - which, for a delta written in
/// arrival order, is all of it. About ten round trips for one row: 292 ms with
/// the bucket 20 ms away.
///
/// Objects are never modified - every sync and compaction writes a new key -
/// so what is learned about one is true for as long as it exists. Its size,
/// footer and id bloom filters are learned once and kept here. Only the list
/// of objects can go stale; it is kept for `listing_ttl`, and the engine
/// forgets it whenever it writes to the table itself. A lookup then fetches
/// just the row groups whose statistics and bloom filter both say the id may
/// be there: normally one, in one request.
pub struct LookupIndex {
    storage: std::sync::Arc<Box<dyn crate::storage::StorageAdapter + Send + Sync>>,
    meta_dir: std::path::PathBuf,
    listing_ttl: std::sync::Mutex<std::time::Duration>,
    tables: std::sync::Mutex<std::collections::HashMap<String, TableIndex>>,
}

struct TableIndex {
    listed_at: std::time::Instant,
    objects: Vec<IndexedObject>,
}

struct IndexedObject {
    key: String,
    file: RemoteFile,
    metadata: std::sync::Arc<parquet::file::metadata::ParquetMetaData>,
    id_column: Option<usize>,
    /// Per row group: not looked at yet, or the id filter (if it has one).
    blooms: Vec<Option<Option<parquet::bloom_filter::Sbbf>>>,
}

fn parquet_error(e: parquet::errors::ParquetError) -> crate::error::DBCenterError {
    crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(e.to_string()))
}

impl IndexedObject {
    fn open(
        storage: &std::sync::Arc<Box<dyn crate::storage::StorageAdapter + Send + Sync>>,
        meta_dir: &std::path::Path,
        key: &str,
    ) -> Result<Self> {
        let size = storage.size(key)?;
        let file = RemoteFile::new(
            storage.clone(),
            key.to_string(),
            size,
            64,
            Some(meta_dir.join(format!("{}.meta", key.replace('/', "_")))),
        );
        let reader = SerializedFileReader::new(file.clone()).map_err(parquet_error)?;
        let metadata = std::sync::Arc::new(reader.metadata().clone());
        let id_column = metadata
            .file_metadata()
            .schema_descr()
            .columns()
            .iter()
            .position(|c| c.path().string() == "id");
        let blooms = vec![None; metadata.num_row_groups()];
        Ok(Self {
            key: key.to_string(),
            file,
            metadata,
            id_column,
            blooms,
        })
    }

    /// Whether row group `rg` can hold `id`, by its min/max and then its bloom
    /// filter. The filter costs one request the first time and none after.
    fn may_contain(&mut self, rg: usize, id: &str) -> Result<bool> {
        let Some(col) = self.id_column else {
            return Ok(true);
        };
        let row_group = self.metadata.row_group(rg);
        if let Some(Statistics::ByteArray(s)) = row_group.column(col).statistics() {
            let min = s.min_opt().map(|b| b.data()).unwrap_or(b"");
            let max = s.max_opt().map(|b| b.data()).unwrap_or(b"");
            if s.min_opt().is_some() && (id.as_bytes() < min || id.as_bytes() > max) {
                return Ok(false);
            }
        }
        if row_group.column(col).bloom_filter_offset().is_none() {
            return Ok(true);
        }
        if self.blooms[rg].is_none() {
            use parquet::file::reader::RowGroupReader;
            let props = parquet::file::properties::ReaderProperties::builder()
                .set_read_bloom_filter(true)
                .build();
            let reader = parquet::file::serialized_reader::SerializedRowGroupReader::new(
                std::sync::Arc::new(self.file.clone()),
                row_group,
                None,
                std::sync::Arc::new(props),
            )
            .map_err(parquet_error)?;
            self.blooms[rg] = Some(reader.get_column_bloom_filter(col).cloned());
        }
        Ok(match &self.blooms[rg] {
            Some(Some(bloom)) => bloom.check(&id),
            _ => true,
        })
    }

    /// The rows of row group `rg` whose id is `id`, fetched in one request.
    fn read(&self, rg: usize, id: &str) -> Result<Vec<Row>> {
        use parquet::file::reader::RowGroupReader;
        let row_group = self.metadata.row_group(rg);
        let (mut low, mut high) = (u64::MAX, 0u64);
        for column in row_group.columns() {
            let (start, length) = column.byte_range();
            low = low.min(start);
            high = high.max(start + length);
        }
        if low < high {
            self.file.prefetch(low, high - low)?;
        }
        let reader = parquet::file::serialized_reader::SerializedRowGroupReader::new(
            std::sync::Arc::new(self.file.clone()),
            row_group,
            None,
            std::sync::Arc::new(parquet::file::properties::ReaderProperties::builder().build()),
        )
        .map_err(parquet_error)?;
        let mut rows = Vec::new();
        for record in reader.get_row_iter(None).map_err(parquet_error)? {
            let row = record.map_err(parquet_error)?;
            let matches = row
                .get_column_iter()
                .any(|(name, field)| name == "id" && field.to_string().trim_matches('"') == id);
            if matches {
                rows.push(row);
            }
        }
        Ok(rows)
    }
}

impl LookupIndex {
    /// How long the list of objects is trusted before it is fetched again.
    /// Rows another node pushes become visible to lookups within this long.
    pub const DEFAULT_LISTING_TTL: std::time::Duration = std::time::Duration::from_secs(5);

    pub fn new(
        storage: std::sync::Arc<Box<dyn crate::storage::StorageAdapter + Send + Sync>>,
        meta_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            storage,
            meta_dir,
            listing_ttl: std::sync::Mutex::new(Self::DEFAULT_LISTING_TTL),
            tables: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn set_listing_ttl(&self, ttl: std::time::Duration) {
        *self.listing_ttl.lock().unwrap() = ttl;
    }

    /// Forget the list of objects for `table`, after writing to it. What is
    /// known about each object stays: objects do not change.
    pub fn invalidate(&self, table: &str) {
        if let Some(t) = self.tables.lock().unwrap().get_mut(table) {
            t.listed_at = std::time::Instant::now() - *self.listing_ttl.lock().unwrap() * 2;
        }
    }

    fn refresh(&self, table: &str, index: &mut TableIndex) -> Result<()> {
        let mut keys: Vec<String> = self
            .storage
            .list(table)?
            .into_iter()
            .filter(|f| f.ends_with(".parquet") && (f.contains("_BASE_") || f.contains("_DELTA_")))
            .collect();
        // Names carry a unix timestamp, so sorted they put the base first and
        // the deltas after it in the order they were written; later ones win.
        keys.sort();
        index.objects.retain(|o| keys.binary_search(&o.key).is_ok());
        std::fs::create_dir_all(&self.meta_dir).ok();
        for key in &keys {
            if !index.objects.iter().any(|o| &o.key == key) {
                index
                    .objects
                    .push(IndexedObject::open(&self.storage, &self.meta_dir, key)?);
            }
        }
        index.objects.sort_by(|a, b| a.key.cmp(&b.key));
        index.listed_at = std::time::Instant::now();
        Ok(())
    }

    /// The newest version of the row with this id, if any object holds one.
    pub fn lookup(&self, table: &str, id: &str) -> Result<Option<Row>> {
        match self.try_lookup(table, id) {
            Ok(found) => Ok(found),
            // A compaction elsewhere may have deleted an object since the list
            // was taken. List again and retry once before giving up.
            Err(_) => {
                self.invalidate(table);
                self.try_lookup(table, id)
            }
        }
    }

    fn try_lookup(&self, table: &str, id: &str) -> Result<Option<Row>> {
        let ttl = *self.listing_ttl.lock().unwrap();
        let mut tables = self.tables.lock().unwrap();
        let index = tables
            .entry(table.to_string())
            .or_insert_with(|| TableIndex {
                listed_at: std::time::Instant::now() - ttl * 2,
                objects: Vec::new(),
            });
        if index.listed_at.elapsed() >= ttl {
            self.refresh(table, index)?;
        }

        let mut newest = None;
        for object in index.objects.iter_mut() {
            for rg in 0..object.metadata.num_row_groups() {
                if object.may_contain(rg, id)? {
                    if let Some(row) = object.read(rg, id)?.pop() {
                        newest = Some(row);
                    }
                }
            }
        }
        Ok(newest)
    }
}
