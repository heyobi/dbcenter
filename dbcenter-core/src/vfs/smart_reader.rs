use crate::error::Result;
use crate::vfs::RemoteParquetReader;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::file::statistics::Statistics;

/// Smart Reader demonstrating Predicate Pushdown on Remote VFS
pub struct SmartParquetReader {
    reader: SerializedFileReader<RemoteParquetReader>,
}

impl SmartParquetReader {
    pub fn new(remote_reader: RemoteParquetReader) -> Result<Self> {
        // SerializedFileReader automatically reads the Footer (Metadata) 
        // using the underlying RemoteParquetReader::get_read/get_bytes
        // which triggers storage.download_range() for ONLY the end of the file.
        let reader = SerializedFileReader::new(remote_reader)
            .map_err(|e| crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(e.to_string())))?;
        
        Ok(Self { reader })
    }

    /// Return list of Row Group indices that MIGHT contain the given ID.
    /// Uses Parquet Statistics (Min/Max) to prune irrelevant groups.
    #[allow(deprecated)]
    pub fn filter_row_groups_by_id(&self, target_id: &str) -> Vec<usize> {
        let metadata = self.reader.metadata();
        let mut matching = Vec::new();

        for (i, rg) in metadata.row_groups().iter().enumerate() {
            // Find "id" column
            // We assume "id" is the first column logic or search by name.
            // For dbcenter schema, `id` is usually column 0.
            let mut found_stat = false;
            
            for col in rg.columns() {
                if col.column_path().string() == "id" {
                    if let Some(stats) = col.statistics() {
                        found_stat = true;
                        match stats {
                            Statistics::ByteArray(s) => {
                                // Convert parquet byte array to string to compare
                                let min = std::str::from_utf8(s.min().data()).unwrap_or("");
                                let max = std::str::from_utf8(s.max().data()).unwrap_or("");
                                
                                // Check Range
                                if target_id >= min && target_id <= max {
                                    tracing::debug!("Row Group {}: MATCH (Range [{} - {}] contains {})", i, min, max, target_id);
                                    matching.push(i);
                                } else {
                                    tracing::debug!("Row Group {}: SKIP (Range [{} - {}] excludes {})", i, min, max, target_id);
                                }
                            }
                            _ => {
                                // Wrong type? Fallback to read.
                                matching.push(i);
                            }
                        }
                    }
                }
            }

            if !found_stat {
                // No stats found, must read to be safe
                matching.push(i);
            }
        }
        
        matching
    }
}
