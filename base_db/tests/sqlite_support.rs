#![cfg(feature = "sqlite")]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base_db::backup::{backup_sqlite, check_sqlite_integrity, restore_sqlite};
use base_db::dbx::sqlitex::{build_sqlite_pool, SqliteConnectionConfig};
use base_db::dbx::DatabasePoolConfig;
use base_db::health::check_sqlite;
use base_db::migration::{run_sqlite_migrations, Migration};
use base_db::sqlx::{Row, SqlitePool};

fn temp_path(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("base-db-{name}-{}-{unique}.db", std::process::id()))
}

fn pool(path: &Path) -> SqlitePool {
    build_sqlite_pool(
        SqliteConnectionConfig::new(path),
        DatabasePoolConfig {
            max_size: 4,
            min_idle: Some(1),
            connection_timeout: Duration::from_secs(2),
            max_lifetime: None,
            idle_timeout: None,
            test_on_check_out: true,
        },
    )
    .unwrap()
}

async fn cleanup(pool: SqlitePool, path: &Path) {
    pool.close().await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
    }
}

#[tokio::test]
async fn initializes_pragmas_and_health() {
    let path = temp_path("pragma");
    let pool = pool(&path);
    check_sqlite(&pool).await.unwrap();
    let row = base_db::sqlx::query("PRAGMA foreign_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    let foreign_keys: i64 = row.get(0);
    let journal_mode: String = base_db::sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(foreign_keys, 1);
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    cleanup(pool, &path).await;
}

#[derive(Debug, base_db::sqlx::FromRow)]
struct ValueRow {
    value: i64,
}

#[tokio::test]
async fn official_from_row_derive_is_reexported() {
    let path = temp_path("from-row");
    let pool = pool(&path);
    let row: ValueRow = base_db::sqlx::query_as("SELECT 9 AS value")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.value, 9);
    cleanup(pool, &path).await;
}

#[tokio::test]
async fn migrations_are_idempotent_and_detect_conflicts() {
    let path = temp_path("migration");
    let pool = pool(&path);
    let migrations = [Migration {
        version: 1,
        name: "create_items",
        sql: "CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
    }];
    run_sqlite_migrations(&pool, &migrations).await.unwrap();
    run_sqlite_migrations(&pool, &migrations).await.unwrap();
    let conflict = [Migration {
        version: 1,
        name: "different",
        sql: "SELECT 1;",
    }];
    assert!(run_sqlite_migrations(&pool, &conflict).await.is_err());
    cleanup(pool, &path).await;
}

#[tokio::test]
async fn backup_restore_and_isolated_files_work() {
    let source_path = temp_path("source");
    let other_path = temp_path("other");
    let backup_path = temp_path("backup");
    let source = pool(&source_path);
    base_db::sqlx::raw_sql("CREATE TABLE value(v INTEGER); INSERT INTO value VALUES (7);")
        .execute(&source)
        .await
        .unwrap();
    backup_sqlite(&source, &backup_path).await.unwrap();
    check_sqlite_integrity(&source).await.unwrap();
    let other = pool(&other_path);
    other.close().await;
    restore_sqlite(&backup_path, &other_path).await.unwrap();
    let restored = pool(&other_path);
    let value: i64 = base_db::sqlx::query_scalar("SELECT v FROM value")
        .fetch_one(&restored)
        .await
        .unwrap();
    assert_eq!(value, 7);
    cleanup(source, &source_path).await;
    cleanup(restored, &other_path).await;
    let _ = std::fs::remove_file(backup_path);
}

#[tokio::test]
async fn pool_supports_concurrent_transactions() {
    let path = temp_path("concurrent");
    let pool = pool(&path);
    base_db::sqlx::query("CREATE TABLE values_table(v INTEGER NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();
    let workers = (0..4)
        .map(|value| {
            let pool = pool.clone();
            tokio::spawn(async move {
                let mut transaction = pool.begin().await.unwrap();
                base_db::sqlx::query("INSERT INTO values_table(v) VALUES (?)")
                    .bind(value)
                    .execute(&mut *transaction)
                    .await
                    .unwrap();
                transaction.commit().await.unwrap();
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.await.unwrap();
    }
    let count: i64 = base_db::sqlx::query_scalar("SELECT COUNT(*) FROM values_table")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 4);
    cleanup(pool, &path).await;
}

#[tokio::test]
async fn restore_rejects_missing_source() {
    let target_path = temp_path("restore-target");
    let missing_path = temp_path("missing-backup");
    assert!(restore_sqlite(&missing_path, &target_path).await.is_err());
    assert!(!missing_path.exists());
    assert!(!target_path.exists());
}
