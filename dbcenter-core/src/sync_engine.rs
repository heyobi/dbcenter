use crate::error::Result;
use crate::storage::StorageAdapter;
use crate::vfs::RemoteParquetReader;
use crate::DBCenter;
use arrow::array::{
    Array, ArrayRef, BooleanArray, BooleanBuilder, Float64Array, Float64Builder, Int64Array,
    Int64Builder, StringArray, StringBuilder,
};
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

/// One table column, and how it is written to Parquet.
///
/// Everything used to be flattened into a single `data` column holding the
/// document as a JSON string, which left Parquet with nothing to be columnar
/// about: a query that wanted one field still read every byte of every document
/// and parsed JSON out of it. Measured over five million rows, that made reads
/// off object storage 1.8x *slower* than the PostgreSQL they were meant to beat,
/// at 2.1x the stored bytes. One column per field is what makes the format do
/// its job - see docs/columnar-reads.md for the numbers.
struct SyncColumn {
    name: String,
    arrow: DataType,
    /// How the column is read out of PostgreSQL, already quoted.
    select: String,
}

impl SyncColumn {
    /// Map a PostgreSQL type to the Arrow type it is written as.
    ///
    /// Only the types the collection API actually produces are given a native
    /// Arrow type: document fields become BIGINT, DOUBLE PRECISION, BOOLEAN,
    /// TEXT or JSONB, depending on what the JSON value was. Everything else -
    /// uuid, timestamps, numeric, arrays, and any type a user made by hand -
    /// goes to text, which is lossless and still gets column pruning,
    /// dictionary compression and row-group statistics. Timestamps in
    /// particular lose nothing by it: ISO-8601 sorts in the same order as the
    /// instant it names, so a range filter can still skip row groups unread.
    fn plan(name: &str, udt: &str) -> Self {
        let quoted = crate::query::quote_ident(name);
        let (arrow, select) = match udt {
            "int2" | "int4" | "int8" => (DataType::Int64, format!("{}::bigint", quoted)),
            "float4" | "float8" => (DataType::Float64, format!("{}::double precision", quoted)),
            "bool" => (DataType::Boolean, quoted.clone()),
            _ => (DataType::Utf8, format!("{}::text", quoted)),
        };
        SyncColumn {
            name: name.to_string(),
            arrow,
            select: format!("{} AS {}", select, quoted),
        }
    }
}

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
/// This reads the same shape [`arrow_row_to_document`] does - one column per
/// document field - through the lower-level record API the smart reader uses.
/// A `data` column holding a JSON object is expanded, which is how files
/// written by earlier versions, and the geo layout, still read back.
fn row_to_document(row: &parquet::record::Row) -> serde_json::Value {
    use parquet::record::Field;
    use serde_json::Value;

    let mut doc = serde_json::Map::new();
    let mut nested: Option<serde_json::Map<String, Value>> = None;

    for (name, value) in row.get_column_iter() {
        // Values carry their type here, so a bigint comes back a JSON number
        // rather than a string of digits, which is what the document had.
        let json = match value {
            Field::Null => continue,
            Field::Bool(b) => Value::from(*b),
            Field::Byte(v) => Value::from(*v),
            Field::Short(v) => Value::from(*v),
            Field::Int(v) => Value::from(*v),
            Field::Long(v) => Value::from(*v),
            Field::UByte(v) => Value::from(*v),
            Field::UShort(v) => Value::from(*v),
            Field::UInt(v) => Value::from(*v),
            Field::ULong(v) => Value::from(*v),
            Field::Float(v) => Value::from(*v),
            Field::Double(v) => Value::from(*v),
            Field::Str(text) => Value::from(text.as_str()),
            other => Value::from(other.to_string()),
        };

        if name == "data" {
            if let Value::String(text) = &json {
                if let Ok(Value::Object(map)) = serde_json::from_str(text) {
                    nested = Some(map);
                    continue;
                }
            }
        }
        doc.insert(name.to_string(), json);
    }

    if let Some(map) = nested {
        for (k, v) in map {
            doc.entry(k).or_insert(v);
        }
    }

    if let Some(id) = doc.get("id").cloned() {
        doc.insert("_id".to_string(), id);
    }
    Value::Object(doc)
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

/// Turn one row of an Arrow batch into the document it came from.
///
/// Columns are read by name and Arrow type rather than by position. The old
/// files had a fixed four-column shape, so position was enough; now that a file
/// carries one column per document field, position means nothing and a file
/// written before a field existed simply has fewer columns.
///
/// A `data` column holding a JSON object is expanded into the document, which
/// is both how files written by earlier versions are read and how the geo and
/// raw-JSONB layouts still store their contents.
fn arrow_row_to_document(
    batch: &RecordBatch,
    i: usize,
) -> (
    serde_json::Map<String, serde_json::Value>,
    serde_json::Value,
) {
    use serde_json::Value;
    let mut doc = serde_json::Map::new();
    let mut data = Value::Null;

    for (idx, field) in batch.schema().fields().iter().enumerate() {
        let column = batch.column(idx);
        if column.is_null(i) {
            continue;
        }
        let value = match field.data_type() {
            DataType::Int64 => column
                .as_any()
                .downcast_ref::<Int64Array>()
                .map(|a| Value::from(a.value(i))),
            DataType::Float64 => column
                .as_any()
                .downcast_ref::<Float64Array>()
                .map(|a| Value::from(a.value(i))),
            DataType::Boolean => column
                .as_any()
                .downcast_ref::<BooleanArray>()
                .map(|a| Value::from(a.value(i))),
            _ => column
                .as_any()
                .downcast_ref::<StringArray>()
                .map(|a| Value::from(a.value(i))),
        };
        let Some(value) = value else { continue };

        if field.name() == "data" {
            // Kept as it was found rather than folded into the document. The
            // geo layout genuinely stores its contents here, and a file written
            // before this was columnar has the whole document here - both want
            // `data` put back as `data`.
            data = match &value {
                Value::String(text) => serde_json::from_str(text).unwrap_or(value.clone()),
                _ => value,
            };
            continue;
        }
        doc.insert(field.name().to_string(), value);
    }

    // A file from before the columnar change, or a geo row, carries the
    // document inside `data`. Those fields are restored as columns too, so the
    // collection API finds them where it puts them.
    if let Value::Object(map) = &data {
        for (k, v) in map {
            doc.entry(k.clone()).or_insert(v.clone());
        }
    }

    (doc, data)
}

/// Escape a value for a single-quoted SQL literal.
///
/// Object keys and endpoints come from configuration rather than from a user's
/// query, but they still end up inside a CREATE VIEW that cannot take
/// parameters, so they are escaped rather than trusted.
fn quote_sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}

/// The PostgreSQL type a Parquet column is surfaced as through the view.
fn pg_type_for(arrow: &DataType) -> &'static str {
    match arrow {
        DataType::Int64 => "bigint",
        DataType::Float64 => "double precision",
        DataType::Boolean => "boolean",
        _ => "text",
    }
}

impl SyncEngine {
    pub fn new(db: Arc<DBCenter>, storage: Arc<Box<dyn StorageAdapter + Send + Sync>>) -> Self {
        Self { db, storage }
    }

    /// The columns a table's rows are written as, in table order.
    ///
    /// Read from the catalog rather than assumed, because the collection API
    /// adds a column the first time a document carries a new field - so the
    /// shape of a table is whatever its documents have made it.
    fn sync_columns(&self, table: &str) -> Result<Vec<SyncColumn>> {
        let mut client = self.db.get_client();
        let rows = client
            .query(
                "SELECT column_name, udt_name FROM information_schema.columns \
                 WHERE table_name = $1 ORDER BY ordinal_position",
                &[&table],
            )
            .map_err(|e| {
                crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
            })?;

        let columns: Vec<SyncColumn> = rows
            .iter()
            .map(|r| {
                let name: String = r.get("column_name");
                let udt: String = r.get("udt_name");
                SyncColumn::plan(&name, &udt)
            })
            .collect();

        if columns.is_empty() {
            return Err(crate::error::DBCenterError::QueryError(format!(
                "Table '{}' has no columns to sync",
                table
            )));
        }
        Ok(columns)
    }

    /// Turn PostgreSQL rows into an Arrow batch with one column per table column.
    fn rows_to_record_batch(&self, columns: &[SyncColumn], rows: Vec<Row>) -> Result<RecordBatch> {
        let schema = Arc::new(Schema::new(
            columns
                .iter()
                .map(|c| Field::new(&c.name, c.arrow.clone(), true))
                .collect::<Vec<_>>(),
        ));

        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(columns.len());
        for (idx, column) in columns.iter().enumerate() {
            let array: ArrayRef = match column.arrow {
                DataType::Int64 => {
                    let mut b = Int64Builder::with_capacity(rows.len());
                    for row in &rows {
                        b.append_option(row.get::<_, Option<i64>>(idx));
                    }
                    Arc::new(b.finish())
                }
                DataType::Float64 => {
                    let mut b = Float64Builder::with_capacity(rows.len());
                    for row in &rows {
                        b.append_option(row.get::<_, Option<f64>>(idx));
                    }
                    Arc::new(b.finish())
                }
                DataType::Boolean => {
                    let mut b = BooleanBuilder::with_capacity(rows.len());
                    for row in &rows {
                        b.append_option(row.get::<_, Option<bool>>(idx));
                    }
                    Arc::new(b.finish())
                }
                _ => {
                    let mut b = StringBuilder::new();
                    for row in &rows {
                        b.append_option(row.get::<_, Option<String>>(idx));
                    }
                    Arc::new(b.finish())
                }
            };
            arrays.push(array);
        }

        RecordBatch::try_new(schema, arrays).map_err(|e| {
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

        // 2. Fetch rows, one Parquet column per table column.
        //
        // This used to collapse the whole document into a single JSON string,
        // which is what a document store does and exactly what Parquet is not
        // for. The columns are read from the catalog because the collection API
        // grows them as documents introduce fields.
        let columns = self.sync_columns(&adds_table)?;
        let sql_fetch = format!(
            r#"SELECT {} FROM "{}" t ORDER BY created_at ASC LIMIT {}"#,
            columns
                .iter()
                .map(|c| c.select.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            adds_table,
            limit
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
        // The ids are taken before the batch is built so `rows` can be moved into
        // it rather than cloned. Cloning held a second copy of the whole sync in
        // memory, which a million-row sync could not afford.
        let ids: Vec<uuid::Uuid> = rows
            .iter()
            .map(|row| {
                let id: String = row.get("id");
                uuid::Uuid::parse_str(&id).map_err(|e| {
                    crate::error::DBCenterError::QueryError(format!(
                        "Row in '{}' has an id that is not a uuid ({}): {}",
                        adds_table, id, e
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let row_count = rows.len();

        let batch = self.rows_to_record_batch(&columns, rows)?;

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
            row_count
        );

        // 6. Compact / Cleanup (Move to Base)
        //
        // Every column, not just (id, data, created_at, updated_at). Listing
        // four columns here dropped the document on the way from _adds to
        // _base: the fields a document was inserted with live in the row's own
        // columns, so a synced row arrived in _base with its id and timestamps
        // and nothing else. The Parquet was written correctly and the local
        // copy was emptied, which is the more dangerous direction of the two.
        //
        // Doing the move in SQL also means one statement instead of two per
        // row, and no round trip through JSON to get there.
        let mut client = self.db.get_client();
        let mut transaction = client.transaction().map_err(|e| {
            crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
        })?;

        // _base and _adds are created together and altered together, so their
        // column lists match and the names can be reused for both sides.
        let names: Vec<String> = columns
            .iter()
            .map(|c| crate::query::quote_ident(&c.name))
            .collect();
        let assignments: Vec<String> = names
            .iter()
            .filter(|n| n.as_str() != "\"id\"")
            .map(|n| format!("{n} = EXCLUDED.{n}"))
            .collect();

        let base_table = format!("{}_base", table_name);
        let move_to_base = format!(
            r#"WITH moved AS (
                   DELETE FROM "{adds}" WHERE id = ANY($1) RETURNING *
               )
               INSERT INTO "{base}" ({cols})
               SELECT {cols} FROM moved
               ON CONFLICT (id) DO UPDATE SET {assign}"#,
            adds = adds_table,
            base = base_table,
            cols = names.join(", "),
            assign = assignments.join(", "),
        );

        // In chunks, because the id list travels as one bind parameter and a
        // sync of a million rows put a million uuids in a single message:
        // PostgreSQL ran out of memory building it ("Failed on request of size
        // 40 in memory context MessageContext"). One statement per batch is
        // still far fewer than the two per row this replaced.
        // Updating a row marks its id in _dels, because that is how the view
        // hides the version still sitting in _base. Once the new version has
        // been moved into _base the marker has to go with it, or the view goes
        // on hiding a row that is now the current one - an update followed by a
        // sync made the row disappear from the collection entirely.
        //
        // Only for the ids being moved. An id in _dels with nothing in _adds is
        // a genuine deletion and must stay.
        let clear_dels = format!(r#"DELETE FROM "{}_dels" WHERE id = ANY($1)"#, table_name);

        const MOVE_CHUNK: usize = 10_000;
        for chunk in ids.chunks(MOVE_CHUNK) {
            transaction.execute(&move_to_base, &[&chunk]).map_err(|e| {
                crate::error::DBCenterError::QueryError(format!(
                    "Failed to move synced rows to base: {}",
                    crate::utils::format_postgres_error(&e)
                ))
            })?;
            transaction.execute(&clear_dels, &[&chunk]).map_err(|e| {
                crate::error::DBCenterError::QueryError(format!(
                    "Failed to clear delete markers for synced rows: {}",
                    crate::utils::format_postgres_error(&e)
                ))
            })?;
        }

        transaction.commit().map_err(|e| {
            crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
        })?;

        // A new delta object changes which deduplication the view needs.
        self.refresh_columnar_view(table_name);

        Ok(row_count)
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
            // The same one-column-per-field shape the incremental path writes.
            // Selecting only (id, data, created_at, updated_at) here had the
            // second half of the bug that cost the delta files their contents:
            // for a table written through the collection API the document lives
            // in the row's own columns and `data` is null, so a compaction
            // replaced every delta with a base file holding ids and timestamps
            // and no document at all.
            let base_table = format!("{}_base", table_name);
            let base_columns = self.sync_columns(&base_table)?;
            let sql_base = format!(
                r#"SELECT {} FROM "{}" ORDER BY created_at ASC"#,
                base_columns
                    .iter()
                    .map(|c| c.select.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                base_table
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

            let batch = self.rows_to_record_batch(&base_columns, sorted_rows)?;

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

            // A compaction replaces the deltas with a single base, which is the
            // largest change the view's definition can see.
            self.refresh_columnar_view(table_name);
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

                for i in 0..batch.num_rows() {
                    let (mut fields, data_json) = arrow_row_to_document(&batch, i);

                    let Some(serde_json::Value::String(id_str)) = fields.remove("id") else {
                        // A row with no id cannot be upserted into _base, and
                        // silently dropping it would be worse than saying so.
                        return Err(crate::error::DBCenterError::QueryError(format!(
                            "Parquet file '{}' has a row with no id column",
                            file
                        )));
                    };

                    let updated_at_val = match fields.remove("updated_at") {
                        Some(serde_json::Value::String(t)) if !t.trim().is_empty() => t,
                        _ => chrono::Utc::now().naive_utc().to_string(),
                    };
                    fields.remove("created_at");

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

                    let id_uuid = uuid::Uuid::parse_str(&id_str).map_err(|e| {
                        crate::error::DBCenterError::QueryError(format!(
                            "Parquet file '{}' has an id that is not a uuid ({}): {}",
                            file, id_str, e
                        ))
                    })?;
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

    /// Rebuild the columnar view, but only if there is already one to rebuild.
    ///
    /// The view has to be regenerated whenever the set of Parquet objects
    /// changes, because which deduplication it needs depends on that set: one
    /// built when there was a single delta does no deduplication, which stops
    /// being true the moment a second delta arrives carrying an updated row.
    /// A stale view is not a slow view, it is a wrong one.
    ///
    /// Silent when pg_duckdb is not installed or no view has been published:
    /// this runs after every sync, and a database that never asked for the
    /// columnar path should not pay for it or hear about it.
    fn refresh_columnar_view(&self, table_name: &str) {
        let view = format!("{}_columnar", table_name);
        let exists = {
            let mut client = self.db.get_client();
            client
                .query_one(
                    "SELECT EXISTS (SELECT 1 FROM pg_views WHERE viewname = $1)",
                    &[&view],
                )
                .map(|row| row.get::<_, bool>(0))
                .unwrap_or(false)
        };
        if !exists {
            return;
        }
        if let Err(e) = self.ensure_columnar_view(table_name) {
            // Worth saying out loud: the view is still there and still
            // answering, with a definition that may no longer match what is in
            // storage.
            tracing::warn!(
                "Could not refresh the columnar view for '{}', so it may be \
                 reading the wrong set of files: {}",
                table_name,
                e
            );
        }
    }

    /// Publish a view that reads this table's Parquet directly, through DuckDB.
    ///
    /// This is the read advantage made reachable. The collection's own view
    /// stays pure PostgreSQL - point lookups there are 0.2 ms against 38 ms
    /// off object storage, and a view that unioned the two would send every
    /// query through DuckDB and cost 550x on the lookups (docs/columnar-reads.md).
    /// So the columnar path gets its own name, `<table>_columnar`, holding the
    /// same rows read the fast way: 12.7x PostgreSQL on aggregates, over data
    /// PostgreSQL never has to hold.
    ///
    /// From a client it is an ordinary view with ordinary columns. `read_parquet`
    /// hands DuckDB's untyped `r['col']` accessor back to PostgreSQL, and hiding
    /// that behind the view is what lets someone connected with a plain
    /// connection string write plain SQL against it.
    ///
    /// Requires the pg_duckdb extension; install it with
    /// `install_extension("pg_duckdb")` first.
    pub fn ensure_columnar_view(&self, table_name: &str) -> Result<()> {
        let Some(access) = self.storage.duckdb_access() else {
            return Err(crate::error::DBCenterError::StorageError(
                "This storage backend cannot be read by DuckDB directly, so there \
                 is no columnar view to publish for it."
                    .to_string(),
            ));
        };

        let base_table = format!("{}_base", table_name);
        let columns = self.sync_columns(&base_table)?;

        let mut client = self.db.get_client();

        // The secret is per-connection state in DuckDB, so it is (re)declared
        // here rather than once at startup - a pooled connection that was
        // opened later would otherwise have no credentials.
        if let Some(creds) = &access.credentials {
            client
                .execute(
                    "SELECT duckdb.create_simple_secret(type := 'S3', key_id := $1, \
                     secret := $2, region := $3, endpoint := $4, url_style := $5, \
                     use_ssl := $6)",
                    &[
                        &creds.key_id,
                        &creds.secret,
                        &creds.region,
                        &creds.endpoint,
                        &creds.url_style.to_string(),
                        &creds.use_ssl.to_string(),
                    ],
                )
                .map_err(|e| {
                    crate::error::DBCenterError::StorageError(format!(
                        "Could not give DuckDB the object storage credentials: {}",
                        crate::utils::format_postgres_error(&e)
                    ))
                })?;
        }

        // `union_by_name` is what makes a file written before a field existed
        // readable beside one written after: the files genuinely have different
        // columns, because the table grows a column the first time a document
        // carries a field.
        let objects: Vec<String> = self
            .storage
            .list(table_name)?
            .into_iter()
            .filter(|f| f.ends_with(".parquet"))
            .collect();
        let has_base = objects.iter().any(|f| f.contains("_BASE_"));
        let delta_count = objects.iter().filter(|f| f.contains("_DELTA_")).count();

        if !has_base && delta_count == 0 {
            return Err(crate::error::DBCenterError::StorageError(format!(
                "Nothing has been synced for '{}' yet, so there is nothing for a \
                 columnar view to read. Call sync_table first.",
                table_name
            )));
        }

        let base_glob =
            quote_sql_literal(&access.url_for(&format!("{}_BASE_*.parquet", table_name)));
        let delta_glob =
            quote_sql_literal(&access.url_for(&format!("{}_DELTA_*.parquet", table_name)));

        let projection = columns
            .iter()
            .map(|c| {
                format!(
                    "r['{}']::{} AS {}",
                    c.name.replace('\'', "''"),
                    pg_type_for(&c.arrow),
                    crate::query::quote_ident(&c.name)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let names = columns
            .iter()
            .map(|c| crate::query::quote_ident(&c.name))
            .collect::<Vec<_>>()
            .join(", ");

        // What it takes for the same id to appear twice.
        //
        // Within one file it cannot: the rows come from _adds, which has id as
        // its primary key. It takes an update to a row that was already synced,
        // which writes it to _adds again and puts it in a later file. So a
        // single delta file needs no deduplication at all, and several of them
        // need it only among themselves.
        //
        // This matters because deduplication is not cheap. Ranking every row -
        // one window over every file - costs 12.9x on an aggregate, because
        // ranking five million rows means sorting them and that is most of the
        // query. Ranking only the deltas and anti-joining their ids out of the
        // compacted base says the same thing for 2.3x, and saying nothing at
        // all, when nothing can be duplicated, costs nothing.
        let plain_base = format!(
            "SELECT {projection} FROM read_parquet('{base_glob}', union_by_name => true) r"
        );
        let plain_deltas = format!(
            "SELECT {projection} FROM read_parquet('{delta_glob}', union_by_name => true) r"
        );
        let deltas = if delta_count > 1 {
            format!(
                "SELECT {names} FROM (SELECT {projection}, row_number() OVER (\
                 PARTITION BY r['id'] ORDER BY r['updated_at'] DESC) AS rn \
                 FROM read_parquet('{delta_glob}', union_by_name => true) r) ranked WHERE rn = 1"
            )
        } else {
            plain_deltas
        };

        let body = match (has_base, delta_count) {
            (true, 0) => plain_base,
            (false, _) => deltas,
            _ => format!(
                "SELECT {projection} FROM read_parquet('{base_glob}', union_by_name => true) r \
                 WHERE NOT EXISTS (SELECT 1 FROM read_parquet('{delta_glob}', union_by_name => true) d \
                 WHERE d['id'] = r['id']) \
                 UNION ALL {deltas}"
            ),
        };

        let view = crate::query::quote_ident(&format!("{}_columnar", table_name));
        let sql = format!("CREATE OR REPLACE VIEW {view} AS {body}");

        client.batch_execute(&sql).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!(
                "Could not create the columnar view for '{}': {}",
                table_name,
                crate::utils::format_postgres_error(&e)
            ))
        })?;

        tracing::info!(
            "Columnar view {}_columnar reads {}",
            table_name,
            access.url_for(&format!("{}_*.parquet", table_name))
        );
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
