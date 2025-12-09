use std::path::PathBuf;

use chrono::Local;
use fern::colors::{Color, ColoredLevelConfig};
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
///   specify: #指定日志输出 可选，不指定则默认输出到全局日志里
///     - crate_name: test_log::a,test_log::d$  #或者test_log用指全部  必选 以$结束为全路径匹配，精准记录指定日志
///       level: debug #日志等级 必选
///       file_name_prefix: a #日志文件前缀 可选 当未指定时，记录到全局日志文件中，等级由指定日志等级控制
///     - crate_name: test_log::b  #或者test_log用指全部
///       level: debug #日志等级
///     - crate_name: test_log::c  #或者test_log用指全部
///       level: debug #日志等级
///       file_name_prefix: c #日志文件前缀
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
}


impl Logger {
    pub fn init() -> GlobalResult<()> {
        let mut log: Logger = Logger::conf();

        // store_path 逻辑
        let store_path = if log.store_path.as_os_str().is_empty() {
            PathBuf::from("./")
        } else {
            (log.store_path).push("");
            log.store_path
        };

        std::fs::create_dir_all(&store_path)
            .hand_log(|msg| error!("create log dir failed: {msg}"))?;

        // 基础 formatter
        let colors = ColoredLevelConfig::new()
            .trace(Color::White)
            .info(Color::Green)
            .debug(Color::Blue)
            .warn(Color::Yellow)
            .error(Color::Red);

        let formatter = move |out: fern::FormatCallback,
                              msg: &std::fmt::Arguments,
                              record: &log::Record| {
            out.finish(format_args!(
                "[{}] [{}] [{}] {}:{} >> {}",
                Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                colors.color(record.level()),
                record.target(),
                record.file().unwrap_or("unknown"),
                record.line().unwrap_or(0),
                msg,
            ))
        };

        // 记录所有独立输出的 target，用于主日志排除
        let mut exclude_targets: Vec<String> = Vec::new();

        // 主日志（最后再 chain）
        let main_logger = fern::Dispatch::new()
            .format(formatter.clone())
            .level(level_filter(&log.level))   // 默认等级
            .chain(std::io::stdout())
            .chain(fern::DateBased::new(
                &store_path,
                format!("{}_{}.log", log.prefix, "%Y-%m-%d"),
            ));

        // 最终总 dispatch
        let mut dispatch = fern::Dispatch::new();

        // 处理 specify
        if let Some(specify) = &log.specify {
            for s in specify {
                let targets: Vec<String> = s
                    .crate_name
                    .split(',')
                    .map(|t| t.trim().to_string())
                    .collect();

                let level = level_filter(&s.level);

                // case ①：独立文件输出
                if let Some(file_prefix) = &s.file_name_prefix {
                    exclude_targets.extend(targets.clone());

                    let file_logger = fern::Dispatch::new()
                        .format(formatter.clone())
                        .level(level)
                        .filter(move |meta| match_target(meta.target(), &targets))
                        .chain(fern::DateBased::new(
                            &store_path,
                            format!("{}_{}.log", file_prefix, "%Y-%m-%d"),
                        ));

                    dispatch = dispatch.chain(file_logger);

                } else {
                    // case ②：使用主日志，提高等级
                    for t in targets {
                        dispatch = dispatch.level_for(t, level);
                    }
                }
            }
        }

        // 主日志排除“独立文件输出”的 targets
        let main_logger = main_logger.filter(move |meta| {
            !match_target(meta.target(), &exclude_targets)
        });

        dispatch = dispatch.chain(main_logger);

        // ⑧ 正式生效
        dispatch
            .apply()
            .hand_log(|msg| error!("Logger init failed: {msg}"))?;

        Ok(())
    }
}

// target 匹配逻辑
fn match_target(target: &str, rules: &[String]) -> bool {
    rules.iter().any(|r| {
        if r.ends_with('$') {
            target == &r[..r.len() - 1]
        } else {
            target.starts_with(r)
        }
    })
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