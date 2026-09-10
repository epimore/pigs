use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::CliBasic;
use clap::{Arg, ArgAction, ArgMatches, Command};
use once_cell::sync::{Lazy, OnceCell};

static CONF: OnceCell<Arc<String>> = OnceCell::new();
type ConfigValidator = Box<dyn Fn() -> Result<(), ConfigError> + Send>;
static INSTANCES: Lazy<Mutex<HashMap<String, ConfigValidator>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static VALIDATOR_REGISTRY_FAILED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub enum FieldCheckError {
    BizError(String), //业务错误
}

impl std::fmt::Display for FieldCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldCheckError::BizError(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for FieldCheckError {}

#[derive(Debug)]
pub enum ConfigError {
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    AlreadyInitialized,
    NotInitialized,
    ValidatorRegistry,
    Parse {
        target: &'static str,
        source: Box<dyn Error + Send + Sync>,
    },
    Validation {
        target: String,
        message: String,
    },
}

impl ConfigError {
    pub fn parse<E>(target: &'static str, source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self::Parse {
            target,
            source: Box::new(source),
        }
    }

    pub fn validation(target: impl Into<String>, error: FieldCheckError) -> Self {
        Self::Validation {
            target: target.into(),
            message: error.to_string(),
        }
    }
}

impl Display for ConfigError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open { path, .. } => {
                write!(f, "open service configuration {} failed", path.display())
            }
            Self::Read { path, .. } => {
                write!(f, "read service configuration {} failed", path.display())
            }
            Self::AlreadyInitialized => write!(f, "service configuration is already initialized"),
            Self::NotInitialized => write!(f, "service configuration is not initialized"),
            Self::ValidatorRegistry => write!(f, "service configuration validator registry failed"),
            Self::Parse { target, .. } => {
                write!(f, "parse service configuration for {target} failed")
            }
            Self::Validation { target, message } => {
                write!(
                    f,
                    "service configuration validation failed for {target}: {message}"
                )
            }
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open { source, .. } | Self::Read { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// 通过配置文件初始化时，
/// 校验struct字段
pub trait CheckFromConf {
    fn _field_check(&self) -> Result<(), FieldCheckError>;
}

pub fn register_function<F>(name: &str, func: F)
where
    F: Fn() -> Result<(), ConfigError> + 'static + Send,
{
    if let Ok(mut instances) = INSTANCES.lock() {
        instances.insert(name.to_string(), Box::new(func));
    } else {
        VALIDATOR_REGISTRY_FAILED.store(true, Ordering::Release);
    }
}

#[deprecated(note = "use try_get_config")]
pub fn get_config() -> Arc<String> {
    try_get_config().expect("service configuration has not yet been initialized")
}

pub fn try_get_config() -> Result<Arc<String>, ConfigError> {
    CONF.get().cloned().ok_or(ConfigError::NotInitialized)
}

#[deprecated(note = "use try_init_cfg")]
pub fn init_cfg(path: String) {
    try_init_cfg(path).expect("initialize service configuration failed")
}

pub fn try_init_cfg(path: impl AsRef<Path>) -> Result<(), ConfigError> {
    let path = path.as_ref();
    let conf = fs::read_to_string(path).map_err(|source| {
        let path = path.to_path_buf();
        if source.kind() == std::io::ErrorKind::NotFound
            || source.kind() == std::io::ErrorKind::PermissionDenied
        {
            ConfigError::Open { path, source }
        } else {
            ConfigError::Read { path, source }
        }
    })?;
    CONF.set(Arc::new(conf))
        .map_err(|_| ConfigError::AlreadyInitialized)?;
    if VALIDATOR_REGISTRY_FAILED.load(Ordering::Acquire) {
        return Err(ConfigError::ValidatorRegistry);
    }
    let instances = INSTANCES
        .lock()
        .map_err(|_| ConfigError::ValidatorRegistry)?;
    for (name, func) in instances.iter() {
        func().map_err(|error| match error {
            ConfigError::Validation { message, .. } => ConfigError::Validation {
                target: name.clone(),
                message,
            },
            other => other,
        })?;
    }
    Ok(())
}

pub fn command(app_info: CliBasic) -> Command {
    Command::new(app_info.name)
        .version(app_info.version)
        .author(app_info.author)
        .about(app_info.about)
        .subcommand(
            Command::new("start")
                .about("Start the service")
                .arg(
                    Arg::new("config")
                        .short('c')
                        .long("config")
                        .help("Path to configuration file")
                        .default_value("./config.yml"),
                )
                .arg(
                    Arg::new("daemon")
                        .short('d')
                        .long("daemon")
                        .help("Run as a daemon")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(Command::new("stop").about("Stop the service"))
        .subcommand(
            Command::new("restart").about("Restart the service").arg(
                Arg::new("config")
                    .short('c')
                    .long("config")
                    .help("Path to configuration file")
                    .default_value("./config.yml"),
            ),
        )
        .subcommand(Command::new("status").about("status the service"))
}

pub fn get_arg_cmd(app_info: CliBasic) -> ArgMatches {
    command(app_info).get_matches()
}
