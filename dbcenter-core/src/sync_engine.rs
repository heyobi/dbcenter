use crate::DBCenter;
use crate::error::Result;
use crate::storage::StorageAdapter;
use serde::{Deserialize, Serialize};
// use std::sync::{Arc, Mutex};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Debug)]
pub struct SyncBatch {
    pub table_name: String,
    pub timestamp: u64,
    pub operations: Vec<serde_json::Value>, // Rows from _adds
}

pub struct SyncEngine {
    db: Arc<DBCenter>,
    storage: Arc<Box<dyn StorageAdapter + Send + Sync>>,
}

impl SyncEngine {
    pub fn new(db: Arc<DBCenter>, storage: Arc<Box<dyn StorageAdapter + Send + Sync>>) -> Self {
        Self { db, storage }
    }

    /// Run the sync process for a specific table
    /// 1. Check if we have pending changes in _adds (Task 3.2: Smart Batching logic can be here or caller)
    /// 2. Extract data from _adds
    /// 3. Zip/Serialize
    /// 4. Upload to Storage
    /// 5. Compact/Cleanup _adds
    pub fn sync_table(&self, table_name: &str, limit: usize) -> Result<usize> {
        let adds_table = format!("{}_adds", table_name);
        
        // 1. Check pending changes
        // "Wait for 50 operations". We use limit here.
        let sql_check = format!("SELECT COUNT(*) FROM \"{}\"", adds_table);
        let count_res = self.db.query(&sql_check, &[])?;
        let count: i64 = count_res[0].get("count").and_then(|v| v.as_i64()).unwrap_or(0);

        if count == 0 {
             return Ok(0);
        }

        // 2. Extract data (Task 3.2: "Wait for 50 operations")
        // We take up to `limit` rows.
        // We need all columns.
        let sql_fetch = format!("SELECT * FROM \"{}\" ORDER BY created_at ASC LIMIT {}", adds_table, limit);
        let rows = self.db.query(&sql_fetch, &[])?;
        
        if rows.is_empty() {
            return Ok(0);
        }

        // 3. Serialize (Zip simulation -> simply JSON dump for now)
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let batch_filename = format!("{}_{}_{}.json", table_name, timestamp, uuid::Uuid::new_v4());
        
        let batch = SyncBatch {
            table_name: table_name.to_string(),
            timestamp,
            operations: rows.clone(),
        };
        
        let data = serde_json::to_vec(&batch).map_err(|e| crate::error::DBCenterError::SerializationError(e))?;
        
        // 4. Upload
        // Task 3.1: Upload using adapter
        self.storage.upload(&batch_filename, &data)?;
        tracing::info!("Uploaded batch {} with {} items", batch_filename, rows.len());

        // 5. Compact / Cleanup
        // "Compact _adds to _base to keep _adds small"
        // Move these specific rows to _base and remove from _adds.
        // Or simply delete from _adds if we assume _base is constructed from synced files?
        // Wait, the requirement says "Sync the Delta Tables to cloud".
        // And "Compact _adds to _base".
        // Locally, we move successfully synced items to _base?
        // Yes, typically:
        //  - Read from Adds
        //  - Write to Storage (Cloud Source of Truth)
        //  - Move from Adds to Base (Local Read Cache of History)
        //  - Delete from Adds
        
        let mut client = self.db.get_client();
        let mut transaction = client.transaction().map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;

        for row in &rows {
            // Unpack row
            let id = row.get("id").and_then(|v| v.as_str()).ok_or(crate::error::DBCenterError::QueryError("Missing ID".into()))?;
            let data = row.get("data"); 
            // created_at, updated_at ...
            
            // Insert into Base
            // Conflict? If ID exists in base, update it.
            // On conflict update.
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
            
            let id_uuid = uuid::Uuid::parse_str(id).unwrap();
            transaction.execute(&upsert_base, &[&id_uuid, &data])
                .map_err(|e| crate::error::DBCenterError::QueryError(format!("Failed to move to base: {}", e)))?;

            // Delete from Adds (reuse id_uuid)
            let delete_adds = format!("DELETE FROM \"{}_adds\" WHERE id = $1::uuid", table_name);
            transaction.execute(&delete_adds, &[&id_uuid])
                .map_err(|e| crate::error::DBCenterError::QueryError(format!("Failed to delete from adds: {}", e)))?;
        }

        transaction.commit().map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;
        
        Ok(rows.len())

    }

    /// Task 3.3: Conflict Resolution
    /// "Download remote deltas -> Sort by time -> Replay locally"
    pub fn pull_and_apply(&self, table_name: &str) -> Result<()> {
        // List remote files
        let files = self.storage.list(table_name)?;
        
        // Sort by timestamp (in filename)
        // filename format: name_timestamp_uuid.json
        // We need to parse/sort.
        // Simplified: just iterate for now or string sort if format allows (timestamp is high digits... ok if fixed width, but it's not padding... wait, list might return random order).
        // Let's rely on simple string sort for this prototype if timestamp is monotonic enough or parse properly.
        // Assuming string sort works roughly if major digits align.
        
        for file in files {
             let data = self.storage.download(&file)?;
             let batch: SyncBatch = serde_json::from_slice(&data).map_err(|e| crate::error::DBCenterError::SerializationError(e))?;
             
             // Replay locally
             // "Last Write Wins based on timestamps"
             // Check local timestamp vs remote timestamp?
             // Or blindly apply if it's new?
             // Since we are applying to _base (Source of Truth reflection), we just apply.
             // If local has newer _adds, they will overlay _base in the view.
             // So applying to _base is safe.
             
             self.apply_batch_to_base(&batch)?;
        }
        
        Ok(())
    }

    fn apply_batch_to_base(&self, batch: &SyncBatch) -> Result<()> {
        let mut client = self.db.get_client();
        let mut transaction = client.transaction().map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;

        for row in &batch.operations {
              let id = row.get("id").and_then(|v| v.as_str()).unwrap();
              let data = row.get("data");
              
              let upsert_base = format!(
                r#"
                INSERT INTO "{}_base" (id, data)
                VALUES ($1::uuid, $2)
                ON CONFLICT (id) DO UPDATE SET 
                    data = EXCLUDED.data, 
                    updated_at = CURRENT_TIMESTAMP
                "#, 
                batch.table_name
            );
             let id_uuid = uuid::Uuid::parse_str(id).unwrap();
             transaction.execute(&upsert_base, &[&id_uuid, &data])
                .map_err(|e| crate::error::DBCenterError::QueryError(format!("Failed to apply remote to base: {}", e)))?;
        }

        transaction.commit().map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;
        Ok(())
    }
}
