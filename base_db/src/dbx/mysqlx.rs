use crate::sqlx::mysql::MySqlConnectOptions;
use crate::sqlx::pool::PoolOptions;
use crate::sqlx::{MySql, Pool};

use crate::dbx::DatabasePoolConfig;
use crate::DatabaseError;

pub fn build_mysql_pool(
    connection: MySqlConnectOptions,
    pool: DatabasePoolConfig,
) -> Result<Pool<MySql>, DatabaseError> {
    pool.validate()?;
    Ok(PoolOptions::<MySql>::new()
        .max_connections(pool.max_size)
        .min_connections(pool.min_idle.unwrap_or(0))
        .acquire_timeout(pool.connection_timeout)
        .max_lifetime(pool.max_lifetime)
        .idle_timeout(pool.idle_timeout)
        .test_before_acquire(pool.test_on_check_out)
        .connect_lazy_with(connection))
}
