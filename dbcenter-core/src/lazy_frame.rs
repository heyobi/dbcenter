use crate::error::{DBCenterError, Result};
use crate::DBCenter;
use serde_json::Value as JsonValue;
use std::sync::Arc;

use arrow::array::{
    ArrayRef, BooleanBuilder, Float64Builder, Int64Builder, RecordBatch, StringBuilder,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;

/// LazyFrame for building queries (Polars-style) and exporting to Arrow.
pub struct LazyFrame {
    db: Arc<DBCenter>,
    table_name: String,
    filters: Vec<String>,
    selected_columns: Option<Vec<String>>,
    limit: Option<usize>,
    offset: Option<usize>,
    order_by: Option<String>,
}

impl LazyFrame {
    pub fn new(db: Arc<DBCenter>, table_name: &str) -> Self {
        Self {
            db,
            table_name: table_name.to_string(),
            filters: Vec::new(),
            selected_columns: None,
            limit: None,
            offset: None,
            order_by: None,
        }
    }

    pub fn filter(mut self, condition: String) -> Self {
        self.filters.push(condition);
        self
    }

    pub fn select(mut self, columns: Vec<String>) -> Self {
        self.selected_columns = Some(columns);
        self
    }

    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }

    pub fn offset(mut self, n: usize) -> Self {
        self.offset = Some(n);
        self
    }

    pub fn order_by(mut self, order: String) -> Self {
        self.order_by = Some(order);
        self
    }

    fn build_sql(&self) -> String {
        let cols = if let Some(cols) = &self.selected_columns {
            cols.iter()
                .map(|c| format!("\"{}\"", c))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            "*".to_string()
        };

        let where_clause = if self.filters.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", self.filters.join(" AND "))
        };

        let order_clause = if let Some(order) = &self.order_by {
            format!("ORDER BY {}", order)
        } else {
            String::new()
        };

        let limit_clause = if let Some(limit) = self.limit {
            format!("LIMIT {}", limit)
        } else {
            String::new()
        };

        let offset_clause = if let Some(offset) = self.offset {
            format!("OFFSET {}", offset)
        } else {
            String::new()
        };

        format!(
            "SELECT {} FROM \"{}\" {} {} {} {}",
            cols, self.table_name, where_clause, order_clause, limit_clause, offset_clause
        )
    }

    /// Execute and return JSON results
    pub fn collect(&self) -> Result<Vec<JsonValue>> {
        let sql = self.build_sql();

        // We need JSON rows back. Postgres can convert ROW to JSON for easy consumption,
        // OR we can query columns and manually map.
        // Re-using collection logic: convert whole row to JSONB
        // wrapping the query.

        let wrapper_sql = format!("SELECT to_jsonb(t) FROM ({}) t", sql);

        let mut client = self.db.get_client();
        let rows = client
            .query(&wrapper_sql, &[])
            .map_err(|e| DBCenterError::QueryError(format!("Collect failed: {}", e)))?;

        let mut results = Vec::new();
        for row in rows {
            if let Some(val) = row.get::<_, Option<JsonValue>>(0) {
                results.push(val);
            }
        }
        Ok(results)
    }

    /// Execute and return Arrow IPC Stream (Zero-Copy compatible buffer)
    pub fn to_arrow_ipc(&self) -> Result<Vec<u8>> {
        let sql = self.build_sql();
        let mut client = self.db.get_client();
        let rows = client
            .query(&sql, &[])
            .map_err(|e| DBCenterError::QueryError(format!("Arrow export failed: {}", e)))?;

        if rows.is_empty() {
            return Ok(Vec::new());
        }

        // 1. Infer Arrow Schema from Postgres Columns
        let stmt = client
            .prepare(&sql)
            .map_err(|e| DBCenterError::QueryError(e.to_string()))?;
        let columns = stmt.columns();

        let mut arrow_fields = Vec::new();
        for col in columns {
            let dtype = match col.type_().name() {
                "bool" => DataType::Boolean,
                "int2" | "int4" => DataType::Int64, // Promoted to Int64 for simplicity
                "int8" => DataType::Int64,
                "float4" | "float8" => DataType::Float64,
                "numeric" => DataType::Float64, // Approx
                _ => DataType::Utf8,            // Fallback to string
            };
            arrow_fields.push(Field::new(col.name(), dtype, true));
        }
        let schema = Arc::new(Schema::new(arrow_fields));

        // 2. Build Arrays
        let mut builders: Vec<Box<dyn arrow::array::ArrayBuilder>> = Vec::new();
        for field in schema.fields() {
            match field.data_type() {
                DataType::Boolean => builders.push(Box::new(BooleanBuilder::new())),
                DataType::Int64 => builders.push(Box::new(Int64Builder::new())),
                DataType::Float64 => builders.push(Box::new(Float64Builder::new())),
                DataType::Utf8 => builders.push(Box::new(StringBuilder::new())),
                _ => unreachable!(),
            }
        }

        for row in &rows {
            for (i, field) in schema.fields().iter().enumerate() {
                let builder = &mut builders[i];

                // Rust robust handling for different builders
                // We have to downcast to specific builder type
                if let Some(b) = builder.as_any_mut().downcast_mut::<BooleanBuilder>() {
                    let val: Option<bool> = row.get(i);
                    b.append_option(val);
                } else if let Some(b) = builder.as_any_mut().downcast_mut::<Int64Builder>() {
                    // Try getting as i64, if fail try i32
                    if let Ok(val) = row.try_get::<_, i64>(i) {
                        b.append_value(val);
                    } else if let Ok(val) = row.try_get::<_, i32>(i) {
                        b.append_value(val as i64);
                    } else if let Ok(val) = row.try_get::<_, i16>(i) {
                        b.append_value(val as i64);
                    } else {
                        b.append_null();
                    }
                } else if let Some(b) = builder.as_any_mut().downcast_mut::<Float64Builder>() {
                    if let Ok(val) = row.try_get::<_, f64>(i) {
                        b.append_value(val);
                    } else if let Ok(val) = row.try_get::<_, f32>(i) {
                        b.append_value(val as f64);
                    } else {
                        // numeric might fail to cast directly to f64 via rust-postgres
                        // fallback to string parsing? expensive.
                        // Assuming numeric -> f64 works for now or NULL.
                        b.append_null();
                    }
                } else if let Some(b) = builder.as_any_mut().downcast_mut::<StringBuilder>() {
                    // Fallback to string representation for everything else
                    // We can try getting specific types first
                    if let Ok(val) = row.try_get::<_, String>(i) {
                        b.append_value(val);
                    } else if let Ok(val) = row.try_get::<_, &str>(i) {
                        b.append_value(val);
                    } else if let Ok(val) = row.try_get::<_, JsonValue>(i) {
                        b.append_value(val.to_string());
                    } else {
                        // If all else fails
                        b.append_null();
                    }
                }
            }
        }

        let mut arrays: Vec<ArrayRef> = Vec::new();
        for mut builder in builders {
            arrays.push(builder.finish());
        }

        let batch = RecordBatch::try_new(schema.clone(), arrays)
            .map_err(|e| DBCenterError::ValidationError(e.to_string()))?;

        // 3. Write to IPC Buffer
        let mut buffer = Vec::new();
        {
            let mut writer = StreamWriter::try_new(&mut buffer, &schema)
                .map_err(|e| DBCenterError::ValidationError(e.to_string()))?;
            writer
                .write(&batch)
                .map_err(|e| DBCenterError::ValidationError(e.to_string()))?;
            writer
                .finish()
                .map_err(|e| DBCenterError::ValidationError(e.to_string()))?;
        }

        Ok(buffer)
    }
}
