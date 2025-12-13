use crate::DBCenter;
use crate::error::Result;
use crate::storage::StorageAdapter;
use crate::vfs::RemoteParquetReader;
use arrow::array::{StringArray, Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use bytes::Bytes;
use postgres::Row;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::arrow_writer::ArrowWriter;
use parquet::file::properties::WriterProperties;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct SyncEngine {
    db: Arc<DBCenter>,
    storage: Arc<Box<dyn StorageAdapter + Send + Sync>>,
}

impl SyncEngine {
    pub fn new(db: Arc<DBCenter>, storage: Arc<Box<dyn StorageAdapter + Send + Sync>>) -> Self {
        Self { db, storage }
    }

    /// Helper to convert Postgres rows to Arrow RecordBatch
    /// Assumes all columns are cast to TEXT in the query.
    fn rows_to_record_batch(&self, rows: Vec<Row>) -> Result<RecordBatch> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new("data", DataType::Utf8, true),
            Field::new("created_at", DataType::Utf8, true), 
            Field::new("updated_at", DataType::Utf8, true),
        ]));

        let mut ids = Vec::new();
        let mut datas = Vec::new();
        let mut created_ats = Vec::new();
        let mut updated_ats = Vec::new();

        for row in rows {
            // Since we cast to ::text in SQL, get("...") returns Option<String> or String
            let id: String = row.get("id"); 
            let data: Option<String> = row.get("data");
            let created_at: Option<String> = row.get("created_at"); 
            let updated_at: Option<String> = row.get("updated_at");

            ids.push(id);
            datas.push(data);
            created_ats.push(created_at);
            updated_ats.push(updated_at);
        }

        RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(ids)),
                Arc::new(StringArray::from(datas)),
                Arc::new(StringArray::from(created_ats)),
                Arc::new(StringArray::from(updated_ats)),
            ],
        ).map_err(|e| crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(e.to_string())))
    }

    /// Task 3.2: Smart Batching (Parquet)
    pub fn sync_table(&self, table_name: &str, limit: usize) -> Result<usize> {
        let adds_table = format!("{}_adds", table_name);
        
        // 1. Check pending changes
        // Use block to drop client lock quickly
        let count: i64 = {
            let mut client = self.db.get_client();
            let sql_check = format!("SELECT COUNT(*) FROM \"{}\"", adds_table);
            let rows = client.query(&sql_check, &[])
                .map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;
            rows[0].get("count")
        };

        if count == 0 {
             return Ok(0);
        }

        // 2. Fetch rows
        // We cast EVERYTHING to text to simplify Arrow generic handling and strictly control format
        let sql_fetch = format!(
            "SELECT id::text, data::text, created_at::text, updated_at::text FROM \"{}\" ORDER BY created_at ASC LIMIT {}", 
            adds_table, limit
        );
        let rows = {
            let mut client = self.db.get_client();
            client.query(&sql_fetch, &[])
                .map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?
        };
        
        if rows.is_empty() {
            return Ok(0);
        }

        // 3. Convert to Arrow
        let batch = self.rows_to_record_batch(rows.clone())?; // Clone needed because we iterate rows later

        // 4. Write to Parquet (In-Memory)
        let mut buffer = Vec::new();
        let props = WriterProperties::builder().build();
        let mut writer = ArrowWriter::try_new(&mut buffer, batch.schema(), Some(props))
             .map_err(|e| crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(e.to_string())))?;
        
        writer.write(&batch).map_err(|e| crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(e.to_string())))?;
        writer.close().map_err(|e| crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(e.to_string())))?;

        // 5. Upload
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let batch_filename = format!("{}_DELTA_{}_{}.parquet", table_name, timestamp, uuid::Uuid::new_v4());
        
        self.storage.upload(&batch_filename, &buffer)?;
        tracing::info!("Uploaded Parquet batch {} with {} items", batch_filename, rows.len());

        // 6. Compact / Cleanup (Move to Base)
        let mut client = self.db.get_client();
        let mut transaction = client.transaction().map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;

        for row in &rows {
            let id: String = row.get("id");
            // data string is JSON string. We need JSONB value for query param? 
            // Or we can cast $2::jsonb in SQL.
            let data_str: Option<String> = row.get("data"); 
            let data_val = match data_str {
                Some(s) => serde_json::from_str(&s).unwrap_or(serde_json::Value::Null),
                None => serde_json::Value::Null
            };
            
            let upsert_base = format!(
                r#"
                INSERT INTO "{}_base" (id, data, created_at, updated_at)
                VALUES ($1::uuid, $2, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
                ON CONFLICT (id) DO UPDATE SET 
                    data = EXCLUDED.data, 
                    updated_at = EXCLUDED.updated_at
                "#, 
                table_name
            );
            
            let id_uuid = uuid::Uuid::parse_str(&id).unwrap();
            transaction.execute(&upsert_base, &[&id_uuid, &data_val])
                .map_err(|e| crate::error::DBCenterError::QueryError(format!("Failed to move to base: {}", e)))?;

            let delete_adds = format!("DELETE FROM \"{}_adds\" WHERE id = $1::uuid", table_name);
            transaction.execute(&delete_adds, &[&id_uuid])
                .map_err(|e| crate::error::DBCenterError::QueryError(format!("Failed to delete from adds: {}", e)))?;
        }

        transaction.commit().map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;
        
        Ok(rows.len())
    }

    /// Task 3.5: Compaction / Vacuum
    /// "Merge x_base + x_adds into a new x_base_v2. Upload x_base_v2. Delete old deltas."
    pub fn vacuum(&self, table_name: &str) -> Result<()> {
        tracing::info!("Starting Vacuum for table: {}", table_name);

        // 1. Ensure consistency: Pull all pending changes from cloud to local _base
        self.pull_and_apply(table_name)?;

        // 2. Identify files to delete (All current files, including old BASE and DELTAs)
        // We do this AFTER pull, so we know we processed them.
        let files_to_delete = self.storage.list(table_name)?;

        // 3. Export _base (Postgres) to a new BASE Parquet file
        // We select everything from _base. Cast to text for consistency.
        let sql_base = format!(
            "SELECT id::text, data::text, created_at::text, updated_at::text FROM \"{}_base\" ORDER BY created_at ASC", 
            table_name
        );
        let rows = {
            let mut client = self.db.get_client();
            client.query(&sql_base, &[])
                .map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?
        };
        
        if rows.is_empty() {
             tracing::info!("Vacuum: Base is empty, nothing to export.");
             // If local base is empty, we still proceed to delete remote files if authority.
             // But let's export empty parquet to be safe and reset state cleanly.
        }

        let batch = self.rows_to_record_batch(rows.clone())?; // Clone or pass ownership? helper usage allows ownership transfer but earlier implementation took ref? 
        // My helper signature `rows: Vec<Row>` takes ownership. So `rows.clone()`? `rows` is used for logging `len` later.
        // Actually logging uses `rows.len()`. I can log first.

        let mut buffer = Vec::new();
        let props = WriterProperties::builder().build();
        let mut writer = ArrowWriter::try_new(&mut buffer, batch.schema(), Some(props))
             .map_err(|e| crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(e.to_string())))?;
        
        writer.write(&batch).map_err(|e| crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(e.to_string())))?;
        writer.close().map_err(|e| crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(e.to_string())))?;

        // 4. Upload New Base
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let base_filename = format!("{}_BASE_{}_{}.parquet", table_name, timestamp, uuid::Uuid::new_v4());
        
        self.storage.upload(&base_filename, &buffer)?;
        tracing::info!("Uploaded New Base {} with {} items", base_filename, rows.len());

        // 5. Delete Old Files
        for file in &files_to_delete {
            if file.ends_with(".parquet") {
                self.storage.delete(&file)?;
            }
        }
        
        tracing::info!("Vacuum Complete. {} old files deleted.", files_to_delete.len());

        Ok(())
    }

    /// Task 3.3: Conflict Resolution (Parquet Download)
    pub fn pull_and_apply(&self, table_name: &str) -> Result<()> {
        let files = self.storage.list(table_name)?;
        
        let mut sorted_files = files.clone();
        sorted_files.sort(); 

        for file in sorted_files {
             if !file.ends_with(".parquet") { continue; }
             
             // Task 3.4 VFS Demonstration
             let full_data = self.storage.download(&file)?;
             let file_len = full_data.len() as u64; 
             
             let remote_reader = RemoteParquetReader::new(self.storage.clone(), file.clone(), file_len);
             
             let builder = ParquetRecordBatchReaderBuilder::try_new(remote_reader)
                  .map_err(|e| crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(e.to_string())))?;
             
             let mut reader = builder.build()
                  .map_err(|e| crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(e.to_string())))?;

             let mut client = self.db.get_client();
             let mut transaction = client.transaction().map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;

             while let Some(batch_result) = reader.next() {
                 let batch = batch_result.map_err(|e| crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(e.to_string())))?;
                 
                 let ids = batch.column(0).as_any().downcast_ref::<StringArray>().unwrap();
                 let datas = batch.column(1).as_any().downcast_ref::<StringArray>().unwrap();
                 // col 2 is created_at, col 3 is updated_at
                 let updated_ats = batch.column(3).as_any().downcast_ref::<StringArray>().unwrap();
                 
                 for i in 0..batch.num_rows() {
                     let id_str = ids.value(i);
                     let data_str = if datas.is_null(i) { "{}" } else { datas.value(i) };
                     // Parse/Validate format or fallback to now
                     // We pass as String to avoid rust-postgres trait issues.
                     // Ensure naive_utc to string is consistent.
                     let updated_at_str = if updated_ats.is_null(i) {
                         chrono::Utc::now().naive_utc().to_string()
                     } else {
                         updated_ats.value(i).to_string()
                     };
                     
                     // Ensure it is not empty
                     let updated_at_val = if updated_at_str.trim().is_empty() {
                         chrono::Utc::now().naive_utc().to_string()
                     } else {
                         updated_at_str
                     };

                     let data_json: serde_json::Value = serde_json::from_str(data_str).unwrap_or(serde_json::Value::Null);

                     // LWW: Update only if incoming (EXCLUDED) is newer than existing
                     // We cast $3 to timestamp explicitly.
                     let upsert_base = format!(
                        r#"
                        INSERT INTO "{}_base" (id, data, updated_at)
                        VALUES ($1::uuid, $2, $3::timestamp)
                        ON CONFLICT (id) DO UPDATE SET 
                            data = EXCLUDED.data, 
                            updated_at = EXCLUDED.updated_at
                        WHERE EXCLUDED.updated_at > "{}_base".updated_at
                        "#, 
                        table_name, table_name
                    );
                    
                    let id_uuid = uuid::Uuid::parse_str(id_str).unwrap();
                    transaction.execute(&upsert_base, &[&id_uuid, &data_json, &updated_at_val])
                        .map_err(|e| crate::error::DBCenterError::QueryError(format!("Failed to apply parquet row to base: {}", e)))?;
                 }
             }

             transaction.commit().map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;
        }
        
        Ok(())
    }
}
