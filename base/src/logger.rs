use std::path::PathBuf;

use chrono::Local;
use fern::colors::{Color, ColoredLevelConfig};
use fern::Dispatch;
use log::{error, LevelFilter};
use serde::{Deserialize, Deserializer};

use cfg_lib::{conf};
use exception::{GlobalResult, GlobalResultExt};
use crate::serde_default;

/// 通过配置文件控制日志格式化输出
/// # Examples
///
///  ```yaml
/// log:
///   level: warn #全局日志等级 可选：默认 info
///   prefix: server #全局日志文件前缀; 可选：默认 app 指定生成日志文件添加日期后缀，如 server_2024-10-26.log
///   store_path: ./logs #日志文件根目录；可选 默认 当前目录
///   specify: #指定日志输出 可选，不指定日志
///     - crate_name: test_log::a,test_log::d$  #或者test_log用指全部  必选 以$结束为全路径匹配，精准记录指定日志
///       level: debug #日志等级 必选
///       file_name_prefix: a #日志文件前缀 可选 当未指定时，记录到全局日志文件中，等级由指定日志等级控制
///       additivity: false #是否记录到全局日志文件中 可选 默认false,全局日志文件会再次根据全局日志等级过滤记录
///     - crate_name: test_log::b  #或者test_log用指全部
///       level: debug #日志等级
///     - crate_name: test_log::c  #或者test_log用指全部
///       level: debug #日志等级
///       file_name_prefix: c #日志文件前缀
///       additivity: true #是否记录到全局日志文件中
///  ```
#[derive(Debug, Deserialize)]
#[conf(prefix = "log", lib)]
pub struct Logger {
    #[serde(default)]
    store_path: PathBuf,
    #[serde(default = "default_prefix")]
    prefix: String,
    #[serde(deserialize_with = "validate_level")]
    #[serde(default = "default_level")]
    level: String,
    specify: Option<Vec<Specify>>,
}
serde_default!(default_prefix, String, "app".to_string());
serde_default!(default_level, String, "info".to_string());

#[derive(Debug, Deserialize)]
pub struct Specify {
    crate_name: String,
    #[serde(deserialize_with = "validate_level")]
    level: String,
    file_name_prefix: Option<String>,
    additivity: Option<bool>,
}


impl Logger {
    pub fn init() -> GlobalResult<()> {
        let mut log: Logger = Logger::conf();
        if !log.store_path.ends_with("/") { log.store_path.push("") };
        let default_level: LevelFilter = level_filter(&log.level);
        let path = std::path::Path::new(&log.store_path);
        std::fs::create_dir_all(path).hand_log(|msg| error!("create log dir failed: {msg}"))?;
        let colors = ColoredLevelConfig::new()
            // .trace(Color::White)
            // .info(Color::Green)
            .debug(Color::Blue)
            .error(Color::Red)
            .warn(Color::Yellow);
        let mut add_crate = Vec::new();
        let mut dispatch = Dispatch::new()
            .format(move |out, message, record| {
                out.finish(format_args!(
                    "[{}] [{}] [{}] {} {}\n{}",
                    Local::now().format("%Y-%m-%d %H:%M:%S"),
                    colors.color(record.level()),
                    record.target(),
                    record.file().unwrap_or("unknown"),
                    record.line().unwrap_or(0),
                    message,
                ))
            });
        if let Some(specify) = &log.specify {
            for s in specify {
                let module_level = level_filter(&s.level);
                let targets: Vec<String> = s.crate_name.split(",").map(|str| str.trim().to_string()).collect();
                // 根据 `additivity` 决定是否记录到默认日志
                if !s.additivity.unwrap_or(false) {
                    add_crate.extend(targets.clone());
                }

                // 为特定模块创建日志输出
                let mut module_dispatch = Dispatch::new()
                    .level(module_level)
                    .filter(move |metadata| targets.iter().any(|t| {
                        if t.ends_with("$") {
                            metadata.target() == &t[..t.len() - 1]
                        } else {
                            metadata.target().starts_with(t)
                        }
                    }));

                // 如果指定了文件名前缀，则将日志输出到指定的文件
                let prefix = s.file_name_prefix.as_deref().unwrap_or(&log.prefix);
                module_dispatch = module_dispatch
                    .chain(std::io::stdout())
                    .chain(fern::DateBased::new(&log.store_path, format!("{}_{}.log", prefix, "%Y-%m-%d".to_string())));

                dispatch = dispatch.chain(module_dispatch);
            }
        }

        // 配置默认日志输出
        let default_dispatch = Dispatch::new()
            .level(default_level)
            .filter(move |metadata| !add_crate.iter().any(|t| {
                if t.ends_with("$") {
                    metadata.target() == &t[..t.len() - 1]
                } else {
                    metadata.target().starts_with(t)
                }
            }))
            .chain(std::io::stdout())
            .chain(fern::DateBased::new(&log.store_path, format!("{}_{}.log", log.prefix, "%Y-%m-%d".to_string())));

        dispatch
            .chain(default_dispatch)
            .apply()
            .hand_log(|msg| error!("Logger initialization failed: {msg}"))?;
        Ok(())
    }
}

pub fn level_filter(level: &str) -> LevelFilter {
    match level.trim().to_uppercase().as_str() {
        "OFF" => LevelFilter::Off,
        "ERROR" => LevelFilter::Error,
        "WARN" => LevelFilter::Warn,
        "INFO" => LevelFilter::Info,
        "DEBUG" => LevelFilter::Debug,
        "TRACE" => LevelFilter::Trace,
        _ => panic!("The log level is invalid"),
    }
}

pub fn validate_level<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let level = String::deserialize(deserializer)?;
    level_filter(&*level);
    Ok(level)
}