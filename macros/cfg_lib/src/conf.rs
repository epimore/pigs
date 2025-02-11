use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::sync::{Arc, Mutex};

use clap::{Arg, ArgMatches, Command};
use once_cell::sync::{Lazy, OnceCell};

static CONF: OnceCell<Arc<String>> = OnceCell::new();
static INSTANCES: Lazy<Mutex<HashMap<String, Box<dyn Fn() -> Result<(), FieldCheckError> +Send>>>> = Lazy::new(|| Mutex::new(HashMap::new()));


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

/// 通过配置文件初始化时，
/// 校验struct字段
pub trait CheckFromConf {
    fn _field_check(&self) -> Result<(), FieldCheckError>;
}

pub fn register_function<F>(name: &str, func: F)
where
    F: Fn() -> Result<(), FieldCheckError> + 'static + Send,
{
    INSTANCES.lock().unwrap().insert(name.to_string(), Box::new(func));
}

pub fn get_config() -> Arc<String> {
    CONF.get().expect("service configuration has not yet been initialized").clone()
}

pub fn init_cfg(path: String) {
    let mut file = File::open(path).expect("not found config file to open");
    let mut conf = String::new();
    file.read_to_string(&mut conf).expect("read file content to string failed");
    CONF.set(Arc::new(conf)).expect("form config of service has been initialized");
    let mut err_msg = String::new();
    //校验配置文件conf初始化类型是否正确
    for (name, func) in INSTANCES.lock().unwrap().iter() {
        if let Err(err) = func() {
            err_msg.push_str(&format!("{}: {}\n", name, err).to_string());
        }
    }
    if err_msg.len() > 0 {
        panic!("{}", err_msg);
    }
}

pub fn get_arg_match() -> ArgMatches {
    Command::new("MyApp")
        .version("1.0")
        .author("Kz. <kz986542@gmail.com>")
        .about("get the path about config file")
        .subcommand(
            Command::new("start")
                .about("Start the service")
                .arg(Arg::new("config")
                    .short('c')
                    .long("config")
                    .help("Path to configuration file")
                    .default_value("./config.yml")
                )
        )
        .subcommand(
            Command::new("stop")
                .about("Stop the service")
        )
        .subcommand(
            Command::new("restart")
                .about("Restart the service")
                .arg(Arg::new("config")
                    .short('c')
                    .long("config")
                    .help("Path to configuration file")
                    .default_value("./config.yml")
                )
        )
        .get_matches()
}
