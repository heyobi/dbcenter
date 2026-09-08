use crate::error::{DBCenterError, Result};
use postgres::types::Type;
use postgres::Row;
use serde_json::Value as JsonValue;
use std::net::TcpListener;

/// Get a free port for PostgreSQL
pub fn get_free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(DBCenterError::IoError)?;

    let port = listener
        .local_addr()
        .map_err(DBCenterError::IoError)?
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
        // Client-side failures wrap their cause and print only the outer layer,
        // so a parameter that could not be encoded arrives as "error serializing
        // parameter 0" with no word of why. Walk the chain and say.
        let mut formatted = error.to_string();
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            let text = cause.to_string();
            if !formatted.contains(&text) {
                formatted.push_str(": ");
                formatted.push_str(&text);
            }
            source = cause.source();
        }
        formatted
    }
}

/// Convert a PostgreSQL row value to JSON
/// PostgreSQL's binary NUMERIC, decoded by hand.
///
/// rust-postgres only maps NUMERIC when it is built against a decimal crate,
/// and `postgres` 0.19 offers no such feature. Without this the value fell
/// through to the "try it as a string" branch, failed, and came back as null -
/// silently, which for a money column is the worst outcome available.
///
/// The wire format is: `ndigits`, `weight`, `sign`, `dscale`, then `ndigits`
/// groups of four decimal digits held as base-10000 shorts. `weight` is the
/// base-10000 exponent of the first group.
struct PgNumeric(String);

impl<'a> postgres::types::FromSql<'a> for PgNumeric {
    fn from_sql(
        _: &Type,
        raw: &'a [u8],
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() < 8 {
            return Err("NUMERIC value is too short".into());
        }
        let be16 = |i: usize| i16::from_be_bytes([raw[i], raw[i + 1]]);
        let ndigits = be16(0);
        let weight = be16(2);
        let sign = u16::from_be_bytes([raw[4], raw[5]]);
        let dscale = be16(6);

        if sign == 0xC000 {
            return Ok(PgNumeric("NaN".to_string()));
        }
        if ndigits < 0 || dscale < 0 {
            return Err("NUMERIC header is malformed".into());
        }
        let ndigits = ndigits as usize;
        if raw.len() < 8 + ndigits * 2 {
            return Err("NUMERIC value is truncated".into());
        }
        let digit = |i: isize| -> i16 {
            if i < 0 || i as usize >= ndigits {
                0
            } else {
                be16(8 + i as usize * 2)
            }
        };

        let mut out = String::new();
        if sign == 0x4000 {
            out.push('-');
        }

        // Integer part: every group from the first down to the units group.
        if weight < 0 {
            out.push('0');
        } else {
            for i in 0..=weight as isize {
                if i == 0 {
                    out.push_str(&digit(i).to_string());
                } else {
                    out.push_str(&format!("{:04}", digit(i)));
                }
            }
        }

        // Fractional part: keep taking groups until dscale digits are covered.
        if dscale > 0 {
            let mut frac = String::new();
            let mut i = weight as isize + 1;
            while frac.len() < dscale as usize {
                frac.push_str(&format!("{:04}", digit(i)));
                i += 1;
            }
            frac.truncate(dscale as usize);
            out.push('.');
            out.push_str(&frac);
        }

        Ok(PgNumeric(out))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::NUMERIC
    }
}

/// Render a typed array as a JSON array, or null if the element type is not one
/// this maps.
fn array_to_json<'a, T>(row: &'a Row, idx: usize, f: impl Fn(T) -> JsonValue) -> JsonValue
where
    T: postgres::types::FromSql<'a>,
{
    match row.try_get::<_, Vec<Option<T>>>(idx) {
        Ok(values) => JsonValue::Array(
            values
                .into_iter()
                .map(|v| v.map(&f).unwrap_or(JsonValue::Null))
                .collect(),
        ),
        Err(_) => JsonValue::Null,
    }
}

fn number(v: f64) -> JsonValue {
    serde_json::Number::from_f64(v)
        .map(JsonValue::Number)
        .unwrap_or(JsonValue::Null)
}

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
        Type::UUID => row
            .try_get::<_, uuid::Uuid>(idx)
            .map(|id| JsonValue::String(id.to_string()))
            .unwrap_or(JsonValue::Null),
        Type::JSON | Type::JSONB => row.try_get::<_, JsonValue>(idx).unwrap_or(JsonValue::Null),

        // NUMERIC is the one that mattered most: it is what money and any exact
        // decimal is stored as. Rendered through f64 like any other JSON number,
        // which is lossy past 15 significant digits; the exact text is kept when
        // it will not fit.
        Type::NUMERIC => match row.try_get::<_, Option<PgNumeric>>(idx) {
            Ok(None) => JsonValue::Null,
            Ok(Some(PgNumeric(text))) => match text.parse::<f64>() {
                Ok(v) if serde_json::Number::from_f64(v).is_some() && text.len() <= 17 => number(v),
                _ => JsonValue::String(text),
            },
            Err(_) => JsonValue::Null,
        },

        Type::TIMESTAMPTZ => row
            .try_get::<_, chrono::DateTime<chrono::Utc>>(idx)
            .map(|v| JsonValue::String(v.to_rfc3339()))
            .unwrap_or(JsonValue::Null),
        Type::TIMESTAMP => row
            .try_get::<_, chrono::NaiveDateTime>(idx)
            .map(|v| JsonValue::String(v.to_string()))
            .unwrap_or(JsonValue::Null),
        Type::DATE => row
            .try_get::<_, chrono::NaiveDate>(idx)
            .map(|v| JsonValue::String(v.to_string()))
            .unwrap_or(JsonValue::Null),
        Type::TIME => row
            .try_get::<_, chrono::NaiveTime>(idx)
            .map(|v| JsonValue::String(v.to_string()))
            .unwrap_or(JsonValue::Null),

        // Hex rather than base64, so it matches what PostgreSQL itself prints.
        Type::BYTEA => row
            .try_get::<_, Vec<u8>>(idx)
            .map(|v| JsonValue::String(format!("\\x{}", hex::encode(v))))
            .unwrap_or(JsonValue::Null),

        Type::OID => row
            .try_get::<_, u32>(idx)
            .map(|v| JsonValue::Number(v.into()))
            .unwrap_or(JsonValue::Null),

        Type::BOOL_ARRAY => array_to_json::<bool>(row, idx, JsonValue::Bool),
        Type::INT2_ARRAY => array_to_json::<i16>(row, idx, |v| JsonValue::Number(v.into())),
        Type::INT4_ARRAY => array_to_json::<i32>(row, idx, |v| JsonValue::Number(v.into())),
        Type::INT8_ARRAY => array_to_json::<i64>(row, idx, |v| JsonValue::Number(v.into())),
        Type::FLOAT4_ARRAY => array_to_json::<f32>(row, idx, |v| number(v as f64)),
        Type::FLOAT8_ARRAY => array_to_json::<f64>(row, idx, number),
        Type::TEXT_ARRAY | Type::VARCHAR_ARRAY | Type::NAME_ARRAY => {
            array_to_json::<String>(row, idx, JsonValue::String)
        }
        Type::UUID_ARRAY => {
            array_to_json::<uuid::Uuid>(row, idx, |v| JsonValue::String(v.to_string()))
        }

        _ => {
            // Some textual types are still readable as a string; anything else
            // is reported rather than quietly turning into null, which is how a
            // whole column can go missing without anyone noticing.
            match row.try_get::<_, Option<String>>(idx) {
                Ok(Some(text)) => JsonValue::String(text),
                Ok(None) => JsonValue::Null,
                Err(_) => {
                    tracing::warn!(
                        "Column '{}' has type '{}', which is not mapped to JSON; returning null",
                        column.name(),
                        col_type.name()
                    );
                    JsonValue::Null
                }
            }
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
