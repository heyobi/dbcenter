use crate::error::Result;
use crate::query::QueryBuilder;
use crate::DBCenter;
use serde_json::Value as JsonValue;
use std::collections::HashMap;

/// Collection wrapper - MongoDB-style API for PostgreSQL
pub struct Collection<'a> {
    db: &'a DBCenter,
    name: String,
}

impl<'a> Collection<'a> {
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

    /// Ensure JSONB table exists
    fn ensure_table(&self) -> Result<()> {
        let sql = format!(
            r#"
            CREATE TABLE IF NOT EXISTS "{}" (
                id SERIAL PRIMARY KEY,
                data JSONB,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX IF NOT EXISTS idx_{}_data ON "{}" USING GIN (data);
            "#,
            self.name, self.name, self.name
        );

        let mut client = self.db.get_client();
        client.batch_execute(&sql).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!("Table creation failed: {}", e))
        })?;

        Ok(())
    }

    /// Insert a single document
    pub fn insert_one(&self, document: &JsonValue) -> Result<i32> {
        let json_str = serde_json::to_string(document)
            .map_err(crate::error::DBCenterError::SerializationError)?;

        // Use direct SQL embedding (escape single quotes for safety)
        let escaped_json = json_str.replace("'", "''");
        let sql = format!(
            r#"INSERT INTO "{}" (data) VALUES ('{}'::jsonb) RETURNING id"#,
            self.name, escaped_json
        );

        tracing::debug!("Executing SQL: {}", sql);

        let mut client = self.db.get_client();
        let row = client.query_one(&sql, &[]).map_err(|e| {
            tracing::error!("Insert failed: {}", e);
            crate::error::DBCenterError::QueryError(format!("Insert failed: {}", e))
        })?;

        Ok(row.get(0))
    }

    /// Insert multiple documents
    pub fn insert_many(&self, documents: &[JsonValue]) -> Result<Vec<i32>> {
        let mut ids = Vec::new();
        for doc in documents {
            ids.push(self.insert_one(doc)?);
        }
        Ok(ids)
    }

    /// Find documents matching query
    pub fn find(&self, query: Option<&HashMap<String, JsonValue>>) -> Result<Vec<JsonValue>> {
        let (where_clause, _) = if let Some(q) = query {
            QueryBuilder::build_where(q)?
        } else {
            (String::new(), Vec::new())
        };

        let sql = if where_clause.is_empty() {
            format!(
                r#"SELECT id, data, created_at, updated_at FROM "{}""#,
                self.name
            )
        } else {
            format!(
                r#"SELECT id, data, created_at, updated_at FROM "{}" WHERE {}"#,
                self.name, where_clause
            )
        };

        let mut client = self.db.get_client();
        let rows = client
            .query(&sql, &[])
            .map_err(|e| crate::error::DBCenterError::QueryError(format!("Find failed: {}", e)))?;

        let mut results = Vec::new();
        for row in rows {
            let id: i32 = row.get(0);
            let mut data: JsonValue = row.get(1);

            // Add _id field
            if let JsonValue::Object(ref mut map) = data {
                map.insert("_id".to_string(), JsonValue::Number(id.into()));
            }

            results.push(data);
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
            format!(r#"SELECT COUNT(*) FROM "{}""#, self.name)
        } else {
            format!(
                r#"SELECT COUNT(*) FROM "{}" WHERE {}"#,
                self.name, where_clause
            )
        };

        let mut client = self.db.get_client();

        let param_refs: Vec<&(dyn postgres::types::ToSql + Sync)> = params
            .iter()
            .map(|p| p as &(dyn postgres::types::ToSql + Sync))
            .collect();

        let row = client
            .query_one(&sql, &param_refs[..])
            .map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;

        Ok(row.get(0))
    }

    /// Update one document
    pub fn update_one(
        &self,
        query: &HashMap<String, JsonValue>,
        update: &HashMap<String, JsonValue>,
    ) -> Result<i32> {
        let (where_clause, where_params) = QueryBuilder::build_where(query)?;
        let (update_clause, update_params) = QueryBuilder::build_update(update)?;

        let sql = format!(
            r#"
            UPDATE "{}" 
            SET data = {}, updated_at = CURRENT_TIMESTAMP
            WHERE id = (
                SELECT id FROM "{}" 
                WHERE {} 
                LIMIT 1
            )
            "#,
            self.name, update_clause, self.name, where_clause
        );

        let mut all_params = update_params;
        all_params.extend(where_params);

        let param_refs: Vec<&(dyn postgres::types::ToSql + Sync)> = all_params
            .iter()
            .map(|p| p as &(dyn postgres::types::ToSql + Sync))
            .collect();

        let mut client = self.db.get_client();
        client
            .execute(&sql, &param_refs[..])
            .map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;

        Ok(1)
    }

    /// Update many documents
    pub fn update_many(
        &self,
        query: &HashMap<String, JsonValue>,
        update: &HashMap<String, JsonValue>,
    ) -> Result<u64> {
        let (where_clause, _) = QueryBuilder::build_where(query)?;
        let (update_clause, _) = QueryBuilder::build_update(update)?;

        let sql = format!(
            r#"UPDATE "{}" SET data = {}, updated_at = CURRENT_TIMESTAMP WHERE {}"#,
            self.name, update_clause, where_clause
        );

        let mut client = self.db.get_client();
        let affected = client.execute(&sql, &[]).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!("Update failed: {}", e))
        })?;

        Ok(affected)
    }

    /// Delete one document
    pub fn delete_one(&self, query: &HashMap<String, JsonValue>) -> Result<i32> {
        let (where_clause, _) = QueryBuilder::build_where(query)?;

        let sql = format!(
            r#"
            DELETE FROM "{}" 
            WHERE id = (
                SELECT id FROM "{}" 
                WHERE {} 
                LIMIT 1
            )
            "#,
            self.name, self.name, where_clause
        );

        let mut client = self.db.get_client();
        client.execute(&sql, &[]).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!("Delete failed: {}", e))
        })?;

        Ok(1)
    }

    /// Delete many documents
    pub fn delete_many(&self, query: Option<&HashMap<String, JsonValue>>) -> Result<u64> {
        let (where_clause, _) = if let Some(q) = query {
            QueryBuilder::build_where(q)?
        } else {
            (String::new(), Vec::new())
        };

        let sql = if where_clause.is_empty() {
            format!(r#"DELETE FROM "{}""#, self.name)
        } else {
            format!(r#"DELETE FROM "{}" WHERE {}"#, self.name, where_clause)
        };

        let mut client = self.db.get_client();
        let affected = client.execute(&sql, &[]).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!("Delete failed: {}", e))
        })?;

        Ok(affected)
    }

    /// Create index on a field
    pub fn create_index(&self, field: &str, index_type: &str) -> Result<()> {
        let idx_name = format!("idx_{}_{}_{}", self.name, field, index_type);
        let sql = format!(
            r#"CREATE INDEX IF NOT EXISTS {} ON "{}" USING {} ((data->>'{}'))"#,
            idx_name, self.name, index_type, field
        );

        let mut client = self.db.get_client();
        client
            .execute(&sql, &[])
            .map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;

        tracing::info!("Created index: {}", idx_name);
        Ok(())
    }
    /// Fuzzy search using pg_trgm
    pub fn fuzzy_search(
        &self, 
        query: &str, 
        field: &str,
        mode: Option<&str>, 
        threshold: Option<f32>
    ) -> Result<Vec<JsonValue>> {
        // Ensure pg_trgm
        if let Err(e) = self.db.install_extension("pg_trgm") {
            tracing::warn!("pg_trgm install warning: {}", e);
        }

        let mut client = self.db.get_client();
        let search_mode = mode.unwrap_or("substring");
        
        // set threshold
        let th = threshold.unwrap_or(0.3);
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
            r#"CREATE INDEX IF NOT EXISTS "{}" ON "{}" USING GIN ((data->>'{}') gin_trgm_ops)"#,
            idx_name, self.name, field
        );
        if let Err(e) = client.batch_execute(&sql_idx) {
             tracing::warn!("Failed to create trgm index (might already exist or permission issue): {}", e);
        }

        let sql_query = if search_mode == "exact" {
            // WHOLE STRING similarity (%)
            format!(
                r#"
                SELECT data, similarity($1, data->>'{}') as score
                FROM "{}"
                WHERE $1 % (data->>'{}')
                ORDER BY score DESC
                LIMIT 100
                "#,
                field, self.name, field
            )
        } else {
             // SUBSTRING similarity (<%), default
             format!(
                r#"
                SELECT data, word_similarity($1, data->>'{}') as score
                FROM "{}"
                WHERE $1 <% (data->>'{}')
                ORDER BY score DESC
                LIMIT 100
                "#,
                field, self.name, field
            )
        };

        let rows = client.query(&sql_query, &[&query])
             .map_err(|e| crate::error::DBCenterError::QueryError(format!("Fuzzy search failed: {}", crate::utils::format_postgres_error(&e))))?;

        let mut results = Vec::new();
        for row in rows {
            let mut data: JsonValue = row.get(0);
            let score: f32 = row.get(1);

            if let JsonValue::Object(ref mut map) = data {
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
        let row = client.query_one("SELECT similarity($1, $2)", &[&s1, &s2])
            .map_err(|e| crate::error::DBCenterError::QueryError(e.to_string()))?;
        
        Ok(row.get(0))
    }
}
