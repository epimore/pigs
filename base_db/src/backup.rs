use std::path::Path;

use crate::dbx::sqlitex::{build_sqlite_pool, SqliteConnectionConfig};
use crate::dbx::DatabasePoolConfig;
use crate::DatabaseError;

pub async fn backup_sqlite(
    pool: &crate::sqlx::SqlitePool,
    destination: impl AsRef<Path>,
) -> Result<(), DatabaseError> {
    let destination = destination.as_ref();
    ensure_parent_directory(destination)?;
    if destination.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "backup destination already exists: {}",
                destination.display()
            ),
        )
        .into());
    }
    let destination = destination.to_string_lossy().into_owned();
    crate::sqlx::query("VACUUM INTO ?")
        .bind(&destination)
        .execute(pool)
        .await?;
    let backup = open_read_only_pool(Path::new(&destination))?;
    let result = check_sqlite_integrity(&backup).await;
    backup.close().await;
    result
}

pub async fn restore_sqlite(
    source_path: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<(), DatabaseError> {
    let source_path = source_path.as_ref();
    let source = open_read_only_pool(source_path)?;
    check_sqlite_integrity(&source).await?;
    source.close().await;
    let destination = destination.as_ref();
    ensure_parent_directory(destination)?;
    std::fs::copy(source_path, destination)?;
    let restored = open_read_only_pool(destination)?;
    let result = check_sqlite_integrity(&restored).await;
    restored.close().await;
    result
}

pub async fn check_sqlite_integrity(pool: &crate::sqlx::SqlitePool) -> Result<(), DatabaseError> {
    let result: String = crate::sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(pool)
        .await?;
    if result.eq_ignore_ascii_case("ok") {
        Ok(())
    } else {
        Err(DatabaseError::Integrity(result))
    }
}

fn open_read_only_pool(path: &Path) -> Result<crate::sqlx::SqlitePool, DatabaseError> {
    if !path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("sqlite file does not exist: {}", path.display()),
        )
        .into());
    }
    let mut connection = SqliteConnectionConfig::new(path);
    connection.read_only = true;
    connection.create_if_missing = false;
    connection.journal_mode = crate::sqlx::sqlite::SqliteJournalMode::Delete;
    build_sqlite_pool(
        connection,
        DatabasePoolConfig {
            max_size: 1,
            min_idle: Some(0),
            ..DatabasePoolConfig::default()
        },
    )
}

fn ensure_parent_directory(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}
