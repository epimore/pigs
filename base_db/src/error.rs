use thiserror::Error;

#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error("invalid pool configuration: {0}")]
    InvalidPoolConfig(String),
    #[cfg(any(feature = "mysql", feature = "sqlite"))]
    #[error("database error: {0}")]
    Sqlx(#[from] crate::sqlx::Error),
    #[cfg(feature = "sqlite")]
    #[error("sqlite I/O error: {0}")]
    SqliteIo(#[from] std::io::Error),
    #[cfg(feature = "sqlite")]
    #[error("sqlite integrity check failed: {0}")]
    Integrity(String),
    #[error("migration conflict at version {version}: expected {expected}, found {actual}")]
    MigrationConflict {
        version: i64,
        expected: String,
        actual: String,
    },
}
