use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq)]
pub enum DbError {
    /// Error reported by the server for a statement.
    #[error("{message}")]
    Server {
        message: String,
        code: Option<String>,
        /// 1-based character position inside the failing statement, when the driver reports it.
        position: Option<usize>,
    },
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("connection lost: {0}")]
    ConnectionLost(String),
    #[error("query cancelled")]
    Cancelled,
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Io(String),
}

impl DbError {
    pub fn server(message: impl Into<String>) -> Self {
        DbError::Server { message: message.into(), code: None, position: None }
    }
}

impl From<std::io::Error> for DbError {
    fn from(e: std::io::Error) -> Self {
        DbError::Io(e.to_string())
    }
}

pub type DbResult<T> = Result<T, DbError>;
