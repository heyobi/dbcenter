use crate::error::Result;
use crate::storage::StorageAdapter;
use crate::vfs::RemoteParquetReader;
use crate::DBCenter;
use arrow::array::{Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
// use bytes::Bytes;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::arrow_writer::ArrowWriter;
use parquet::file::properties::WriterProperties;
use postgres::Row;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
// use std::sync::atomic::{AtomicBool, Ordering};
// use serde::ser::Error;

pub struct SyncEngine {
    db: Arc<DBCenter>,
    storage: Arc<Box<dyn StorageAdapter + Send + Sync>>,
}

/// Synchronization Mode
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SyncMode {
    Full,
    Partial,
}

/// Turn a Parquet row from a synced table into the document it came from.
///
/// The synced schema is (id, data, created_at, updated_at) with everything cast
/// to text, and `data` holds the document as JSON. This used to emit `col_0`,
/// `data`, `col_2`, `col_3`, which no caller could do anything with.
fn row_to_document(row: &parquet::record::Row) -> serde_json::Value {
    let mut doc = serde_json::Map::new();
    let mut fields = serde_json::Map::new();

    for (name, value) in row.get_column_iter() {
        let text = value.to_string();
        let text = text.trim_matches('"');
        match name.as_str() {
            "data" => {
                // Parquet's Display escapes the JSON it holds.
                let unescaped = text.replace("\\\"", "\"");
                if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(&unescaped) {
                    fields = map;
                }
            }
            other => {
                doc.insert(
                    other.to_string(),
                    serde_json::Value::String(text.to_string()),
                );
            }
        }
    }

    // The document's own fields sit at the top level, the way find() returns
    // them, with the row's id alongside.
    for (k, v) in fields {
        doc.insert(k, v);
    }
    if let Some(id) = doc.get("id").cloned() {
        doc.insert("_id".to_string(), id);
    }
    serde_json::Value::Object(doc)
}

/// Add a column for a restored document field if the table has not got one.
///
/// The synced schema is only (id, data, created_at, updated_at); the fields a
/// document carries are whatever it happened to have. A database restoring a
/// table it has never seen has to grow the columns as it reads them.
fn ensure_restored_column(
    transaction: &mut postgres::Transaction<'_>,
    table: &str,
    column: &str,
    value: &serde_json::Value,
) -> crate::error::Result<bool> {
    let sql_type = match value {
        serde_json::Value::Bool(_) => "BOOLEAN",
        serde_json::Value::Number(n) if n.is_i64() => "BIGINT",
        serde_json::Value::Number(_) => "DOUBLE PRECISION",
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => "JSONB",
        // A null says nothing about the type; text takes anything.
        _ => "TEXT",
    };

    // Both delta tables, because the view unions them and a UNION ALL needs the
    // same shape on each side.
    for target in [format!("{}_base", table), format!("{}_adds", table)] {
        let sql = format!(
            "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} {}",
            crate::query::quote_ident(&target),
            crate::query::quote_ident(column),
            sql_type
        );
        transaction.batch_execute(&sql).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!(
                "Could not add column '{}' while restoring '{}': {}",
                column,
                target,
                crate::utils::format_postgres_error(&e)
            ))
        })?;
    }
    Ok(true)
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
        )
        .map_err(|e| {
            crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(
                e.to_string(),
            ))
        })
    }

    /// Task 3.2: Smart Batching (Parquet)
    pub fn sync_table(&self, table_name: &str, limit: usize) -> Result<usize> {
        let adds_table = format!("{}_adds", table_name);

        // 1. Check pending changes
        // Use block to drop client lock quickly
        let count: i64 = {
            let mut client = self.db.get_client();
            let sql_check = format!("SELECT COUNT(*) FROM \"{}\"", adds_table);
            let rows = client.query(&sql_check, &[]).map_err(|e| {
                crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
            })?;
            rows[0].get("count")
        };

        if count == 0 {
            return Ok(0);
        }

        // 2. Fetch rows
        // We cast EVERYTHING to text to simplify Arrow generic handling and strictly control format
        // The document lives in the row's own columns: inserts go through the
        // relational path, which adds a column per field, and leaves the `data`
        // JSONB column null. Reading only `data` therefore synced ids and
        // timestamps and nothing else - every document written through the
        // collection API was lost on the way to object storage.
        //
        // Take the whole row as JSON instead, minus the bookkeeping columns, and
        // merge in whatever `data` did hold, for rows written as JSONB directly.
        let sql_fetch = format!(
            r#"SELECT id::text,
                      ((to_jsonb(t) - 'id' - 'data' - 'created_at' - 'updated_at')
                       || COALESCE(t.data, '{{}}'::jsonb))::text AS data,
                      created_at::text,
                      updated_at::text
               FROM "{}" t
               ORDER BY created_at ASC
               LIMIT {}"#,
            adds_table, limit
        );
        let rows = {
            let mut client = self.db.get_client();
            client.query(&sql_fetch, &[]).map_err(|e| {
                crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
            })?
        };

        if rows.is_empty() {
            return Ok(0);
        }

        // 3. Convert to Arrow
        let batch = self.rows_to_record_batch(rows.clone())?; // Clone needed because we iterate rows later

        // 4. Write to Parquet (In-Memory)
        let mut buffer = Vec::new();
        let props = WriterProperties::builder().build();
        let mut writer =
            ArrowWriter::try_new(&mut buffer, batch.schema(), Some(props)).map_err(|e| {
                crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(
                    e.to_string(),
                ))
            })?;

        writer.write(&batch).map_err(|e| {
            crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(
                e.to_string(),
            ))
        })?;
        writer.close().map_err(|e| {
            crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(
                e.to_string(),
            ))
        })?;

        // 5. Upload
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let batch_filename = format!(
            "{}_DELTA_{}_{}.parquet",
            table_name,
            timestamp,
            uuid::Uuid::new_v4()
        );

        self.storage.upload(&batch_filename, &buffer)?;
        tracing::info!(
            "Uploaded Parquet batch {} with {} items",
            batch_filename,
            rows.len()
        );

        // 6. Compact / Cleanup (Move to Base)
        let mut client = self.db.get_client();
        let mut transaction = client.transaction().map_err(|e| {
            crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
        })?;

        for row in &rows {
            let id: String = row.get("id");
            // data string is JSON string. We need JSONB value for query param?
            // Or we can cast $2::jsonb in SQL.
            let data_str: Option<String> = row.get("data");
            let data_val = match data_str {
                Some(s) => serde_json::from_str(&s).unwrap_or(serde_json::Value::Null),
                None => serde_json::Value::Null,
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
            transaction
                .execute(&upsert_base, &[&id_uuid, &data_val])
                .map_err(|e| {
                    crate::error::DBCenterError::QueryError(format!(
                        "Failed to move to base: {}",
                        e
                    ))
                })?;

            let delete_adds = format!("DELETE FROM \"{}_adds\" WHERE id = $1::uuid", table_name);
            transaction
                .execute(&delete_adds, &[&id_uuid])
                .map_err(|e| {
                    crate::error::DBCenterError::QueryError(format!(
                        "Failed to delete from adds: {}",
                        e
                    ))
                })?;
        }

        transaction.commit().map_err(|e| {
            crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
        })?;

        Ok(rows.len())
    }

    /// Task 3.5: Compaction / Vacuum with Heartbeat Lease (Phase 4)
    /// "Merge x_base + x_adds into a new x_base_v2. Upload x_base_v2. Delete old deltas."
    /// Uses lock.json for concurrency control.
    pub fn vacuum(&self, table_name: &str) -> Result<()> {
        tracing::info!("Starting Vacuum for table: {}", table_name);

        let worker_id = uuid::Uuid::new_v4().to_string();
        let lock_key = format!("{}_vacuum.lock", table_name);
        let ttl_ms = 30_000; // 30 seconds

        // 1. Acquire Lock (Spin Wait)
        let mut acquired = false;
        let mut retries = 0;
        let max_retries = 5;

        while retries < max_retries {
            // Try to lock with a short TTL (e.g. 30s)
            if self.storage.lock(&lock_key, 30_000, &worker_id)? {
                acquired = true;
                break;
            }

            // Check if existing lock is stale (optional optimization: storage.lock handles expiry check)
            tracing::warn!(
                "Vacuum lock held by another process. Retrying in 1s... ({}/{})",
                retries + 1,
                max_retries
            );
            std::thread::sleep(std::time::Duration::from_millis(1000));
            retries += 1;
        }

        if !acquired {
            return Err(crate::error::DBCenterError::LockError);
        }

        tracing::info!("Acquired vacuum lock: {}", worker_id);

        // 2. Start Heartbeat Thread
        // We use an AtomicBool to signal the thread to stop.
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));

        // Safety: If heartbeat fails, we MUST stop the main thread from modifying storage
        let lock_lost = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let running_clone = running.clone();
        let lock_lost_clone = lock_lost.clone();
        let storage_clone = self.storage.clone();
        let lock_key_clone = lock_key.clone();
        let worker_id_clone = worker_id.clone();

        std::thread::spawn(move || {
            while running_clone.load(std::sync::atomic::Ordering::SeqCst) {
                // Sleep for 10s (TTL/3) but check for cancellation every 500ms
                for _ in 0..20 {
                    if !running_clone.load(std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }

                if !running_clone.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }

                match storage_clone.refresh_lock(&lock_key_clone, ttl_ms, &worker_id_clone) {
                    Ok(true) => tracing::debug!("Refreshed vacuum lock"),
                    Ok(false) => {
                        tracing::warn!(
                            "Failed to refresh lock (lost ownership?) - Signaling Abort"
                        );
                        lock_lost_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                    Err(e) => {
                        tracing::error!("Error refreshing lock: {} - Signaling Abort", e);
                        lock_lost_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                }
            }
        });

        // Ensure lock is released even if panic happens (Best effort with Drop guard would be better, but try-finally block here)
        let result = (|| -> Result<()> {
            // Check lock before heavy lifting
            if lock_lost.load(std::sync::atomic::Ordering::SeqCst) {
                tracing::error!("Lock lost before vacuum start, aborting.");
                return Err(crate::error::DBCenterError::LockError);
            }

            // 3. Ensure consistency: Pull all pending changes
            // TASK 5.4: VACUUM SAFETY GUARD
            // We force SyncMode::Full to prevent data loss.
            tracing::info!("Vacuum Safety: Enforcing SyncMode::Full...");

            // 3.0 SAFETY CHECK: Disk Space
            // Calculate required space for Base files
            let remote_files = self.storage.list(table_name)?;
            let mut required_bytes: u64 = 0;
            for file in &remote_files {
                if file.contains("_BASE_") {
                    if let Ok(size) = self.storage.size(file) {
                        required_bytes += size;
                    }
                }
            }

            // Allow 2x overhead for merge process
            let needed_space = required_bytes * 2;

            if needed_space > 0 {
                if let Ok(mem_info) = sys_info::disk_info() {
                    // Check free bytes (converting KB to Bytes if needed, crate dependent)
                    // sys_info::disk_info returns DiskInfo { total: u64, free: u64 } in KB
                    let free_bytes = mem_info.free * 1024;
                    if free_bytes < needed_space {
                        tracing::error!(
                            "Vacuum Aborted: Not enough disk space. Needed: {} B, Free: {} B",
                            needed_space,
                            free_bytes
                        );
                        return Err(crate::error::DBCenterError::StorageError(
                            "Not enough disk space for safe vacuum".to_string(),
                        ));
                    }
                    tracing::info!(
                        "Disk Space Verified for Safety: {} MB available.",
                        free_bytes / 1024 / 1024
                    );
                } else {
                    tracing::warn!(
                        "Could not verify disk space: sys_info failed. Proceeding with caution."
                    );
                }
            }

            // 3.1 Execute Force Pull
            self.pull_and_apply(table_name, SyncMode::Full)?;

            // 4. Identify files to delete
            let files_to_delete = self.storage.list(table_name)?;

            // Smart Vacuum: optimization
            // If there are no delta files, it means we only have base files (likely just one).
            // In this case, we don't need to re-upload/compact.
            let has_deltas = files_to_delete.iter().any(|f| f.contains("_DELTA_"));
            if !has_deltas {
                tracing::info!("Smart Vacuum: Storage already clean (no deltas). Skipping upload.");
                return Ok(());
            }

            // 5. Export _base (Postgres) to a new BASE Parquet file
            let sql_base = format!(
                "SELECT id::text, data::text, created_at::text, updated_at::text FROM \"{}_base\" ORDER BY created_at ASC", 
                table_name
            );
            let rows = {
                let mut client = self.db.get_client();
                client.query(&sql_base, &[]).map_err(|e| {
                    crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
                })?
            };

            if rows.is_empty() {
                tracing::info!("Vacuum: Base is empty, nothing to export.");
            }

            // Task 5.2: Optimized Parquet Storage (Sorting)
            // Sort rows by ID to enable efficient Predicate Pushdown (Min/Max pruning)
            let mut sorted_rows = rows.clone();
            sorted_rows.sort_by(|a, b| {
                let id_a: String = a.get("id");
                let id_b: String = b.get("id");
                id_a.cmp(&id_b)
            });

            let batch = self.rows_to_record_batch(sorted_rows)?;

            let mut buffer = Vec::new();
            // Task 5.2: Enable Statistics & Bloom Filters
            let props = WriterProperties::builder()
                .set_statistics_enabled(parquet::file::properties::EnabledStatistics::Chunk)
                .set_bloom_filter_enabled(true)
                // Rows, not bytes. This was 100 * 1024 * 1024, which reads as
                // "100MB" but means 104 million rows - so every realistic file
                // was a single row group, there was nothing for the reader to
                // skip, and a lookup pulled the whole object down. Rows are
                // written in id order, so smaller groups give the statistics
                // something to prune on.
                .set_max_row_group_size(20_000)
                .build();

            let mut writer = ArrowWriter::try_new(&mut buffer, batch.schema(), Some(props))
                .map_err(|e| {
                    crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(
                        e.to_string(),
                    ))
                })?;

            writer.write(&batch).map_err(|e| {
                crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(
                    e.to_string(),
                ))
            })?;
            writer.close().map_err(|e| {
                crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(
                    e.to_string(),
                ))
            })?;

            // CRITICAL CHECK: Before uploading/swapping, verify lock one last time
            if lock_lost.load(std::sync::atomic::Ordering::SeqCst) {
                tracing::error!("Lock lost before upload, aborting vacuum.");
                return Err(crate::error::DBCenterError::LockError);
            }

            // 6. Upload New Base (Atomic Swap)
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let base_filename_final = format!(
                "{}_BASE_{}_{}.parquet",
                table_name,
                timestamp,
                uuid::Uuid::new_v4()
            );
            let base_filename_tmp = format!("{}.tmp", base_filename_final);

            self.storage.upload(&base_filename_tmp, &buffer)?;
            tracing::info!("Uploaded temp base: {}", base_filename_tmp);

            // Check again just before rename (the point of no return)
            if lock_lost.load(std::sync::atomic::Ordering::SeqCst) {
                tracing::error!("Lock lost before rename, aborting.");
                // Try to clean up temp file?
                let _ = self.storage.delete(&base_filename_tmp);
                return Err(crate::error::DBCenterError::LockError);
            }

            self.storage
                .rename(&base_filename_tmp, &base_filename_final)?;
            tracing::info!("Atomic swap complete: {}", base_filename_final);

            // 7. Delete Old Files
            for file in &files_to_delete {
                if file.ends_with(".parquet") {
                    // Don't delete the file we just created! (Though files_to_delete list is from BEFORE creation)
                    // So we are safe.
                    self.storage.delete(file)?;
                }
            }

            tracing::info!(
                "Vacuum Complete. {} old files deleted.",
                files_to_delete.len()
            );
            Ok(())
        })();

        // Signal heartbeat to stop
        running.store(false, std::sync::atomic::Ordering::SeqCst);

        // Release Lock
        if let Err(e) = self.storage.unlock(&lock_key, &worker_id) {
            tracing::warn!("Failed to release vacuum lock: {}", e);
        } else {
            tracing::info!("Released vacuum lock");
        }

        result
    }

    /// Task 3.3 & 5.1: Conflict Resolution (Parquet Download) & Lazy Sync
    pub fn pull_and_apply(&self, table_name: &str, mode: SyncMode) -> Result<()> {
        // Ensure table structure exists before trying to insert to _base
        use crate::delta_manager::DeltaManager;
        DeltaManager::ensure_delta_tables(&self.db, table_name).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!("Failed to ensure tables: {}", e))
        })?;

        let files = self.storage.list(table_name)?;

        let mut sorted_files = files.clone();
        sorted_files.sort();
        let mut added_columns = false;

        for file in sorted_files {
            if !file.ends_with(".parquet") {
                continue;
            }

            // Task 5.1: Lazy Sync Logic
            if mode == SyncMode::Partial && file.contains("_BASE_") {
                tracing::debug!("Lazy Sync: Skipping BASE file download: {}", file);
                continue;
            }

            // Ask storage how big the object is. Downloading the whole thing to
            // call .len() on it defeats the reader that is about to fetch only
            // the ranges it needs - and then fetches them a second time.
            let file_len = self.storage.size(&file)?;
            if file_len == 0 {
                continue;
            }

            let remote_reader =
                RemoteParquetReader::new(self.storage.clone(), file.clone(), file_len);

            let builder = ParquetRecordBatchReaderBuilder::try_new(remote_reader).map_err(|e| {
                crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(
                    e.to_string(),
                ))
            })?;

            let reader = builder.build().map_err(|e| {
                crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(
                    e.to_string(),
                ))
            })?;

            let mut client = self.db.get_client();
            let mut transaction = client.transaction().map_err(|e| {
                crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
            })?;

            for batch_result in reader {
                let batch = batch_result.map_err(|e| {
                    crate::error::DBCenterError::SerializationError(serde::ser::Error::custom(
                        e.to_string(),
                    ))
                })?;

                let ids = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap();
                let datas = batch
                    .column(1)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap();
                // col 2 is created_at, col 3 is updated_at
                let updated_ats = batch
                    .column(3)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap();

                for i in 0..batch.num_rows() {
                    let id_str = ids.value(i);
                    let data_str = if datas.is_null(i) {
                        "{}"
                    } else {
                        datas.value(i)
                    };
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

                    let data_json: serde_json::Value =
                        serde_json::from_str(data_str).unwrap_or(serde_json::Value::Null);

                    // Put the document back where the collection API expects it:
                    // one column per field. Restoring only into `data` left the
                    // fields nested under it, so a round trip through storage
                    // came back in a shape find() does not produce.
                    let fields = data_json.as_object().cloned().unwrap_or_default();
                    let base_table = format!("{}_base", table_name);

                    let mut columns = vec![
                        "id".to_string(),
                        "data".to_string(),
                        "updated_at".to_string(),
                    ];
                    let mut values = vec![
                        "$1::uuid".to_string(),
                        "$2".to_string(),
                        "$3::text::timestamptz".to_string(),
                    ];
                    let mut assignments = vec![
                        "data = EXCLUDED.data".to_string(),
                        "updated_at = EXCLUDED.updated_at".to_string(),
                    ];

                    for (key, value) in &fields {
                        if matches!(key.as_str(), "id" | "data" | "created_at" | "updated_at") {
                            continue;
                        }
                        if ensure_restored_column(&mut transaction, table_name, key, value)? {
                            added_columns = true;
                        }
                        let quoted = crate::query::quote_ident(key);
                        columns.push(quoted.clone());
                        values.push(crate::query::QueryBuilder::json_to_sql_literal(value));
                        assignments.push(format!("{q} = EXCLUDED.{q}", q = quoted));
                    }

                    // LWW: Update only if incoming (EXCLUDED) is newer than existing
                    // We cast $3 to text first, then timestamptz, to accept String input from Rust.
                    let upsert_base = format!(
                        r#"
                        INSERT INTO "{}" ({})
                        VALUES ({})
                        ON CONFLICT (id) DO UPDATE SET {}
                        WHERE EXCLUDED.updated_at > "{}".updated_at
                        "#,
                        base_table,
                        columns.join(", "),
                        values.join(", "),
                        assignments.join(", "),
                        base_table
                    );

                    let id_uuid = uuid::Uuid::parse_str(id_str).unwrap();
                    transaction
                        .execute(&upsert_base, &[&id_uuid, &data_json, &updated_at_val])
                        .map_err(|e| {
                            crate::error::DBCenterError::QueryError(format!(
                                "Failed to apply parquet row to base: {:?}",
                                e
                            ))
                        })?;
                }
            }

            transaction.commit().map_err(|e| {
                crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
            })?;
        }

        // PostgreSQL freezes `SELECT *` when a view is created, so columns added
        // during the restore are invisible through the collection until the view
        // is rebuilt. Do it after the client lock is released - ensure_delta_tables
        // takes it itself.
        if added_columns {
            DeltaManager::ensure_delta_tables(&self.db, table_name)?;
        }

        Ok(())
    }

    /// Task 5.3: Smart Scan (Predicate Pushdown)
    /// Finds the latest BASE file and executes a smart scan on it using RemoteFile.
    /// Look one id up in object storage, reading as little as possible.
    ///
    /// Every Parquet object for the table is consulted, not only the newest base
    /// snapshot: a compaction writes `<table>_BASE_...` and each incremental push
    /// writes `<table>_DELTA_...`, so reading only the base silently misses
    /// everything written since the last vacuum. Later objects win, which matches
    /// how the rows were applied locally.
    ///
    /// Row groups whose id range cannot contain the target are skipped without
    /// being fetched. See `tests/lazy_read.rs` for what that saves.
    pub fn scan_smart(
        &self,
        table_name: &str,
        target_id: &str,
    ) -> crate::error::Result<Vec<serde_json::Value>> {
        use crate::vfs::remote_file::RemoteFile;
        use crate::vfs::smart_reader::SmartParquetReader;

        // Names carry a unix timestamp, so sorting them puts the base first and
        // the deltas after it in the order they were written.
        let mut objects: Vec<String> = self
            .storage
            .list(table_name)?
            .into_iter()
            .filter(|f| f.ends_with(".parquet") && (f.contains("_BASE_") || f.contains("_DELTA_")))
            .collect();
        objects.sort();

        let meta_dir = self.db.get_path().join(".meta");
        std::fs::create_dir_all(&meta_dir).ok();

        let mut newest: Option<serde_json::Value> = None;

        for key in objects {
            let size = self.storage.size(&key).unwrap_or(0);
            if size == 0 {
                continue;
            }

            let remote_file = RemoteFile::new(
                self.storage.clone(),
                key.clone(),
                size,
                5 * 1024 * 1024,
                Some(meta_dir.join(format!("{}.meta", key.replace('/', "_")))),
            );

            let reader = SmartParquetReader::new(remote_file)?;
            for row in reader.scan_for_id(target_id)? {
                newest = Some(row_to_document(&row));
            }
        }

        Ok(newest.into_iter().collect())
    }
}
