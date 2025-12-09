use crate::error::{DBCenterError, Result};
use postgres::types::Type;
use postgres::Row;
use serde_json::Value as JsonValue;
use std::net::TcpListener;

/// Get a free port for PostgreSQL
pub fn get_free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| DBCenterError::IoError(e))?;

    let port = listener
        .local_addr()
        .map_err(|e| DBCenterError::IoError(e))?
        .port();

    Ok(port)
}

/// Format PostgreSQL error with details
pub fn format_postgres_error(error: &postgres::Error) -> String {
    if let Some(db_error) = error.as_db_error() {
        let severity = db_error.severity();
        let message = db_error.message();
        let code = db_error.code().code();
        
        let mut formatted = format!("[{}] {} (SQLSTATE: {})", severity, message, code);
        
        if let Some(detail) = db_error.detail() {
            formatted.push_str(&format!("\nDETAIL: {}", detail));
        }
        
        if let Some(hint) = db_error.hint() {
            formatted.push_str(&format!("\nHINT: {}", hint));
        }
        
        formatted
    } else {
        error.to_string()
    }
}

/// Convert a PostgreSQL row value to JSON
pub fn row_value_to_json(row: &Row, idx: usize) -> JsonValue {
    let column = &row.columns()[idx];
    let col_type = column.type_();

    match *col_type {
        Type::BOOL => row
            .try_get::<_, bool>(idx)
            .map(JsonValue::Bool)
            .unwrap_or(JsonValue::Null),
        Type::INT2 => row
            .try_get::<_, i16>(idx)
            .map(|v| JsonValue::Number(v.into()))
            .unwrap_or(JsonValue::Null),
        Type::INT4 => row
            .try_get::<_, i32>(idx)
            .map(|v| JsonValue::Number(v.into()))
            .unwrap_or(JsonValue::Null),
        Type::INT8 => row
            .try_get::<_, i64>(idx)
            .map(|v| JsonValue::Number(v.into()))
            .unwrap_or(JsonValue::Null),
        Type::FLOAT4 => row
            .try_get::<_, f32>(idx)
            .and_then(|v| {
                serde_json::Number::from_f64(v as f64)
                    .ok_or(postgres::Error::__private_api_timeout())
            })
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        Type::FLOAT8 => row
            .try_get::<_, f64>(idx)
            .and_then(|v| {
                serde_json::Number::from_f64(v).ok_or(postgres::Error::__private_api_timeout())
            })
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => row
            .try_get::<_, String>(idx)
            .map(JsonValue::String)
            .unwrap_or(JsonValue::Null),
        Type::JSON | Type::JSONB => row.try_get::<_, JsonValue>(idx).unwrap_or(JsonValue::Null),
        _ => {
            // For other types, try to get as string
            row.try_get::<_, String>(idx)
                .map(JsonValue::String)
                .unwrap_or(JsonValue::Null)
        }
    }
}

/// Remove a directory recursively with retries (robust for Windows)
pub fn remove_dir_all_with_retry(path: &std::path::Path) -> Result<()> {
    use std::time::Duration;
    
    let max_attempts = 20;
    let retry_interval = Duration::from_millis(250);

    for attempt in 1..=max_attempts {
        match std::fs::remove_dir_all(path) {
            Ok(_) => {
                return Ok(());
            }
            Err(e) if attempt < max_attempts => {
                tracing::debug!("Remove dir attempt {} failed: {}, retrying...", attempt, e);
                std::thread::sleep(retry_interval);
            }
            Err(e) => {
                // Check if path still exists - maybe it was gone
                if !path.exists() {
                     return Ok(());
                }
                return Err(DBCenterError::IoError(e));
            }
        }
    }
    Ok(())
}
