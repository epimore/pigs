use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::sync::{Arc, Mutex};

use clap::{Arg, ArgMatches, Command};
use once_cell::sync::{Lazy, OnceCell};

static CONF: OnceCell<Arc<String>> = OnceCell::new();
static INSTANCES: Lazy<Mutex<HashMap<String, fn()>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// 通过配置文件初始化时，
/// 校验struct字段
pub trait CheckFromConf{
    fn _field_check(&self);
}
pub fn register_function(name: &str, func: fn()) {
    INSTANCES.lock().unwrap().insert(name.to_string(), func);
}

pub fn get_config() -> Arc<String> {
    CONF.get().expect("service configuration has not yet been initialized").clone()
}

pub fn init_cfg(path: String) {
    let mut file = File::open(path).expect("not found config file to open");
    let mut conf = String::new();
    file.read_to_string(&mut conf).expect("read file content to string failed");
    CONF.set(Arc::new(conf)).expect("form config of service has been initialized");
    //校验配置文件conf初始化类型是否正确
    for (_name, func) in INSTANCES.lock().unwrap().iter() {
        func();
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
