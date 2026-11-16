use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Protocol(String),
    BufferUnderflow {
        needed: usize,
        available: usize,
    },
    BufferOverflow {
        needed: usize,
        available: usize,
    },
    InvalidLengthIndicator(u8),
    InvalidPacketType(u8),
    InvalidMessageType(u8),
    AuthenticationFailed(String),
    DataConversionError(String),
    Postgres(String),
    /// A PostgreSQL error raised while running a client statement. Carries the
    /// server's 1-based character position into the query text, when it gave
    /// one, so it can be surfaced in the Oracle `error_pos` field.
    PgStatement {
        detail: String,
        position: Option<u32>,
    },
    SqlParse(String),
    /// An error that already has its Oracle identity: `ORA-<code>: <message>`
    /// is what the client sees. `position` is the 1-based offset into the
    /// statement, when known.
    Ora {
        code: u32,
        message: String,
        position: Option<u32>,
    },
    /// The backend connection was lost with uncommitted work: the session
    /// cannot continue (ORA-03113), and the client connection is closed.
    SessionLost(String),
}

impl Error {
    /// An `Error::Ora` with no position.
    pub fn ora(code: u32, message: impl Into<String>) -> Self {
        Error::Ora {
            code,
            message: message.into(),
            position: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Protocol(s) => write!(f, "protocol error: {s}"),
            Error::BufferUnderflow { needed, available } => {
                write!(
                    f,
                    "buffer underflow: needed {needed}, available {available}"
                )
            }
            Error::BufferOverflow { needed, available } => {
                write!(f, "buffer overflow: needed {needed}, available {available}")
            }
            Error::InvalidLengthIndicator(b) => write!(f, "invalid length indicator {b}"),
            Error::InvalidPacketType(b) => write!(f, "invalid packet type {b}"),
            Error::InvalidMessageType(b) => write!(f, "invalid message type {b}"),
            Error::AuthenticationFailed(s) => write!(f, "authentication failed: {s}"),
            Error::DataConversionError(s) => write!(f, "data conversion error: {s}"),
            Error::Postgres(s) => write!(f, "postgres error: {s}"),
            Error::PgStatement { detail, .. } => write!(f, "postgres error: {detail}"),
            Error::SqlParse(s) => write!(f, "sql parse error: {s}"),
            Error::Ora { code, message, .. } => write!(f, "ORA-{code:05}: {message}"),
            Error::SessionLost(s) => write!(f, "ORA-03113: end-of-file on communication channel ({s})"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<tokio_postgres::Error> for Error {
    fn from(e: tokio_postgres::Error) -> Self {
        Error::Postgres(e.to_string())
    }
}

impl From<sqlparser::parser::ParserError> for Error {
    fn from(e: sqlparser::parser::ParserError) -> Self {
        Error::SqlParse(e.to_string())
    }
}

impl From<String> for Error {
    fn from(e: String) -> Self {
        Error::AuthenticationFailed(e)
    }
}
