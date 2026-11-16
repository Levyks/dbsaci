//! MariaDB server errors → Oracle errors, by error number.
//!
//! Mapping on MariaDB's `errno` (not on message text) keeps the Oracle code
//! stable across server versions and locales. The message keeps MariaDB's
//! text so the cause stays visible; the ORA code is what applications branch
//! on (`ORA-00001`, `ORA-02292`, `ORA-00060`, …).

use crate::error::Error;

/// What the failed statement was, where the Oracle code depends on it
/// (NOT NULL on INSERT is ORA-01400, on UPDATE ORA-01407; a lock timeout under
/// `NOWAIT` is ORA-00054, under `WAIT n` ORA-30006).
#[derive(Debug, Clone, Copy, Default)]
pub struct StatementShape {
    pub update: bool,
    pub nowait: bool,
}

impl StatementShape {
    pub fn of(sql: &str) -> Self {
        let head = sql.trim_start();
        let up = head
            .get(..head.len().min(16))
            .unwrap_or(head)
            .to_ascii_uppercase();
        let tail_up = sql.to_ascii_uppercase();
        StatementShape {
            update: up.starts_with("UPDATE"),
            nowait: tail_up.contains("NOWAIT"),
        }
    }
}

/// The Oracle error for a MariaDB server error. `rolled_back` tells the
/// caller that MariaDB ended the whole transaction (deadlock), which Oracle
/// would not have done; the message says so.
pub struct Mapped {
    pub error: Error,
    pub transaction_rolled_back: bool,
}

/// Map a server error (`code`, SQLSTATE, message).
pub fn map_server_error(
    code: u16,
    state: &str,
    message: &str,
    sql: &str,
    shape: StatementShape,
) -> Mapped {
    let mut rolled_back = false;
    let ora: u32 = match code {
        1062 | 1586 => 1,             // duplicate key: unique constraint violated
        1451 => 2292,                 // cannot delete/update a parent row: child record found
        1452 | 1216 => 2291,          // cannot add/update a child row: parent key not found
        1217 => 2292,                 // cannot delete parent (older code)
        1048 if shape.update => 1407, // cannot update to NULL
        1048 | 1364 => 1400,          // cannot insert NULL / no default value
        1406 => 12899,                // data too long: value too large for column
        1264 | 1690 => 1438,          // out of range: value larger than specified precision
        1365 => 1476,                 // division by 0
        1213 => {
            rolled_back = true;
            60 // deadlock detected
        }
        1205 if shape.nowait => 54, // resource busy and acquire with NOWAIT
        1205 => 30006,              // resource busy; acquire with WAIT timeout expired
        3572 => 54,                 // statement aborted: NOWAIT lock could not be acquired
        1146 | 1051 | 1109 => 942,  // table or view does not exist
        1049 => 1918,               // unknown database: user/schema does not exist
        1054 => 904,                // unknown column: invalid identifier
        1305 if message.contains("FUNCTION") || message.contains("PROCEDURE") => 904,
        1305 => 1086,                     // savepoint never established
        1050 | 1304 | 1359 | 4091 => 955, // name is already used by an existing object
        1052 => 918,                      // column ambiguously defined
        1136 => 947,                      // column count mismatch: not enough/too many values
        1055 => 979,                      // not a GROUP BY expression
        1242 => 1427,                     // single-row subquery returns more than one row
        1172 => 1422,                     // exact fetch returns more than requested number of rows
        1329 => 1403,                     // no data found
        1317 | 1969 | 3024 => 1013,       // interrupted / max_statement_time: user requested cancel
        1292 | 1411 if is_temporal_message(message) => 1858,
        1292 | 1366 | 1265 => 1722, // incorrect / truncated value: invalid number
        1139 | 1210 if message.to_ascii_lowercase().contains("regex") => 12726,
        3819 | 4025 => 2290,                      // check constraint violated
        1142 | 1143 | 1044 | 1227 | 1370 => 1031, // insufficient privileges
        1045 => 1017,                             // invalid username/password
        1064 | 1149 => 900,                       // syntax error: invalid SQL statement
        1644 => {
            // SIGNAL from translated RAISE_APPLICATION_ERROR / DbSaci helpers:
            // MESSAGE_TEXT carries `ORA-nnnnn: text`.
            if let Some((code, text)) = parse_ora_prefix(message) {
                return Mapped {
                    error: Error::Ora {
                        code,
                        message: text.to_string(),
                        position: None,
                    },
                    transaction_rolled_back: false,
                };
            }
            20000
        }
        1834 | 1836 | 1792 => 1031, // read-only transaction / table
        2006 | 2013 => 3113,        // server gone away / lost connection
        _ => match state {
            "23000" => 1,
            "22001" => 12899,
            "22003" => 1438,
            "22012" => 1476,
            "22007" | "22008" => 1858,
            "40001" => 60,
            "42S02" => 942,
            "42S22" => 904,
            "42S01" => 955,
            "21000" => 1427,
            _ => 900,
        },
    };
    let mut text = message.to_string();
    if rolled_back {
        text.push_str(" (the backend rolled back the whole transaction)");
    }
    Mapped {
        error: Error::Ora {
            code: ora,
            message: text,
            position: estimate_position(sql, message),
        },
        transaction_rolled_back: rolled_back,
    }
}

/// Map any `mysql_async` error for `sql`.
pub fn map_error(e: mysql_async::Error, sql: &str) -> Mapped {
    match e {
        mysql_async::Error::Server(server) => map_server_error(
            server.code,
            &server.state,
            &server.message,
            sql,
            StatementShape::of(sql),
        ),
        other => Mapped {
            error: if is_link_error(&other) {
                Error::Postgres(format!("connection lost: {other}"))
            } else {
                Error::Postgres(format!("MariaDB error: {other}"))
            },
            transaction_rolled_back: false,
        },
    }
}

/// Does this driver error mean the network link to MariaDB is gone?
pub fn is_link_error(e: &mysql_async::Error) -> bool {
    match e {
        mysql_async::Error::Io(_) => true,
        mysql_async::Error::Driver(d) => {
            let m = d.to_string().to_ascii_lowercase();
            m.contains("connection") || m.contains("eof")
        }
        mysql_async::Error::Server(s) => matches!(s.code, 2006 | 2013 | 1927 | 4031),
        _ => false,
    }
}

fn is_temporal_message(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("date") || m.contains("time")
}

/// `ORA-20001: boom` → `(20001, "boom")`.
fn parse_ora_prefix(message: &str) -> Option<(u32, &str)> {
    let rest = message.trim_start().strip_prefix("ORA-")?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let code: u32 = rest[..digits].parse().ok()?;
    let text = rest[digits..].trim_start_matches(':').trim_start();
    Some((code, text))
}

/// Best-effort 1-based offset into `sql` of the identifier a MariaDB message
/// names (`Unknown column 'x'`, `Table 'db.t' doesn't exist`).
fn estimate_position(sql: &str, message: &str) -> Option<u32> {
    let start = message.find('\'')? + 1;
    let end = start + message[start..].find('\'')?;
    let quoted = &message[start..end];
    let name = quoted.rsplit('.').next().unwrap_or(quoted);
    if name.is_empty() {
        return None;
    }
    let hay = sql.to_ascii_uppercase();
    hay.find(&name.to_ascii_uppercase())
        .map(|i| sql[..i].chars().count() as u32 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(errno: u16, state: &str, msg: &str, sql: &str) -> u32 {
        match map_server_error(errno, state, msg, sql, StatementShape::of(sql)).error {
            Error::Ora { code, .. } => code,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn maps_by_errno() {
        assert_eq!(
            code(1062, "23000", "Duplicate entry", "INSERT INTO t VALUES (1)"),
            1
        );
        assert_eq!(
            code(
                1451,
                "23000",
                "Cannot delete or update a parent row: a foreign key constraint fails",
                "DELETE FROM p"
            ),
            2292
        );
        assert_eq!(
            code(
                1452,
                "23000",
                "Cannot add or update a child row: a foreign key constraint fails",
                "INSERT INTO c VALUES (9)"
            ),
            2291
        );
        assert_eq!(
            code(
                1048,
                "23000",
                "Column 'a' cannot be null",
                "INSERT INTO t VALUES (NULL)"
            ),
            1400
        );
        assert_eq!(
            code(
                1048,
                "23000",
                "Column 'a' cannot be null",
                "UPDATE t SET a = NULL"
            ),
            1407
        );
        assert_eq!(
            code(1213, "40001", "Deadlock found", "UPDATE t SET a = 1"),
            60
        );
        assert_eq!(
            code(
                1205,
                "HY000",
                "Lock wait timeout exceeded",
                "SELECT * FROM t FOR UPDATE NOWAIT"
            ),
            54
        );
        assert_eq!(
            code(
                1205,
                "HY000",
                "Lock wait timeout exceeded",
                "SELECT * FROM t FOR UPDATE WAIT 3"
            ),
            30006
        );
        assert_eq!(
            code(
                1146,
                "42S02",
                "Table 'APP.X' doesn't exist",
                "SELECT * FROM X"
            ),
            942
        );
        assert_eq!(
            code(1292, "22007", "Incorrect datetime value: 'x'", "SELECT 1"),
            1858
        );
        assert_eq!(
            code(
                1292,
                "22007",
                "Truncated incorrect DECIMAL value: 'x'",
                "SELECT 1"
            ),
            1722
        );
    }

    #[test]
    fn signal_carries_ora_code() {
        let m = map_server_error(
            1644,
            "45000",
            "ORA-20001: custom failure",
            "BEGIN ... END",
            StatementShape::default(),
        );
        match m.error {
            Error::Ora { code, message, .. } => {
                assert_eq!(code, 20001);
                assert_eq!(message, "custom failure");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn deadlock_reports_rollback() {
        let m = map_server_error(
            1213,
            "40001",
            "Deadlock found when trying to get lock",
            "UPDATE t SET a=1",
            StatementShape::default(),
        );
        assert!(m.transaction_rolled_back);
    }

    #[test]
    fn position_of_named_identifier() {
        assert_eq!(
            estimate_position("SELECT nope FROM t", "Unknown column 'nope' in 'SELECT'"),
            Some(8)
        );
    }
}
