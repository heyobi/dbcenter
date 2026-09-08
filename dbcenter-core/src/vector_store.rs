use crate::error::Result;
use crate::DBCenter;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
// use crate::query::QueryBuilder;

/// VectorStore - ChromaDB-style API for PostgreSQL (pgvector)
pub struct VectorStore<'a> {
    db: &'a DBCenter,
    name: String,
    dimension: usize,
}

impl<'a> VectorStore<'a> {
    pub fn new(db: &'a DBCenter, name: &str, dimension: usize) -> Result<Self> {
        let store = Self {
            db,
            name: name.replace("\"", "").to_string(),
            dimension,
        };

        // Ensure pgvector extension
        db.install_extension("pgvector")?;

        // Initialize table
        store.ensure_table()?;

        Ok(store)
    }

    fn ensure_table(&self) -> Result<()> {
        let sql = format!(
            r#"
            CREATE TABLE IF NOT EXISTS "{}" (
                id TEXT PRIMARY KEY,
                embedding vector({}),
                metadata JSONB DEFAULT '{{}}'::jsonb,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );

            -- HNSW index for fast ANN search (cosine distance)
            CREATE INDEX IF NOT EXISTS idx_{}_embedding ON "{}" USING hnsw (embedding vector_cosine_ops);
            
            -- GIN index for metadata filtering
            CREATE INDEX IF NOT EXISTS idx_{}_metadata ON "{}" USING GIN (metadata);
            "#,
            self.name, self.dimension, self.name, self.name, self.name, self.name
        );

        let mut client = self.db.get_client();
        client.batch_execute(&sql).map_err(|e| {
            crate::error::DBCenterError::QueryError(format!(
                "VectorStore init failed: {}",
                crate::utils::format_postgres_error(&e)
            ))
        })?;

        Ok(())
    }

    /// Add embeddings (Upsert)
    pub fn add(
        &self,
        ids: &[String],
        embeddings: &[Vec<f32>],
        metadatas: Option<&[HashMap<String, JsonValue>]>,
    ) -> Result<()> {
        if ids.len() != embeddings.len() {
            return Err(crate::error::DBCenterError::QueryError(
                "ids and embeddings must have the same length".to_string(),
            ));
        }

        let mut client = self.db.get_client();
        let mut transaction = client.transaction().map_err(|e| {
            crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
        })?;

        for i in 0..ids.len() {
            let id = &ids[i];
            let embedding = &embeddings[i];

            // Check dimension
            if embedding.len() != self.dimension {
                return Err(crate::error::DBCenterError::QueryError(format!(
                    "Embedding at index {} has dimension {} but store expects {}",
                    i,
                    embedding.len(),
                    self.dimension
                )));
            }

            let embedding_str = format!("{:?}", embedding);

            let default_meta = HashMap::new();
            let metadata = if let Some(metas) = metadatas {
                metas.get(i).unwrap_or(&default_meta)
            } else {
                &default_meta
            };
            let metadata_json = serde_json::to_value(metadata).unwrap();

            // Direct interpolation for vector to avoid bind param type issues
            let sql = format!(
                r#"
                INSERT INTO "{}" (id, embedding, metadata, updated_at)
                VALUES ($1, '{}'::vector, $2, CURRENT_TIMESTAMP)
                ON CONFLICT (id) DO UPDATE SET
                    embedding = EXCLUDED.embedding,
                    metadata = EXCLUDED.metadata,
                    updated_at = CURRENT_TIMESTAMP
                "#,
                self.name, embedding_str
            );

            transaction
                .execute(&sql, &[&id, &metadata_json])
                .map_err(|e| {
                    crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
                })?;
        }

        transaction.commit().map_err(|e| {
            crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
        })?;

        Ok(())
    }

    /// Semantic search
    pub fn query(
        &self,
        query_embeddings: &[Vec<f32>],
        n_results: i64,
        where_filter: Option<&HashMap<String, JsonValue>>,
    ) -> Result<Vec<JsonValue>> {
        let embedding = query_embeddings
            .first()
            .ok_or(crate::error::DBCenterError::QueryError(
                "No query embedding provided".to_string(),
            ))?;

        if embedding.len() != self.dimension {
            // ...
        }

        // Build metadata filter
        let where_clause = if let Some(filter) = where_filter {
            let json_filter = serde_json::to_string(filter).unwrap();
            let escaped_filter = json_filter.replace("'", "''");
            format!("metadata @> '{}'", escaped_filter)
        } else {
            "TRUE".to_string()
        };

        let embedding_str = format!("{:?}", embedding);

        // Interpolate embedding
        let sql = format!(
            r#"
            SELECT id, metadata, (embedding <=> '{}'::vector) as distance
            FROM "{}"
            WHERE {}
            ORDER BY embedding <=> '{}'::vector ASC
            LIMIT $1
            "#,
            embedding_str, self.name, where_clause, embedding_str
        );

        let mut client = self.db.get_client();
        let rows = client.query(&sql, &[&n_results]).map_err(|e| {
            crate::error::DBCenterError::QueryError(crate::utils::format_postgres_error(&e))
        })?;

        let mut results = Vec::new();
        for row in rows {
            let id: String = row.get(0);
            let metadata: JsonValue = row.get(1);
            let distance: f64 = row.get(2);

            // And result format: `{"id": "...", "distance": ..., "metadata": ...}`
            let mut res_map = serde_json::Map::new();
            res_map.insert("id".to_string(), JsonValue::String(id));
            res_map.insert(
                "distance".to_string(),
                JsonValue::Number(
                    serde_json::Number::from_f64(distance).unwrap_or(serde_json::Number::from(0)),
                ),
            );
            res_map.insert("metadata".to_string(), metadata);

            results.push(JsonValue::Object(res_map));
        }

        Ok(results)
    }
}
