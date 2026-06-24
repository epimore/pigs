use crate::DatabaseError;

#[cfg(feature = "mysql")]
pub async fn check_mysql(pool: &crate::sqlx::MySqlPool) -> Result<(), DatabaseError> {
    crate::sqlx::query("SELECT 1").execute(pool).await?;
    Ok(())
}

#[cfg(feature = "sqlite")]
pub async fn check_sqlite(pool: &crate::sqlx::SqlitePool) -> Result<(), DatabaseError> {
    crate::sqlx::query("SELECT 1").execute(pool).await?;
    Ok(())
}
