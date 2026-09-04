//! Typed TTC function codes and dispatch classification.
//!
//! The session loop must never ACK an unknown RPC as a successful empty DML.
//! Known no-ops (PING, CLOSE_CURSORS, SET_END_TO_END) still complete cleanly;
//! everything else that is not implemented returns [`Dispatch::Unimplemented`].

/// TTC `FUNCTION` message type (payload byte 2).
pub const MSG_FUNCTION: u8 = 0x03;
/// TTC `PIGGYBACK` message type (ojdbc deferred CLOSE_CURSORS, etc.).
pub const MSG_PIGGYBACK: u8 = 0x11;
/// TTC `PROTOCOL` negotiation.
pub const MSG_PROTOCOL: u8 = 0x01;
/// TTC `DATA_TYPES` negotiation.
pub const MSG_DATA_TYPES: u8 = 0x02;

/// TTC function code (payload byte 3 of a FUNCTION message).
///
/// Numbers match the Oracle thin-driver `TNS_FUNC_*` constants (python-oracledb
/// `impl/thin/constants.pxi`, ojdbc, ODP.NET).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FunctionCode {
    Reexecute = 0x04,
    Fetch = 0x05,
    Logoff = 0x09,
    Commit = 0x0e,
    Rollback = 0x0f,
    Oversion = 0x3b,
    ReexecuteAndFetch = 0x4e,
    Execute = 0x5e,
    CloseCursors = 0x69,
    AuthPhaseTwo = 0x73,
    AuthPhaseOne = 0x76,
    SetEndToEnd = 0x87,
    Ping = 0x93,
    SetSchema = 0x98,
    /// Not a known code we implement. Carries the raw byte for error text.
    Other(u8),
}

impl FunctionCode {
    pub fn from_u8(code: u8) -> Self {
        match code {
            0x04 => Self::Reexecute,
            0x05 => Self::Fetch,
            0x09 => Self::Logoff,
            0x0e => Self::Commit,
            0x0f => Self::Rollback,
            0x3b => Self::Oversion,
            0x4e => Self::ReexecuteAndFetch,
            0x5e => Self::Execute,
            0x69 => Self::CloseCursors,
            0x73 => Self::AuthPhaseTwo,
            0x76 => Self::AuthPhaseOne,
            0x87 => Self::SetEndToEnd,
            0x93 => Self::Ping,
            0x98 => Self::SetSchema,
            other => Self::Other(other),
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::Other(c) => c,
            Self::Reexecute => 0x04,
            Self::Fetch => 0x05,
            Self::Logoff => 0x09,
            Self::Commit => 0x0e,
            Self::Rollback => 0x0f,
            Self::Oversion => 0x3b,
            Self::ReexecuteAndFetch => 0x4e,
            Self::Execute => 0x5e,
            Self::CloseCursors => 0x69,
            Self::AuthPhaseTwo => 0x73,
            Self::AuthPhaseOne => 0x76,
            Self::SetEndToEnd => 0x87,
            Self::Ping => 0x93,
            Self::SetSchema => 0x98,
        }
    }

    /// How the session loop should treat this function after handshake.
    pub fn dispatch(self) -> Dispatch {
        match self {
            Self::Execute | Self::Reexecute | Self::ReexecuteAndFetch => Dispatch::Execute,
            Self::Fetch => Dispatch::Fetch,
            Self::Logoff => Dispatch::Logoff,
            Self::Commit | Self::Rollback => Dispatch::Transaction,
            Self::Oversion => Dispatch::Oversion,
            // Harmless session housekeeping. Clients (especially ojdbc) send
            // these constantly; they must stay successful no-ops.
            Self::Ping | Self::CloseCursors | Self::SetEndToEnd | Self::SetSchema => Dispatch::NoOp,
            Self::AuthPhaseOne | Self::AuthPhaseTwo => Dispatch::Unimplemented {
                ora: 1017,
                detail: "authentication function after session is established",
            },
            Self::Other(code) => Dispatch::Unimplemented {
                ora: 3001,
                detail: unimplemented_detail(code),
            },
        }
    }
}

/// Session-loop classification for a TTC function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    Execute,
    Fetch,
    Logoff,
    Transaction,
    Oversion,
    /// Complete the call with an empty success end-of-call (no backend work).
    NoOp,
    /// Return this Oracle error to the client. Never a success ACK.
    Unimplemented {
        ora: u32,
        detail: &'static str,
    },
}

fn unimplemented_detail(code: u8) -> &'static str {
    match code {
        // LOB locator RPCs (open/read/write/close family used by thick/thin).
        0x60 | 0x61 | 0x62 | 0x63 | 0x64 | 0x65 | 0x66 | 0x67 | 0x68 | 0x6a | 0x6b | 0x6c => {
            "LOB locators are not implemented"
        }
        // REF CURSOR / describe-any / piggybacked cursor ops commonly seen.
        0x2b | 0x2f | 0x47 => "SYS_REFCURSOR / nested cursor is not implemented",
        _ => "feature not implemented",
    }
}

/// Oracle SQL constructs that must fail with a clear ORA rather than a wrong
/// success. Conservative: only shapes that cannot be lowered correctly on the
/// selected backend. MariaDB Oracle mode can run `CREATE PACKAGE`; PostgreSQL
/// cannot. Autonomous-transaction blocks currently lower to a no-op and stay
/// accepted so existing corpus cases do not regress.
pub fn reject_unsupported_sql(
    sql: &str,
    backend: crate::backend::BackendKind,
) -> Option<(u32, &'static str)> {
    let up = sql.to_ascii_uppercase();
    let compact: String = up.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.contains("SYS_REFCURSOR") || compact.contains("REF CURSOR") {
        return Some((3001, "SYS_REFCURSOR is not implemented"));
    }
    if matches!(backend, crate::backend::BackendKind::Postgres)
        && (compact.contains("CREATE PACKAGE")
            || compact.contains("CREATE OR REPLACE PACKAGE")
            || compact.contains("ALTER PACKAGE"))
    {
        return Some((3001, "PL/SQL packages are not implemented"));
    }
    if compact.contains("PIPELINED") {
        return Some((3001, "pipelined functions are not implemented"));
    }
    if compact.contains("BULK COLLECT") || compact.contains(" FORALL ") {
        return Some((3001, "BULK COLLECT / FORALL is not implemented"));
    }
    if compact.contains("DBMS_LOB.READ")
        || compact.contains("DBMS_LOB.WRITE")
        || compact.contains("DBMS_LOB.OPEN")
        || compact.contains("DBMS_LOB.CLOSE")
    {
        return Some((3001, "LOB locators are not implemented"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_function_is_unimplemented_ora_3001() {
        match FunctionCode::from_u8(0xff).dispatch() {
            Dispatch::Unimplemented { ora: 3001, .. } => {}
            other => panic!("expected Unimplemented, got {other:?}"),
        }
    }

    #[test]
    fn ping_and_close_cursors_are_success_noops() {
        assert_eq!(FunctionCode::Ping.dispatch(), Dispatch::NoOp);
        assert_eq!(FunctionCode::CloseCursors.dispatch(), Dispatch::NoOp);
        assert_eq!(FunctionCode::SetEndToEnd.dispatch(), Dispatch::NoOp);
        assert_eq!(FunctionCode::SetSchema.dispatch(), Dispatch::NoOp);
    }

    #[test]
    fn execute_family_is_classified() {
        assert_eq!(FunctionCode::Execute.dispatch(), Dispatch::Execute);
        assert_eq!(FunctionCode::Reexecute.dispatch(), Dispatch::Execute);
        assert_eq!(FunctionCode::Fetch.dispatch(), Dispatch::Fetch);
        assert_eq!(FunctionCode::Commit.dispatch(), Dispatch::Transaction);
        assert_eq!(FunctionCode::Oversion.dispatch(), Dispatch::Oversion);
    }

    #[test]
    fn lob_function_codes_are_explicit() {
        match FunctionCode::from_u8(0x60).dispatch() {
            Dispatch::Unimplemented { ora: 3001, detail } => {
                assert!(detail.contains("LOB"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reject_package_and_refcursor_sql() {
        use crate::backend::BackendKind;
        assert!(
            reject_unsupported_sql("CREATE PACKAGE emp_pkg AS END;", BackendKind::Postgres)
                .is_some()
        );
        assert!(
            reject_unsupported_sql("CREATE PACKAGE emp_pkg AS END;", BackendKind::MariaDb)
                .is_none()
        );
        assert!(
            reject_unsupported_sql(
                "CREATE PROCEDURE p (c OUT SYS_REFCURSOR) AS BEGIN NULL; END;",
                BackendKind::MariaDb
            )
            .is_some()
        );
        assert!(reject_unsupported_sql("SELECT 1 FROM dual", BackendKind::Postgres).is_none());
        assert!(reject_unsupported_sql("BEGIN NULL; END;", BackendKind::Postgres).is_none());
        assert!(
            reject_unsupported_sql(
                "DECLARE PRAGMA AUTONOMOUS_TRANSACTION; BEGIN NULL; END;",
                BackendKind::Postgres
            )
            .is_none()
        );
    }
}
