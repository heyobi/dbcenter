use crate::error::{DBCenterError, Result};
use serde_json::Value as JsonValue;
use std::collections::HashMap;

/// Quote a SQL identifier, escaping any embedded double quotes.
///
/// Every identifier that reaches SQL (table, column, index name) must go
/// through this. Interpolating a raw name into `"{}"` lets a name containing
/// a `"` terminate the quoted identifier and inject arbitrary SQL.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub struct QueryBuilder;

impl QueryBuilder {
    /// Build WHERE clause from filtered query.
    ///
    /// Returns an empty string when `query` is empty, meaning "match everything".
    /// Callers must treat that as "no WHERE clause" rather than splicing `WHERE `
    /// into the statement.
    ///
    /// Unrecognised operators are rejected rather than ignored: silently dropping
    /// a condition turns a targeted `delete_many`/`update_many` into one that
    /// matches every row.
    pub fn build_where(query: &HashMap<String, JsonValue>) -> Result<(String, Vec<JsonValue>)> {
        let mut conditions = Vec::new();

        for (key, value) in query {
            // Map _id to id
            let db_key = if key == "_id" { "id" } else { key };
            let ident = quote_ident(db_key);

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
                                conditions.push(format!("{} {} {}", ident, op_sym, val_str));
                            }
                            "$eq" => {
                                let val_str = Self::json_to_sql_literal(op_value);
                                conditions.push(format!("{} = {}", ident, val_str));
                            }
                            "$ne" => {
                                let val_str = Self::json_to_sql_literal(op_value);
                                conditions.push(format!("{} != {}", ident, val_str));
                            }
                            "$in" | "$nin" => {
                                let values = op_value.as_array().ok_or_else(|| {
                                    DBCenterError::InvalidQuery(format!(
                                        "{} on field '{}' requires an array, got {}",
                                        op, db_key, op_value
                                    ))
                                })?;
                                if values.is_empty() {
                                    // An empty $in matches nothing; an empty $nin matches everything.
                                    conditions.push(
                                        if op == "$in" { "FALSE" } else { "TRUE" }.to_string(),
                                    );
                                    continue;
                                }
                                let values_str: Vec<String> =
                                    values.iter().map(Self::json_to_sql_literal).collect();
                                let sql_op = if op == "$in" { "IN" } else { "NOT IN" };
                                conditions.push(format!(
                                    "{} {} ({})",
                                    ident,
                                    sql_op,
                                    values_str.join(", ")
                                ));
                            }
                            "$exists" => {
                                let exists = op_value.as_bool().ok_or_else(|| {
                                    DBCenterError::InvalidQuery(format!(
                                        "$exists on field '{}' requires a boolean",
                                        db_key
                                    ))
                                })?;
                                conditions.push(format!(
                                    "{} IS {} NULL",
                                    ident,
                                    if exists { "NOT" } else { "" }
                                ));
                            }
                            "$regex" => {
                                let val_str = Self::json_to_sql_literal(op_value);
                                conditions.push(format!("{} ~ {}", ident, val_str));
                            }
                            other => {
                                return Err(DBCenterError::InvalidQuery(format!(
                                    "Unsupported query operator '{}' on field '{}'. \
                                     Supported: $eq $ne $gt $gte $lt $lte $in $nin $exists $regex",
                                    other, db_key
                                )));
                            }
                        }
                    }
                }
                JsonValue::Null => {
                    conditions.push(format!("{} IS NULL", ident));
                }
                _ => {
                    // Direct equality
                    let val_str = Self::json_to_sql_literal(value);
                    conditions.push(format!("{} = {}", ident, val_str));
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
            // E'' would re-enable backslash escapes; a plain quoted literal with
            // doubled single quotes is safe under standard_conforming_strings,
            // which PostgreSQL has defaulted to since 9.1.
            JsonValue::String(s) => format!("'{}'", s.replace('\'', "''")),
            _ => format!("'{}'", value.to_string().replace('\'', "''")),
        }
    }

    /// Build UPDATE SET clause (Relational)
    pub fn build_relational_update(
        update: &HashMap<String, JsonValue>,
    ) -> Result<(String, Vec<JsonValue>)> {
        let set_data = match update.get("$set") {
            Some(data) => data.as_object().ok_or_else(|| {
                DBCenterError::InvalidQuery("$set must be an object".to_string())
            })?,
            // No $set: treat the whole map as column assignments.
            None => return Self::build_direct_update_string(update),
        };

        let mut updates = Vec::new();
        for (k, v) in set_data {
            let val_str = Self::json_to_sql_literal(v);
            updates.push(format!("{} = {}", quote_ident(k), val_str));
        }

        if updates.is_empty() {
            return Err(DBCenterError::InvalidQuery(
                "Update document contains no fields to set".to_string(),
            ));
        }

        Ok((updates.join(", "), Vec::new()))
    }

    fn build_direct_update_string(
        update: &HashMap<String, JsonValue>,
    ) -> Result<(String, Vec<JsonValue>)> {
        let mut updates = Vec::new();
        for (k, v) in update {
            if k.starts_with('$') {
                return Err(DBCenterError::InvalidQuery(format!(
                    "Unsupported update operator '{}'. Use $set, or pass plain column values.",
                    k
                )));
            }
            let val_str = Self::json_to_sql_literal(v);
            updates.push(format!("{} = {}", quote_ident(k), val_str));
        }

        if updates.is_empty() {
            return Err(DBCenterError::InvalidQuery(
                "Update document contains no fields to set".to_string(),
            ));
        }

        Ok((updates.join(", "), Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn q(pairs: &[(&str, JsonValue)]) -> HashMap<String, JsonValue> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn quotes_embedded_double_quotes() {
        assert_eq!(quote_ident(r#"a"b"#), r#""a""b""#);
    }

    #[test]
    fn field_name_cannot_break_out_of_the_identifier() {
        let evil = r#"name" = 'keep' OR "1"="1"#;
        let (where_clause, _) = build(&q(&[(evil, json!("zzz"))]));
        // The whole hostile name stays inside one quoted identifier.
        assert!(where_clause.starts_with(r#""name"" = 'keep' OR ""1""=""1" = "#));
        assert!(!where_clause.contains(" OR \"1\"=\"1\" ="));
    }

    #[test]
    fn unknown_operator_is_rejected_not_dropped() {
        let err = QueryBuilder::build_where(&q(&[("age", json!({"$bogus": 1}))])).unwrap_err();
        assert!(err.to_string().contains("Unsupported query operator"));
    }

    #[test]
    fn in_requires_an_array() {
        let err = QueryBuilder::build_where(&q(&[("name", json!({"$in": "a"}))])).unwrap_err();
        assert!(err.to_string().contains("requires an array"));
    }

    #[test]
    fn empty_in_matches_nothing() {
        let (w, _) = build(&q(&[("name", json!({"$in": []}))]));
        assert_eq!(w, "FALSE");
    }

    #[test]
    fn empty_query_yields_empty_clause() {
        let (w, _) = build(&HashMap::new());
        assert!(w.is_empty());
    }

    #[test]
    fn null_becomes_is_null() {
        let (w, _) = build(&q(&[("name", json!(null))]));
        assert_eq!(w, r#""name" IS NULL"#);
    }

    #[test]
    fn id_is_mapped_to_the_id_column() {
        let (w, _) = build(&q(&[("_id", json!(3))]));
        assert_eq!(w, r#""id" = 3"#);
    }

    #[test]
    fn string_literals_have_quotes_doubled() {
        let (w, _) = build(&q(&[("name", json!("O'Brien"))]));
        assert_eq!(w, r#""name" = 'O''Brien'"#);
    }

    #[test]
    fn update_without_set_rejects_dollar_operators() {
        let err =
            QueryBuilder::build_relational_update(&q(&[("$inc", json!({"n": 1}))])).unwrap_err();
        assert!(err.to_string().contains("Unsupported update operator"));
    }

    #[test]
    fn empty_update_is_rejected() {
        let err = QueryBuilder::build_relational_update(&HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("no fields to set"));
    }

    fn build(m: &HashMap<String, JsonValue>) -> (String, Vec<JsonValue>) {
        QueryBuilder::build_where(m).expect("build_where should succeed")
    }
}
