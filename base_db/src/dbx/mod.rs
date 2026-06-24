use std::time::Duration;

use crate::DatabaseError;

#[cfg(feature = "mysql")]
pub mod mysqlx;
#[cfg(feature = "sqlite")]
pub mod sqlitex;

#[derive(Debug, Clone)]
pub struct DatabasePoolConfig {
    pub max_size: u32,
    pub min_idle: Option<u32>,
    pub connection_timeout: Duration,
    pub max_lifetime: Option<Duration>,
    pub idle_timeout: Option<Duration>,
    pub test_on_check_out: bool,
}

impl DatabasePoolConfig {
    pub fn validate(&self) -> Result<(), DatabaseError> {
        if self.max_size == 0 {
            return Err(DatabaseError::InvalidPoolConfig(
                "max_size must be greater than zero".to_string(),
            ));
        }
        if self.min_idle.is_some_and(|min| min > self.max_size) {
            return Err(DatabaseError::InvalidPoolConfig(
                "min_idle must not exceed max_size".to_string(),
            ));
        }
        if self.connection_timeout.is_zero() {
            return Err(DatabaseError::InvalidPoolConfig(
                "connection_timeout must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }
}

impl Default for DatabasePoolConfig {
    fn default() -> Self {
        Self {
            max_size: 16,
            min_idle: Some(1),
            connection_timeout: Duration::from_secs(8),
            max_lifetime: Some(Duration::from_secs(1800)),
            idle_timeout: Some(Duration::from_secs(60)),
            test_on_check_out: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_pool_bounds() {
        let config = DatabasePoolConfig {
            max_size: 1,
            min_idle: Some(2),
            ..DatabasePoolConfig::default()
        };
        assert!(matches!(
            config.validate(),
            Err(DatabaseError::InvalidPoolConfig(_))
        ));
    }
}
