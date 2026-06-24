use crate::DatabaseError;

#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

#[cfg(feature = "sqlite")]
pub async fn run_sqlite_migrations(
    pool: &crate::sqlx::SqlitePool,
    migrations: &[Migration],
) -> Result<(), DatabaseError> {
    crate::sqlx::query(
        "CREATE TABLE IF NOT EXISTS _base_db_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at_ms INTEGER NOT NULL)",
    )
    .execute(pool)
    .await?;
    for migration in migrations {
        let mut transaction = pool.begin().await?;
        let existing = crate::sqlx::query_scalar::<_, String>(
            "SELECT name FROM _base_db_migrations WHERE version = ?",
        )
        .bind(migration.version)
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(existing) = existing {
            ensure_migration_matches(migration, existing)?;
            transaction.commit().await?;
            continue;
        }
        crate::sqlx::raw_sql(migration.sql)
            .execute(&mut *transaction)
            .await?;
        crate::sqlx::query(
            "INSERT INTO _base_db_migrations(version, name, applied_at_ms) VALUES (?, ?, ?)",
        )
        .bind(migration.version)
        .bind(migration.name)
        .bind(now_ms())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
    }
    Ok(())
}

#[cfg(feature = "mysql")]
pub async fn run_mysql_migrations(
    pool: &crate::sqlx::MySqlPool,
    migrations: &[Migration],
) -> Result<(), DatabaseError> {
    crate::sqlx::query(
        "CREATE TABLE IF NOT EXISTS _base_db_migrations (version BIGINT PRIMARY KEY, name VARCHAR(255) NOT NULL, applied_at_ms BIGINT NOT NULL)",
    )
    .execute(pool)
    .await?;
    for migration in migrations {
        let mut transaction = pool.begin().await?;
        let existing = crate::sqlx::query_scalar::<_, String>(
            "SELECT name FROM _base_db_migrations WHERE version = ?",
        )
        .bind(migration.version)
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(existing) = existing {
            ensure_migration_matches(migration, existing)?;
            transaction.commit().await?;
            continue;
        }
        crate::sqlx::raw_sql(migration.sql)
            .execute(&mut *transaction)
            .await?;
        crate::sqlx::query(
            "INSERT INTO _base_db_migrations(version, name, applied_at_ms) VALUES (?, ?, ?)",
        )
        .bind(migration.version)
        .bind(migration.name)
        .bind(now_ms())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
    }
    Ok(())
}

fn ensure_migration_matches(migration: &Migration, existing: String) -> Result<(), DatabaseError> {
    if existing == migration.name {
        Ok(())
    } else {
        Err(DatabaseError::MigrationConflict {
            version: migration.version,
            expected: existing,
            actual: migration.name.to_string(),
        })
    }
}

fn now_ms() -> i64 {
    let milliseconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(milliseconds).unwrap_or(i64::MAX)
}
