use crate::error::{DBCenterError, Result};
use serde_json::Value as JsonValue;
use std::collections::HashMap;

pub struct QueryBuilder;

impl QueryBuilder {
    /// Build WHERE clause from MongoDB-style query (parameter-free version)
    pub fn build_where(query: &HashMap<String, JsonValue>) -> Result<(String, Vec<JsonValue>)> {
        let mut conditions = Vec::new();

        for (key, value) in query {
            // Special handling for _id (Primary Key)
            if key == "_id" {
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
                                    conditions.push(format!("id {} {}", op_sym, val_str));
                                }
                                "$ne" => {
                                    let val_str = Self::json_to_sql_literal(op_value);
                                    conditions.push(format!("id != {}", val_str));
                                }
                                "$in" => {
                                    if let JsonValue::Array(values) = op_value {
                                        let values_str: Vec<String> =
                                            values.iter().map(Self::json_to_sql_literal).collect();
                                        conditions.push(format!("id IN ({})", values_str.join(", ")));
                                    }
                                }
                                _ => {
                                    // Ignore unknown operators for now or error
                                }
                            }
                        }
                    }
                    _ => {
                        // Direct equality
                        let val_str = Self::json_to_sql_literal(value);
                        conditions.push(format!("id = {}", val_str));
                    }
                }
                continue;
            }

            match value {
                JsonValue::Object(operators) => {
                    // Operators: $gt, $lt, $gte, $lte, $ne, $in, $regex, $exists
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
                                conditions.push(format!(
                                    "(data->>'{}'::numeric {} {})",
                                    key, op_sym, val_str
                                ));
                            }
                            "$ne" => {
                                let val_str = Self::json_to_sql_literal(op_value);
                                conditions.push(format!("data->>'{}' != {}", key, val_str));
                            }
                            "$in" => {
                                if let JsonValue::Array(values) = op_value {
                                    let values_str: Vec<String> =
                                        values.iter().map(Self::json_to_sql_literal).collect();
                                    conditions.push(format!(
                                        "data->>'{}' IN ({})",
                                        key,
                                        values_str.join(", ")
                                    ));
                                }
                            }
                            "$regex" => {
                                let val_str = Self::json_to_sql_literal(op_value);
                                conditions.push(format!("data->>'{}' ~ {}", key, val_str));
                            }
                            "$exists" => {
                                if op_value.as_bool() == Some(true) {
                                    conditions.push(format!("data ? '{}'", key));
                                } else {
                                    conditions.push(format!("NOT data ? '{}'", key));
                                }
                            }
                            _ => {
                                return Err(DBCenterError::InvalidQuery(format!(
                                    "Unknown operator: {}",
                                    op
                                )));
                            }
                        }
                    }
                }
                _ => {
                    // Direct equality
                    let val_str = Self::json_to_sql_literal(value);
                    conditions.push(format!("data->>'{}' = {}", key, val_str));
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
    fn json_to_sql_literal(value: &JsonValue) -> String {
        match value {
            JsonValue::Null => "NULL".to_string(),
            JsonValue::Bool(b) => b.to_string(),
            JsonValue::Number(n) => n.to_string(),
            JsonValue::String(s) => format!("'{}'", s.replace("'", "''")),
            _ => format!("'{}'", value.to_string().replace("'", "''")),
        }
    }

    /// Build UPDATE clause from MongoDB-style update operators (parameter-free version)
    pub fn build_update(update: &HashMap<String, JsonValue>) -> Result<(String, Vec<JsonValue>)> {
        let set_data = if let Some(data) = update.get("$set") {
            data
        } else {
            // Direct update (treat as $set)
            &JsonValue::Object(update.clone().into_iter().collect())
        };

        let json_str =
            serde_json::to_string(set_data).map_err(|e| DBCenterError::SerializationError(e))?;
        let escaped = json_str.replace("'", "''");

        Ok((format!("data || '{}'::jsonb", escaped), Vec::new()))
    }
}
