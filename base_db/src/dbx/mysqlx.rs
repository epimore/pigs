use std::sync::LazyLock;
use std::time::Duration;

use crate::sqlx::mysql::{MySqlConnectOptions, MySqlSslMode};
use crate::sqlx::pool::PoolOptions;
use crate::sqlx::{ConnectOptions, Connection, MySql, Pool};
use base::log::LevelFilter;
use base::serde::Deserialize;

use base::cfg_lib::conf;

use base::utils::crypto::default_decrypt;
use base::{logger, serde_default};

use crate::dbx::DatabasePoolConfig;
use crate::DatabaseError;
static MYSQL_POOL: LazyLock<Pool<MySql>> = LazyLock::new(|| DbModel::build_pool_conn());

pub fn get_conn_by_pool() -> &'static Pool<MySql> {
    &*MYSQL_POOL
}

#[derive(Debug, Deserialize)]
#[conf(prefix = "db.mysql")]
#[serde(crate = "base::serde")]
struct DbModel {
    host_or_ip: String,
    port: u16,
    db_name: String,
    user: String,
    pass: String,
    pass_crypto_enable: Option<bool>,
    attrs: Option<AttrsModel>,
    #[serde(default = "default_pool_model")]
    pool: PoolModel,
}
serde_default!(default_pool_model, PoolModel, PoolModel::default());
impl DbModel {
    fn build_pool_conn() -> Pool<MySql> {
        let model: DbModel = DbModel::conf();
        let password = if model.pass_crypto_enable.unwrap_or(false) {
            default_decrypt(&model.pass).expect("mysql pass invalid")
        } else {
            model.pass.clone()
        };
        let mut connection =
            <<MySql as crate::sqlx::Database>::Connection as Connection>::Options::new()
                .host(&model.host_or_ip)
                .port(model.port)
                .database(&model.db_name)
                .pipes_as_concat(false)
                .username(&model.user)
                .password(&password);
        if let Some(attrs) = model.attrs {
            connection = apply_attributes(connection, attrs);
        }
        build_mysql_pool(connection, model.pool.into()).expect("invalid mysql pool configuration")
    }
}

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

fn apply_attributes(mut connection: MySqlConnectOptions, attrs: AttrsModel) -> MySqlConnectOptions {
    if let Some(log) = attrs.log_global_sql_level {
        connection = connection.log_statements(logger::level_filter(&log));
    }
    if let Some(timeout) = attrs.log_slow_sql_timeout {
        connection = connection
            .log_slow_statements(LevelFilter::Warn, Duration::from_secs(u64::from(timeout)));
    }
    if let Some(timezone) = attrs.timezone {
        connection = connection.timezone(Some(timezone));
    }
    if let Some(charset) = attrs.charset {
        connection = connection.charset(&charset);
    }
    connection = match attrs.ssl_level {
        None | Some(1) => connection,
        Some(0) => connection.ssl_mode(MySqlSslMode::Disabled),
        Some(2) => connection.ssl_mode(MySqlSslMode::Required),
        Some(3) => connection.ssl_mode(MySqlSslMode::VerifyIdentity),
        Some(4) => connection.ssl_mode(MySqlSslMode::VerifyCa),
        Some(other) => panic!("连接无效加密等级:{other}"),
    };
    if let Some(ca) = attrs.ssl_ca_crt_file {
        connection = connection.ssl_ca(ca);
    }
    if let Some(cert) = attrs.ssl_ca_client_cert_file {
        connection = connection.ssl_client_cert(cert);
    }
    if let Some(key) = attrs.ssl_ca_client_key_file {
        connection = connection.ssl_client_key(key);
    }
    connection
}

#[derive(Debug, Deserialize)]
#[serde(crate = "base::serde")]
struct AttrsModel {
    log_global_sql_level: Option<String>,
    log_slow_sql_timeout: Option<u16>,
    timezone: Option<String>,
    charset: Option<String>,
    ssl_level: Option<u8>,
    ssl_ca_crt_file: Option<String>,
    ssl_ca_client_cert_file: Option<String>,
    ssl_ca_client_key_file: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(crate = "base::serde")]
struct PoolModel {
    #[serde(default = "default_max_connections")]
    max_connections: u32,
    #[serde(default = "default_min_connections")]
    min_connections: u32,
    #[serde(default = "default_connection_timeout")]
    connection_timeout: u8,
    #[serde(default = "default_max_lifetime")]
    max_lifetime: u32,
    #[serde(default = "default_idle_timeout")]
    idle_timeout: u32,
    #[serde(default = "default_check_health")]
    check_health: bool,
}

impl From<PoolModel> for DatabasePoolConfig {
    fn from(value: PoolModel) -> Self {
        Self {
            max_size: value.max_connections,
            min_idle: Some(value.min_connections),
            connection_timeout: Duration::from_secs(u64::from(value.connection_timeout)),
            max_lifetime: Some(Duration::from_secs(u64::from(value.max_lifetime))),
            idle_timeout: Some(Duration::from_secs(u64::from(value.idle_timeout))),
            test_on_check_out: value.check_health,
        }
    }
}

serde_default!(default_max_connections, u32, DEFAULT_MAX_CONNECTIONS);
serde_default!(default_min_connections, u32, DEFAULT_MIN_CONNECTIONS);
serde_default!(default_connection_timeout, u8, DEFAULT_CONNECTION_TIMEOUT);
serde_default!(default_max_lifetime, u32, DEFAULT_MAX_LIFETIME);
serde_default!(default_idle_timeout, u32, DEFAULT_IDLE_TIMEOUT);
serde_default!(default_check_health, bool, DEFAULT_CHECK_HEALTH);
const DEFAULT_MAX_CONNECTIONS: u32 = 151;
const DEFAULT_MIN_CONNECTIONS: u32 = 10;
const DEFAULT_CONNECTION_TIMEOUT: u8 = 8;
const DEFAULT_MAX_LIFETIME: u32 = 1800;
const DEFAULT_IDLE_TIMEOUT: u32 = 60;
const DEFAULT_CHECK_HEALTH: bool = false;

impl Default for PoolModel {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_MAX_CONNECTIONS,
            min_connections: DEFAULT_MIN_CONNECTIONS,
            connection_timeout: DEFAULT_CONNECTION_TIMEOUT,
            max_lifetime: DEFAULT_MAX_LIFETIME,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            check_health: DEFAULT_CHECK_HEALTH,
        }
    }
}
