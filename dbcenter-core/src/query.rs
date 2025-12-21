use crate::error::{DBCenterError, Result};
use serde_json::Value as JsonValue;
use std::collections::HashMap;

pub struct QueryBuilder;

impl QueryBuilder {
    /// Build WHERE clause from filtered query
    pub fn build_where(query: &HashMap<String, JsonValue>) -> Result<(String, Vec<JsonValue>)> {
        let mut conditions = Vec::new();

        for (key, value) in query {
            // Map _id to id
            let db_key = if key == "_id" { "id" } else { key };

            // Special handling for operators
            match value {
                JsonValue::Object(operators) => {
                    for (op, op_value) in operators {
                        match op.as_str() {
                            "$gt" | "$gte" | "$lt" | "$lte" => {
                                let op_sym = match op.as_str() {
                                    "$gt" => ">",
                                    "$gte" => ">=",
                                    "$lt" => "<",
                                    "$lte" => "<=",
                                    _ => unreachable!(),
                                };
                                let val_str = Self::json_to_sql_literal(op_value);
                                conditions.push(format!("\"{}\" {} {}", db_key, op_sym, val_str));
                            }
                            "$ne" => {
                                let val_str = Self::json_to_sql_literal(op_value);
                                conditions.push(format!("\"{}\" != {}", db_key, val_str));
                            }
                            "$in" => {
                                if let JsonValue::Array(values) = op_value {
                                    let values_str: Vec<String> =
                                        values.iter().map(Self::json_to_sql_literal).collect();
                                    conditions.push(format!(
                                        "\"{}\" IN ({})",
                                        db_key,
                                        values_str.join(", ")
                                    ));
                                }
                            }
                            "$regex" => {
                                let val_str = Self::json_to_sql_literal(op_value);
                                conditions.push(format!("\"{}\" ~ {}", db_key, val_str));
                            }
                            _ => {
                                // Ignore unknown or handle strictly?
                            }
                        }
                    }
                }
                _ => {
                    // Direct equality
                    let val_str = Self::json_to_sql_literal(value);
                    conditions.push(format!("\"{}\" = {}", db_key, val_str));
                }
            }
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            conditions.join(" AND ")
        };

        Ok((where_clause, Vec::new()))
    }

    /// Convert JSON value to SQL literal (escaped)
    pub fn json_to_sql_literal(value: &JsonValue) -> String {
        match value {
            JsonValue::Null => "NULL".to_string(),
            JsonValue::Bool(b) => b.to_string(),
            JsonValue::Number(n) => n.to_string(),
            JsonValue::String(s) => format!("'{}'", s.replace("'", "''")),
            _ => format!("'{}'", value.to_string().replace("'", "''")),
        }
    }

    /// Build UPDATE SET clause (Relational)
    pub fn build_relational_update(
        update: &HashMap<String, JsonValue>,
    ) -> Result<(String, Vec<JsonValue>)> {
        let set_data = if let Some(data) = update.get("$set") {
            data.as_object().ok_or(DBCenterError::InvalidQuery(
                "$set must be an object".to_string(),
            ))?
        } else {
            // Treat as direct update if no $Operator found, BUT check if keys start with $
            // Simplified: if keys are columns.
            if update.contains_key("$set") {
                return Err(DBCenterError::InvalidQuery(
                    "Start with $set or use dictionary directly".to_string(),
                ));
            }
            match update.get("$set") {
                Some(val) => val
                    .as_object()
                    .ok_or(DBCenterError::InvalidQuery("Invalid $set".into()))?,
                None => return Self::build_direct_update_string(update),
            }
        };

        let mut updates = Vec::new();
        for (k, v) in set_data {
            let val_str = Self::json_to_sql_literal(v);
            updates.push(format!("\"{}\" = {}", k, val_str));
        }

        Ok((updates.join(", "), Vec::new()))
    }

    fn build_direct_update_string(
        update: &HashMap<String, JsonValue>,
    ) -> Result<(String, Vec<JsonValue>)> {
        let mut updates = Vec::new();
        for (k, v) in update {
            if k.starts_with('$') {
                continue;
            } // ignore ops
            let val_str = Self::json_to_sql_literal(v);
            updates.push(format!("\"{}\" = {}", k, val_str));
        }
        Ok((updates.join(", "), Vec::new()))
    }
}
