use crate::error::Result;
use crate::vfs::remote_file::RemoteFile;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::file::statistics::Statistics;
use parquet::record::Row;

/// Smart Reader demonstrating Predicate Pushdown on Remote VFS
pub struct SmartParquetReader {
    reader: SerializedFileReader<RemoteFile>,
}

impl SmartParquetReader {
    pub fn new(remote_file: RemoteFile) -> Result<Self> {
        // SerializedFileReader automatically reads the Footer (Metadata) 
        // using the underlying RemoteFile which caches it to disk/LRU.
        let reader = SerializedFileReader::new(remote_file)
            .map_err(|e| crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(e.to_string())))?;
        
        Ok(Self { reader })
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
                                let min = s.min_opt().map(|b| std::str::from_utf8(b.data()).unwrap_or("")).unwrap_or("");
                                let max = s.max_opt().map(|b| std::str::from_utf8(b.data()).unwrap_or("")).unwrap_or("");
                                
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
            // This triggers download ONLY for this RG's columns due to RemoteFile
            let row_group_reader = self.reader.get_row_group(rg_idx)
                 .map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;
            
             // Read rows (Iterate)
             // Note: get_row_iter might trigger bulk reads for columns
             if let Ok(iter) = row_group_reader.get_row_iter(None) {
                 for record in iter {
                     let row = record.map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;
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
