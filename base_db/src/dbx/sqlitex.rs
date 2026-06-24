use std::path::PathBuf;

use crate::dbx::DatabasePoolConfig;
use crate::sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use crate::sqlx::SqlitePool;
use crate::DatabaseError;

#[derive(Debug, Clone)]
pub struct SqliteConnectionConfig {
    pub path: PathBuf,
    pub read_only: bool,
    pub create_if_missing: bool,
    pub foreign_keys: bool,
    pub journal_mode: SqliteJournalMode,
    pub synchronous: SqliteSynchronous,
    pub busy_timeout: std::time::Duration,
}

impl SqliteConnectionConfig {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            read_only: false,
            create_if_missing: true,
            foreign_keys: true,
            journal_mode: SqliteJournalMode::Wal,
            synchronous: SqliteSynchronous::Normal,
            busy_timeout: std::time::Duration::from_secs(5),
        }
    }

    fn connect_options(&self) -> SqliteConnectOptions {
        SqliteConnectOptions::new()
            .filename(&self.path)
            .read_only(self.read_only)
            .create_if_missing(self.create_if_missing && !self.read_only)
            .foreign_keys(self.foreign_keys)
            .journal_mode(self.journal_mode)
            .synchronous(self.synchronous)
            .busy_timeout(self.busy_timeout)
    }
}

pub fn build_sqlite_pool(
    connection: SqliteConnectionConfig,
    pool: DatabasePoolConfig,
) -> Result<SqlitePool, DatabaseError> {
    pool.validate()?;
    Ok(SqlitePoolOptions::new()
        .max_connections(pool.max_size)
        .min_connections(pool.min_idle.unwrap_or(0))
        .acquire_timeout(pool.connection_timeout)
        .max_lifetime(pool.max_lifetime)
        .idle_timeout(pool.idle_timeout)
        .test_before_acquire(pool.test_on_check_out)
        .connect_lazy_with(connection.connect_options()))
}
