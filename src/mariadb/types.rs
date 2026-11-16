//! MariaDB result columns → Oracle describe metadata and row values.
//!
//! The describe is decided once per result set from MariaDB's column
//! definitions; every row is then encoded with the same [`Wire`] decision, so
//! metadata and values always agree, whichever protocol (text or binary)
//! produced the row.

use chrono::{NaiveDate, NaiveDateTime};
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::{Column, Value};

use crate::backend::DescribeCaps;
use crate::error::{Error, Result};
use crate::translate::IdentifierCase;
use crate::wire::{ColumnMeta, encode_oracle_number_decimal, encode_oracle_number_i64};

/// Largest size reported for a character or binary column (Oracle's extended
/// `VARCHAR2`/`RAW` limit). Bigger values are still sent whole.
const MAX_REPORTED_SIZE: u32 = 32_767;
/// MariaDB's binary character set id.
const BINARY_CHARSET: u16 = 63;
/// `DECIMAL(65,30)` is how DbSaci stores an unconstrained Oracle `NUMBER`.
const UNCONSTRAINED_NUMBER: (u32, u8) = (65, 30);

/// How one result column travels to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    Number,
    BinaryDouble,
    BinaryFloat,
    /// Oracle DATE (7 bytes).
    Date,
    /// Oracle TIMESTAMP (11 bytes).
    Timestamp,
    /// Character data (VARCHAR2 / CHAR / CLOB-ish text).
    Text,
    /// MariaDB TIME, rendered `HH:MM:SS[.ffffff]` as text.
    Time,
    /// Binary data (RAW).
    Raw,
    /// MariaDB BIT, sent as an Oracle NUMBER.
    Bit,
}

/// Describe one column and pick its [`Wire`] form.
pub fn describe(col: &Column, caps: &DescribeCaps, case: IdentifierCase) -> (ColumnMeta, Wire) {
    use ColumnType::*;
    let name = crate::translate::names::oracle_name(&col.name_str(), case);
    let from_table = !col.org_table_str().is_empty();
    let nullable = !col.flags().contains(ColumnFlags::NOT_NULL_FLAG);
    let unsigned = col.flags().contains(ColumnFlags::UNSIGNED_FLAG);
    let kind = col.column_type();
    let (mut meta, wire) = match kind {
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => {
            let scale = col.decimals();
            // column_length counts digits, plus one for the decimal point when
            // there is a fractional part, plus one for the sign when signed.
            let precision = col
                .column_length()
                .saturating_sub(u32::from(scale > 0))
                .saturating_sub(u32::from(!unsigned));
            let (p, s) = if !caps.report_number_scale
                || !from_table
                || (precision, scale) == UNCONSTRAINED_NUMBER
                || precision > 38
                || precision == 0
            {
                unconstrained(caps)
            } else {
                (precision as i8, scale as i8)
            };
            (ColumnMeta::number(name, p, s), Wire::Number)
        }
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
        | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_YEAR => {
            let digits: i8 = match kind {
                MYSQL_TYPE_TINY => 3,
                MYSQL_TYPE_SHORT => 5,
                MYSQL_TYPE_INT24 => 8,
                MYSQL_TYPE_LONG => 10,
                MYSQL_TYPE_YEAR => 4,
                _ => 19,
            };
            let (p, s) = if from_table && caps.report_number_scale {
                (digits, 0)
            } else {
                unconstrained(caps)
            };
            (ColumnMeta::number(name, p, s), Wire::Number)
        }
        MYSQL_TYPE_DOUBLE | MYSQL_TYPE_FLOAT if from_table && caps.native_binary_floats => {
            let double = kind == MYSQL_TYPE_DOUBLE;
            let size = if double { 8 } else { 4 };
            let mut m = ColumnMeta::number(name, 0, 0);
            m.oracle_type = if double { 101 } else { 100 };
            m.buffer_size = size;
            m.max_size = size;
            (
                m,
                if double {
                    Wire::BinaryDouble
                } else {
                    Wire::BinaryFloat
                },
            )
        }
        MYSQL_TYPE_DOUBLE | MYSQL_TYPE_FLOAT => {
            let (p, s) = unconstrained(caps);
            (ColumnMeta::number(name, p, s), Wire::Number)
        }
        MYSQL_TYPE_BIT => (ColumnMeta::number(name, 0, -127), Wire::Bit),
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => (ColumnMeta::date(name), Wire::Date),
        MYSQL_TYPE_DATETIME
        | MYSQL_TYPE_DATETIME2
        | MYSQL_TYPE_TIMESTAMP
        | MYSQL_TYPE_TIMESTAMP2 => {
            let decimals = col.decimals();
            if decimals > 0 && decimals <= 6 && caps.native_timestamps && !caps.datetime_as_date {
                let scale = if caps.timestamp_scale_zero {
                    0
                } else {
                    decimals as i8
                };
                (ColumnMeta::timestamp(name, scale), Wire::Timestamp)
            } else {
                (ColumnMeta::date(name), Wire::Date)
            }
        }
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => (ColumnMeta::varchar(name, 32), Wire::Time),
        _ if col.character_set() == BINARY_CHARSET && is_string_family(kind) => {
            let size = (col.column_length()).clamp(1, MAX_REPORTED_SIZE);
            (ColumnMeta::raw(name, size), Wire::Raw)
        }
        MYSQL_TYPE_STRING
            if col.flags().contains(ColumnFlags::ENUM_FLAG)
                || col.flags().contains(ColumnFlags::SET_FLAG) =>
        {
            (ColumnMeta::varchar(name, char_length(col)), Wire::Text)
        }
        MYSQL_TYPE_STRING if from_table => {
            let mut m = ColumnMeta::varchar(name, char_length(col));
            m.oracle_type = 96; // CHAR
            (m, Wire::Text)
        }
        _ => (ColumnMeta::varchar(name, char_length(col)), Wire::Text),
    };
    meta.nullable = nullable;
    (meta, wire)
}

/// The describe of an unconstrained Oracle NUMBER. Clients that predate real
/// NUMBER describes get DbSaci's historical `(38, 0)`.
fn unconstrained(caps: &DescribeCaps) -> (i8, i8) {
    if caps.report_number_scale {
        (0, -127)
    } else {
        (38, 0)
    }
}

fn is_string_family(kind: ColumnType) -> bool {
    use ColumnType::*;
    matches!(
        kind,
        MYSQL_TYPE_TINY_BLOB
            | MYSQL_TYPE_MEDIUM_BLOB
            | MYSQL_TYPE_LONG_BLOB
            | MYSQL_TYPE_BLOB
            | MYSQL_TYPE_STRING
            | MYSQL_TYPE_VAR_STRING
            | MYSQL_TYPE_VARCHAR
            | MYSQL_TYPE_GEOMETRY
    )
}

/// A character column's length in characters (MariaDB reports bytes).
fn char_length(col: &Column) -> u32 {
    let bytes_per_char = match col.character_set() {
        45 | 46 | 224..=247 | 255..=309 => 4, // utf8mb4 collations
        33 | 83 | 192..=223 => 3,             // utf8mb3 collations
        _ => 1,
    };
    (col.column_length() / bytes_per_char).clamp(1, MAX_REPORTED_SIZE)
}

/// Encode one value for a column described as `wire`. `None` is SQL NULL.
pub fn encode(value: &Value, wire: Wire) -> Result<Option<Vec<u8>>> {
    if matches!(value, Value::NULL) {
        return Ok(None);
    }
    let bytes = match wire {
        Wire::Number => match value {
            Value::Bytes(b) => encode_oracle_number_decimal(utf8(b)?)?,
            Value::Int(v) => encode_oracle_number_i64(*v),
            Value::UInt(v) => encode_oracle_number_decimal(&v.to_string())?,
            Value::Float(v) => encode_oracle_number_decimal(&finite(f64::from(*v))?)?,
            Value::Double(v) => encode_oracle_number_decimal(&finite(*v)?)?,
            other => return Err(conv("NUMBER", other)),
        },
        Wire::BinaryDouble => crate::backend::encode_binary_double(as_f64(value)?).to_vec(),
        Wire::BinaryFloat => crate::backend::encode_binary_float(as_f64(value)? as f32).to_vec(),
        Wire::Date => crate::backend::encode_oracle_date(as_datetime(value)?),
        Wire::Timestamp => crate::backend::encode_oracle_timestamp(as_datetime(value)?, None),
        Wire::Time => match value {
            Value::Time(neg, days, h, m, s, us) => {
                let hours = u32::from(*h) + days * 24;
                let mut t = format!("{}{hours:02}:{m:02}:{s:02}", if *neg { "-" } else { "" });
                if *us > 0 {
                    t.push_str(&format!(".{us:06}"));
                }
                t.into_bytes()
            }
            Value::Bytes(b) => b.clone(),
            other => return Err(conv("TIME", other)),
        },
        Wire::Bit => match value {
            Value::Bytes(b) => {
                let n = b.iter().fold(0u64, |acc, x| (acc << 8) | u64::from(*x));
                encode_oracle_number_decimal(&n.to_string())?
            }
            Value::Int(v) => encode_oracle_number_i64(*v),
            Value::UInt(v) => encode_oracle_number_decimal(&v.to_string())?,
            other => return Err(conv("BIT", other)),
        },
        Wire::Text | Wire::Raw => match value {
            Value::Bytes(b) => b.clone(),
            Value::Int(v) => v.to_string().into_bytes(),
            Value::UInt(v) => v.to_string().into_bytes(),
            Value::Float(v) => v.to_string().into_bytes(),
            Value::Double(v) => v.to_string().into_bytes(),
            Value::Date(..) => as_datetime(value)?
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
                .into_bytes(),
            Value::Time(..) => return encode(value, Wire::Time),
            Value::NULL => unreachable!(),
        },
    };
    Ok(Some(bytes))
}

fn utf8(b: &[u8]) -> Result<&str> {
    std::str::from_utf8(b).map_err(|e| Error::DataConversionError(e.to_string()))
}

fn finite(v: f64) -> Result<String> {
    if v.is_finite() {
        Ok(v.to_string())
    } else {
        Err(Error::ora(1426, "numeric overflow (non-finite value)"))
    }
}

fn as_f64(value: &Value) -> Result<f64> {
    match value {
        Value::Double(v) => Ok(*v),
        Value::Float(v) => Ok(f64::from(*v)),
        Value::Int(v) => Ok(*v as f64),
        Value::UInt(v) => Ok(*v as f64),
        Value::Bytes(b) => utf8(b)?
            .trim()
            .parse()
            .map_err(|_| conv("BINARY_DOUBLE", value)),
        other => Err(conv("BINARY_DOUBLE", other)),
    }
}

fn as_datetime(value: &Value) -> Result<NaiveDateTime> {
    match value {
        Value::Date(y, mo, d, h, mi, s, us) => {
            NaiveDate::from_ymd_opt(i32::from(*y), u32::from(*mo), u32::from(*d))
                .and_then(|date| {
                    date.and_hms_micro_opt(u32::from(*h), u32::from(*mi), u32::from(*s), *us)
                })
                .ok_or_else(|| conv("DATE", value))
        }
        Value::Bytes(b) => parse_datetime(utf8(b)?).ok_or_else(|| conv("DATE", value)),
        other => Err(conv("DATE", other)),
    }
}

/// `2024-02-29 13:14:15[.ffffff]` / `2024-02-29`.
pub fn parse_datetime(text: &str) -> Option<NaiveDateTime> {
    let text = text.trim();
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S"))
        .or_else(|_| {
            NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .map(|d| d.and_hms_opt(0, 0, 0).expect("midnight"))
        })
        .ok()
}

fn conv(target: &str, value: &Value) -> Error {
    Error::DataConversionError(format!(
        "cannot convert MariaDB value {value:?} to {target}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_binary_protocol_datetime_with_micros() {
        let v = Value::Date(2024, 2, 29, 13, 14, 15, 123_456);
        let enc = encode(&v, Wire::Timestamp).unwrap().unwrap();
        assert_eq!(enc.len(), 11);
        // nanoseconds field = 123_456_000
        assert_eq!(
            u32::from_be_bytes([enc[7], enc[8], enc[9], enc[10]]),
            123_456_000
        );
        assert_eq!(encode(&v, Wire::Date).unwrap().unwrap().len(), 7);
    }

    #[test]
    fn time_is_text_not_date_bytes() {
        let v = Value::Time(false, 1, 2, 3, 4, 0);
        assert_eq!(
            encode(&v, Wire::Time).unwrap().unwrap(),
            b"26:03:04".to_vec()
        );
    }

    #[test]
    fn null_is_none_and_numbers_encode() {
        assert_eq!(encode(&Value::NULL, Wire::Number).unwrap(), None);
        assert!(
            encode(&Value::Bytes(b"1.5".to_vec()), Wire::Number)
                .unwrap()
                .is_some()
        );
        assert!(encode(&Value::Double(f64::NAN), Wire::Number).is_err());
    }
}
