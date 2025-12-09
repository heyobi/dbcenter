use thiserror::Error;

#[derive(Error, Debug)]
pub enum DBCenterError {
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Database connection error: {0}")]
    ConnectionError(String),

    #[error("Query execution error: {0}")]
    QueryError(String),

    #[error("Binary download error: {0}")]
    BinaryError(String),

    #[error("Process error: {0}")]
    ProcessError(String),

    #[error("Extension error: {0}")]
    ExtensionError(String),

    #[error("Lock error")]
    LockError,

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("Invalid query: {0}")]
    InvalidQuery(String),

    #[error("Unknown error: {0}")]
    Unknown(String),
}

pub type Result<T> = std::result::Result<T, DBCenterError>;
