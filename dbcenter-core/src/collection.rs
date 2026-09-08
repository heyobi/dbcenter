use crate::error::Result;
use crate::query::{quote_ident, QueryBuilder};
use crate::DBCenter;
use serde_json::Value as JsonValue;
use std::collections::HashMap;

/// Collection wrapper - MongoDB-style API for PostgreSQL
pub struct Collection<'a> {
    db: &'a DBCenter,
    name: String,
}

impl<'a> Collection<'a> {
    /// The collection's table name, safely quoted for interpolation into SQL.
    fn table(&self) -> String {
        quote_ident(&self.name)
    }

    pub fn new(db: &'a DBCenter, name: &str) -> Self {
        let collection = Self {
            db,
            name: name.to_string(),
        };

        // Ensure table exists
        if let Err(e) = collection.ensure_table() {
            tracing::error!("Failed to create table '{}': {}", name, e);
        }

        collection
    }

    /// Ensure base table exists (Relational Model)
    fn ensure_table(&self) -> Result<()> {
        // Phase 2: Delta Layer
        if self.db.config().multi_user_sync_mode {
            return crate::delta_manager::DeltaManager::ensure_delta_tables(self.db, &self.name);
        }

        let sql = format!(
            r#"
            CREATE TABLE IF NOT EXISTS {} (
                id SERIAL PRIMARY KEY,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            "#,
            self.table()
        );

        let mut client = self.db.get_client();
        client.batch_execute(&sql).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!(
                "Table creation failed: {}",
                crate::utils::format_postgres_error(&e)
            ))
        })?;

        Ok(())
    }

    /// Check and create missing columns based on the document structure
    fn ensure_columns(&self, document: &HashMap<String, JsonValue>) -> Result<()> {
        // Delta Logic: Apply to _base and _adds
        let delta_mode = self.db.config().multi_user_sync_mode;

        let target_tables = if delta_mode {
            vec![format!("{}_base", self.name), format!("{}_adds", self.name)]
        } else {
            vec![self.name.clone()]
        };

        let mut client = self.db.get_client();

        // We need to sync schema for ALL target tables
        for table in target_tables {
            // 1. Which columns the table has. Asking information_schema costs
            //    about 3ms - seventeen times what the insert itself costs inside
            //    a transaction - and the answer only changes when we change it.
            let mut existing_cols = match self.db.cached_columns(&table) {
                Some(cached) => cached,
                None => {
                    let sql_cols =
                        "SELECT column_name FROM information_schema.columns WHERE table_name = $1";
                    let rows = client.query(sql_cols, &[&table]).map_err(|e| {
                        crate::error::DBCenterError::QueryError(
                            crate::utils::format_postgres_error(&e),
                        )
                    })?;
                    let cols: std::collections::HashSet<String> =
                        rows.iter().map(|r| r.get(0)).collect();
                    self.db.remember_columns(&table, cols.clone());
                    cols
                }
            };

            // 2. Identify new columns
            let mut alter_stmts = Vec::new();
            for (key, value) in document {
                if key == "_id" || key == "id" || key == "created_at" || key == "updated_at" {
                    continue;
                }
                // In delta mode the row body lives in a reserved `data` JSONB column,
                // so a user field of that name would collide. In relational mode a
                // field named "data" is an ordinary column like any other.
                if delta_mode && key == "data" {
                    continue;
                }

                if !existing_cols.contains(key) {
                    let col_type = match value {
                        JsonValue::Bool(_) => "BOOLEAN",
                        JsonValue::Number(n) => {
                            if n.is_i64() {
                                "BIGINT"
                            } else {
                                "DOUBLE PRECISION"
                            }
                        }
                        JsonValue::String(_) => "TEXT",
                        JsonValue::Array(_) | JsonValue::Object(_) => "JSONB",
                        JsonValue::Null => "TEXT", // Default to text for nulls
                    };
                    alter_stmts.push(format!(
                        "ADD COLUMN IF NOT EXISTS {} {}",
                        quote_ident(key),
                        col_type
                    ));
                }
            }

            if !alter_stmts.is_empty() {
                let sql_alter = format!(
                    "ALTER TABLE {} {}",
                    quote_ident(&table),
                    alter_stmts.join(", ")
                );
                tracing::info!("Evolving schema for {}: {}", table, sql_alter);
                if let Err(e) = client.batch_execute(&sql_alter) {
                    // What the cache believed is no longer reliable.
                    self.db.forget_columns(&table);
                    return Err(crate::error::DBCenterError::QueryError(format!(
                        "Schema evolution failed: {}",
                        crate::utils::format_postgres_error(&e)
                    )));
                }

                for key in document.keys() {
                    existing_cols.insert(key.clone());
                }
                self.db.remember_columns(&table, existing_cols);
            }
        }

        // Special handling for Delta Views:
        // When we add columns to base/adds, the VIEW (SELECT * ...) might need recreating
        // depending on Postgres version/behavior (early versions didn't auto-update views on SELECT *).
        // Postgres views generally capture the * at creation time.
        // So we MUST recreate the Delta View if we added columns.
        // Drop client to release Mutex lock before calling ensure_delta_tables (which acquires lock)
        drop(client);

        if delta_mode {
            // Simply call ensure_delta_tables again? It does CREATE OR REPLACE VIEW.
            crate::delta_manager::DeltaManager::ensure_delta_tables(self.db, &self.name)?;
        }

        Ok(())
    }

    /// Insert a single document (Relational)
    pub fn insert_one(&self, document: &JsonValue) -> Result<String> {
        let obj = document.as_object().ok_or_else(|| {
            crate::error::DBCenterError::ValidationError(
                "Document must be a JSON object".to_string(),
            )
        })?;

        // 1. Ensure columns exist
        // Convert Map<String, Value> back to HashMap because verify logic used it.
        // Or just use the serde map directly.
        let mut map: HashMap<String, JsonValue> = HashMap::new();
        for (k, v) in obj {
            map.insert(k.clone(), v.clone());
        }

        self.ensure_columns(&map)?;

        // 2. Build Insert SQL
        let mut cols = Vec::new();
        let mut vals = Vec::new();

        for (k, v) in map {
            if k == "_id" || k == "id" {
                continue;
            } // Auto-generated ID
            cols.push(quote_ident(&k));
            vals.push(QueryBuilder::json_to_sql_literal(&v));
        }

        let sql = format!(
            r#"INSERT INTO {} ({}) VALUES ({}) RETURNING id::text"#,
            self.table(),
            cols.join(", "),
            vals.join(", ")
        );

        tracing::debug!("Executing Relational Insert: {}", sql);

        let mut client = self.db.get_client();
        let row = client.query_one(&sql, &[]).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!(
                "Insert failed: {}",
                crate::utils::format_postgres_error(&e)
            ))
        })?;

        Ok(row.get(0))
    }

    /// Insert multiple documents (Relational Batch)
    pub fn insert_many(&self, documents: &[JsonValue]) -> Result<Vec<String>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }

        // 1. Analyze ALL documents to find the superset of columns
        let mut all_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Also keep a sample value type for each key to determine column type
        let mut schema_sample: HashMap<String, JsonValue> = HashMap::new();

        for doc in documents {
            if let Some(obj) = doc.as_object() {
                for (k, v) in obj {
                    if k != "_id" && k != "id" {
                        all_keys.insert(k.clone());
                        if !schema_sample.contains_key(k) && !v.is_null() {
                            schema_sample.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
        }

        // 2. Ensure Schema
        self.ensure_columns(&schema_sample)?;

        // 3. Batch Insert
        // We can't use a single big VALUES (...) list easily if rows have different columns populated.
        // We must ensure every row provides a value for every column in 'all_keys' (or DEFAULT/NULL).
        // To optimize, we'll sort columns.

        let mut sorted_cols: Vec<String> = all_keys.into_iter().collect();
        sorted_cols.sort();
        let cols_sql: Vec<String> = sorted_cols.iter().map(|c| quote_ident(c)).collect();
        let col_names_sql = cols_sql.join(", ");

        let mut all_ids = Vec::new();
        let mut client = self.db.get_client();

        // Postgres has a parameter limit (65535). With N columns, batch size limit is approx 65000/N.
        // But we are constructing raw SQL strings for values (using json_to_sql_literal), so no param limit.
        // Still, avoid massive SQL strings. Chunk it.

        for chunk in documents.chunks(500) {
            let mut values_list = Vec::new();
            for doc in chunk {
                let obj = doc.as_object().ok_or_else(|| {
                    crate::error::DBCenterError::ValidationError(
                        "Every document must be a JSON object".to_string(),
                    )
                })?;
                let mut row_vals = Vec::new();
                for col in &sorted_cols {
                    let val = obj.get(col).unwrap_or(&JsonValue::Null);
                    row_vals.push(QueryBuilder::json_to_sql_literal(val));
                }
                values_list.push(format!("({})", row_vals.join(", ")));
            }

            let sql = format!(
                r#"INSERT INTO {} ({}) VALUES {} RETURNING id::text"#,
                self.table(),
                col_names_sql,
                values_list.join(", ")
            );

            let rows = client.query(&sql, &[]).map_err(|e| {
                crate::error::DBCenterError::QueryError(format!(
                    "Batch insert failed: {}",
                    crate::utils::format_postgres_error(&e)
                ))
            })?;

            for r in rows {
                all_ids.push(r.get(0));
            }
        }

        Ok(all_ids)
    }

    /// Find documents matching query (Hybrid Relational)
    pub fn find(&self, query: Option<&HashMap<String, JsonValue>>) -> Result<Vec<JsonValue>> {
        let (where_clause, _) = if let Some(q) = query {
            QueryBuilder::build_where(q)?
        } else {
            (String::new(), Vec::new())
        };

        // We need JSON rows back. Postgres can convert ROW to JSON.
        let sql = if where_clause.is_empty() {
            format!(r#"SELECT to_jsonb(t) FROM {} t"#, self.table())
        } else {
            format!(
                r#"SELECT to_jsonb(t) FROM {} t WHERE {}"#,
                self.table(),
                where_clause
            )
        };

        let mut client = self.db.get_client();
        let rows = client.query(&sql, &[]).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!(
                "Find failed: {}",
                crate::utils::format_postgres_error(&e)
            ))
        })?;

        let mut results = Vec::new();
        for row in rows {
            // Postgres returns the whole row as a JSONB value in column 0
            if let Some(mut val) = row.get::<_, Option<JsonValue>>(0) {
                // Ensure _id is present (mapped from id)
                if let JsonValue::Object(ref mut map) = val {
                    if let Some(id_val) = map.get("id") {
                        // Copy id to _id for compatibility
                        map.insert("_id".to_string(), id_val.clone());
                    }
                }
                results.push(val);
            }
        }

        Ok(results)
    }

    /// Find one document
    pub fn find_one(
        &self,
        query: Option<&HashMap<String, JsonValue>>,
    ) -> Result<Option<JsonValue>> {
        let results = self.find(query)?;
        Ok(results.into_iter().next())
    }

    /// Count documents
    pub fn count(&self, query: Option<&HashMap<String, JsonValue>>) -> Result<i64> {
        let (where_clause, params) = if let Some(q) = query {
            QueryBuilder::build_where(q)?
        } else {
            (String::new(), Vec::new())
        };

        let sql = if where_clause.is_empty() {
            format!(r#"SELECT COUNT(*) FROM {}"#, self.table())
        } else {
            format!(
                r#"SELECT COUNT(*) FROM {} WHERE {}"#,
                self.table(),
                where_clause
            )
        };

        let mut client = self.db.get_client();

        let param_refs: Vec<&(dyn postgres::types::ToSql + Sync)> = params
            .iter()
            .map(|p| p as &(dyn postgres::types::ToSql + Sync))
            .collect();

        let row = client.query_one(&sql, &param_refs[..]).map_err(|e| {
            crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
        })?;

        Ok(row.get(0))
    }

    /// Update one document
    pub fn update_one(
        &self,
        query: &HashMap<String, JsonValue>,
        update: &HashMap<String, JsonValue>,
    ) -> Result<i32> {
        // 1. Find the ID of the document to update
        let (where_clause, _) = QueryBuilder::build_where(query)?;

        let select_sql = if where_clause.is_empty() {
            format!(r#"SELECT id::text FROM {} LIMIT 1"#, self.table())
        } else {
            format!(
                r#"SELECT id::text FROM {} WHERE {} LIMIT 1"#,
                self.table(),
                where_clause
            )
        };

        let mut client = self.db.get_client();
        let rows = client.query(&select_sql, &[]).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!(
                "Update (find) failed: {}",
                crate::utils::format_postgres_error(&e)
            ))
        })?;

        if rows.is_empty() {
            return Ok(0);
        }

        let id_str: String = rows[0].get(0);

        // 2. Perform Update by ID
        let (update_clause, _) = QueryBuilder::build_relational_update(update)?;

        let update_sql = format!(
            r#"
            UPDATE {}
            SET {}, updated_at = CURRENT_TIMESTAMP
            WHERE id::text = {}
            "#,
            self.table(),
            update_clause,
            QueryBuilder::json_to_sql_literal(&JsonValue::String(id_str))
        );

        let affected = client.execute(&update_sql, &[]).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!(
                "Update (execute) failed: {}",
                crate::utils::format_postgres_error(&e)
            ))
        })?;

        Ok(affected as i32)
    }

    /// Update many documents
    pub fn update_many(
        &self,
        query: &HashMap<String, JsonValue>,
        update: &HashMap<String, JsonValue>,
    ) -> Result<u64> {
        let (where_clause, _) = QueryBuilder::build_where(query)?;
        let (update_clause, _) = QueryBuilder::build_relational_update(update)?;

        // An empty query means "every row". Interpolating it after WHERE would
        // produce `... WHERE ` and a syntax error, so omit the clause entirely.
        let sql = if where_clause.is_empty() {
            format!(
                r#"UPDATE {} SET {}, updated_at = CURRENT_TIMESTAMP"#,
                self.table(),
                update_clause
            )
        } else {
            format!(
                r#"UPDATE {} SET {}, updated_at = CURRENT_TIMESTAMP WHERE {}"#,
                self.table(),
                update_clause,
                where_clause
            )
        };

        let mut client = self.db.get_client();
        let affected = client.execute(&sql, &[]).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!(
                "Update failed: {}",
                crate::utils::format_postgres_error(&e)
            ))
        })?;

        Ok(affected)
    }

    /// Delete one document
    pub fn delete_one(&self, query: &HashMap<String, JsonValue>) -> Result<i32> {
        // 1. Find the ID of the document to delete
        let (where_clause, _) = QueryBuilder::build_where(query)?;

        let select_sql = if where_clause.is_empty() {
            format!(r#"SELECT id::text FROM {} LIMIT 1"#, self.table())
        } else {
            format!(
                r#"SELECT id::text FROM {} WHERE {} LIMIT 1"#,
                self.table(),
                where_clause
            )
        };

        let mut client = self.db.get_client();
        let rows = client.query(&select_sql, &[]).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!(
                "Delete (find) failed: {}",
                crate::utils::format_postgres_error(&e)
            ))
        })?;

        if rows.is_empty() {
            return Ok(0);
        }

        let id_str: String = rows[0].get(0);

        // 2. Perform Delete by ID
        let sql = format!(
            r#"DELETE FROM {} WHERE id::text = {}"#,
            self.table(),
            QueryBuilder::json_to_sql_literal(&JsonValue::String(id_str))
        );

        let affected = client.execute(&sql, &[]).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!(
                "Delete failed: {}",
                crate::utils::format_postgres_error(&e)
            ))
        })?;

        Ok(affected as i32)
    }

    /// Delete many documents
    pub fn delete_many(&self, query: Option<&HashMap<String, JsonValue>>) -> Result<u64> {
        let (where_clause, _) = if let Some(q) = query {
            QueryBuilder::build_where(q)?
        } else {
            (String::new(), Vec::new())
        };

        let sql = if where_clause.is_empty() {
            format!(r#"DELETE FROM {}"#, self.table())
        } else {
            format!(r#"DELETE FROM {} WHERE {}"#, self.table(), where_clause)
        };

        let mut client = self.db.get_client();
        let affected = client.execute(&sql, &[]).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!(
                "Delete failed: {}",
                crate::utils::format_postgres_error(&e)
            ))
        })?;

        Ok(affected)
    }

    /// Does this collection's table have the given column?
    ///
    /// Two physical layouts are in play. A plain collection is relational: every
    /// document field becomes its own column. Delta-sync and geo collections keep
    /// the document body in a reserved `data` JSONB column. Index and similarity
    /// expressions have to address whichever layout the table actually has.
    fn has_column(&self, column: &str) -> Result<bool> {
        let mut client = self.db.get_client();
        let row = client
            .query_one(
                "SELECT EXISTS (
                     SELECT 1 FROM information_schema.columns
                     WHERE table_name = $1 AND column_name = $2
                 )",
                &[&self.name, &column],
            )
            .map_err(|e| {
                crate::error::DBCenterError::QueryError(format!(
                    "Column lookup failed: {}",
                    crate::utils::format_postgres_error(&e)
                ))
            })?;
        Ok(row.get(0))
    }

    /// SQL expression that yields `field` as text under either layout.
    fn field_as_text(&self, field: &str) -> Result<String> {
        if self.has_column("data")? {
            Ok(format!(
                "data->>{}",
                QueryBuilder::json_to_sql_literal(&JsonValue::String(field.to_string()))
            ))
        } else {
            Ok(format!("{}::text", quote_ident(field)))
        }
    }

    /// Create index on a field
    pub fn create_index(&self, field: &str, index_type: &str) -> Result<()> {
        // Index method names are keywords, not identifiers - allow only known ones
        // rather than interpolating whatever the caller passed.
        const METHODS: [&str; 6] = ["btree", "hash", "gist", "spgist", "gin", "brin"];
        let method = index_type.to_ascii_lowercase();
        if !METHODS.contains(&method.as_str()) {
            return Err(crate::error::DBCenterError::InvalidQuery(format!(
                "Unknown index type '{}'. Supported: {}",
                index_type,
                METHODS.join(", ")
            )));
        }

        let expr = self.field_as_text(field)?;
        let idx_name = format!("idx_{}_{}_{}", self.name, field, method);
        let sql = format!(
            r#"CREATE INDEX IF NOT EXISTS {} ON {} USING {} (({}))"#,
            quote_ident(&idx_name),
            self.table(),
            method,
            expr
        );

        let mut client = self.db.get_client();
        client.execute(&sql, &[]).map_err(|e| {
            crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
        })?;

        tracing::info!("Created index: {}", idx_name);
        Ok(())
    }

    /// Fuzzy search using pg_trgm
    pub fn fuzzy_search(
        &self,
        query: &str,
        field: &str,
        mode: Option<&str>,
        threshold: Option<f32>,
    ) -> Result<Vec<JsonValue>> {
        // Ensure pg_trgm
        if let Err(e) = self.db.install_extension("pg_trgm") {
            tracing::warn!("pg_trgm install warning: {}", e);
        }

        let search_mode = mode.unwrap_or("substring");
        if search_mode != "exact" && search_mode != "substring" {
            return Err(crate::error::DBCenterError::InvalidQuery(format!(
                "Unknown fuzzy search mode '{}'. Supported: exact, substring",
                search_mode
            )));
        }

        // Resolve the field expression before taking the client lock - has_column
        // acquires the same mutex.
        let expr = self.field_as_text(field)?;

        let mut client = self.db.get_client();

        // set threshold
        let th = threshold.unwrap_or(0.3);
        if !(0.0..=1.0).contains(&th) {
            return Err(crate::error::DBCenterError::InvalidQuery(format!(
                "Similarity threshold must be between 0.0 and 1.0, got {}",
                th
            )));
        }
        // set BOTH just in case
        if let Err(e) = client.batch_execute(&format!(
            "SET pg_trgm.similarity_threshold = {}; SET pg_trgm.word_similarity_threshold = {};",
            th, th
        )) {
            tracing::warn!("Failed to set similarity thresholds: {}", e);
        }

        // Create index if not exists (critical for performance)
        let idx_name = format!("idx_{}_{}_trgm", self.name, field);
        let sql_idx = format!(
            r#"CREATE INDEX IF NOT EXISTS {} ON {} USING GIN (({}) gin_trgm_ops)"#,
            quote_ident(&idx_name),
            self.table(),
            expr
        );
        if let Err(e) = client.batch_execute(&sql_idx) {
            tracing::warn!(
                "Failed to create trgm index (might already exist or permission issue): {}",
                e
            );
        }

        // `similarity` compares whole strings, `word_similarity` finds the best
        // matching substring. Return the whole row so the caller gets the same
        // shape find() gives them, under either physical layout.
        let sim_fn = if search_mode == "exact" {
            "similarity"
        } else {
            "word_similarity"
        };
        let match_op = if search_mode == "exact" { "%" } else { "<%" };

        let sql_query = format!(
            r#"
            SELECT to_jsonb(t), {sim_fn}($1::text, {expr}) AS score
            FROM {table} t
            WHERE $1::text {match_op} ({expr})
            ORDER BY score DESC
            LIMIT 100
            "#,
            sim_fn = sim_fn,
            expr = expr,
            table = self.table(),
            match_op = match_op
        );

        let rows = client.query(&sql_query, &[&query]).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!(
                "Fuzzy search failed: {}",
                crate::utils::format_postgres_error(&e)
            ))
        })?;

        let mut results = Vec::new();
        for row in rows {
            let mut data: JsonValue = row.get(0);
            let score: f32 = row.get(1);

            if let JsonValue::Object(ref mut map) = data {
                if let Some(id_val) = map.get("id").cloned() {
                    map.insert("_id".to_string(), id_val);
                }
                map.insert("similarity".to_string(), serde_json::Value::from(score));
            }
            results.push(data);
        }

        Ok(results)
    }

    /// Calculate similarity between two strings
    pub fn similarity(&self, s1: &str, s2: &str) -> Result<f32> {
        if let Err(e) = self.db.install_extension("pg_trgm") {
            tracing::warn!("pg_trgm install warning: {}", e);
        }

        let mut client = self.db.get_client();
        let row = client
            .query_one("SELECT similarity($1, $2)", &[&s1, &s2])
            .map_err(|e| {
                crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
            })?;

        Ok(row.get(0))
    }
}
