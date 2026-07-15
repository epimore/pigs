use std::borrow::Cow;
use std::path::PathBuf;

use chrono::Local;
use fern::colors::{Color, ColoredLevelConfig};
use log::{error, LevelFilter};
use serde::{Deserialize, Deserializer};

use crate::serde_default;
use cfg_lib::conf;
use exception::{GlobalResult, GlobalResultExt};

pub mod episode;

/// 通过配置文件控制日志格式化输出
/// # Examples
///
///  ```yaml
/// log:
///   level: warn #全局日志等级 可选：默认 info
///   stdout: true #是否输出到控制台；可选：默认 true
///   prefix: server #全局日志文件前缀; 可选：默认 app 指定生成日志文件添加日期后缀，如 server_2024-10-26.log
///   store_path: ./logs #日志文件根目录；可选 默认 当前目录
///   specify: #指定日志输出 可选，不指定则默认输出到全局日志里
///     - crate_name: test_log::a,test_log::d$  #或者test_log用指全部  必选 以$结束为全路径匹配，精准记录指定日志
///       level: debug #日志等级 可选，不指定则使用全局日志等级
///       file_name_prefix: a #日志文件前缀 可选 当未指定时，记录到全局日志文件中，等级由指定日志等级控制
///       stdout: false #独立文件日志是否输出到控制台；可选：默认 true
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
    #[serde(default = "default_stdout")]
    stdout: bool,
    specify: Option<Vec<Specify>>,
}
serde_default!(default_prefix, String, "app".to_string());
serde_default!(default_level, String, "info".to_string());
serde_default!(default_stdout, bool, true);

#[derive(Debug, Deserialize)]
pub struct Specify {
    crate_name: String,
    #[serde(default, deserialize_with = "validate_optional_level")]
    level: Option<String>,
    file_name_prefix: Option<String>,
    stdout: Option<bool>,
}

#[derive(Clone, Debug)]
struct LogRule {
    targets: Vec<String>,
    level: Option<LevelFilter>,
}

impl Logger {
    pub fn init() -> GlobalResult<()> {
        let mut log: Logger = Logger::conf();
        let default_level = level_filter(&log.level);

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

        let formatter =
            move |out: fern::FormatCallback, msg: &std::fmt::Arguments, record: &log::Record| {
                let source_file = display_source_file(record.file().unwrap_or("unknown"));
                out.finish(format_args!(
                    "[{}] [{}] [{}] {}:{} >> {}",
                    Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                    colors.color(record.level()),
                    record.target(),
                    source_file,
                    record.line().unwrap_or(0),
                    msg,
                ))
            };

        // 记录所有独立输出的 target，用于主日志排除
        let mut exclude_targets: Vec<String> = Vec::new();
        let mut main_rules: Vec<LogRule> = Vec::new();
        let mut main_max_level = default_level;

        // 最终总 dispatch
        let mut dispatch = fern::Dispatch::new();

        // 处理 specify
        if let Some(specify) = &log.specify {
            for s in specify {
                let targets = parse_targets(&s.crate_name);
                let level = s.level.as_deref().map(level_filter);
                let effective_level = level.unwrap_or(default_level);

                // case ①：独立文件输出
                if let Some(file_prefix) = &s.file_name_prefix {
                    exclude_targets.extend(targets.clone());

                    let mut file_logger = fern::Dispatch::new()
                        .format(formatter)
                        .level(effective_level)
                        .filter(move |meta| match_target(meta.target(), &targets));

                    if s.stdout.unwrap_or(true) {
                        file_logger = file_logger.chain(std::io::stdout());
                    }

                    file_logger = file_logger.chain(fern::DateBased::new(
                        &store_path,
                        format!("{}_{}.log", file_prefix, "%Y-%m-%d"),
                    ));

                    dispatch = dispatch.chain(file_logger);
                } else {
                    main_max_level = main_max_level.max(effective_level);
                    main_rules.push(LogRule { targets, level });
                }
            }
        }

        // 主日志排除“独立文件输出”的 targets
        let mut main_logger = fern::Dispatch::new()
            .format(formatter)
            .level(main_max_level)
            .filter(move |meta| {
                !match_target(meta.target(), &exclude_targets)
                    && meta.level() <= effective_level(meta.target(), default_level, &main_rules)
            });

        if log.stdout {
            main_logger = main_logger.chain(std::io::stdout());
        }

        main_logger = main_logger.chain(fern::DateBased::new(
            &store_path,
            format!("{}_{}.log", log.prefix, "%Y-%m-%d"),
        ));

        dispatch = dispatch.chain(main_logger);

        // ⑧ 正式生效
        dispatch
            .apply()
            .hand_log(|msg| error!("Logger init failed: {msg}"))?;

        Ok(())
    }
}

fn parse_targets(crate_name: &str) -> Vec<String> {
    crate_name
        .split(',')
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn display_source_file(file: &str) -> Cow<'_, str> {
    let windows_absolute =
        file.starts_with("\\\\") || matches!(file.as_bytes(), [_, b':', b'\\' | b'/', ..]);
    if !file.starts_with('/') && !windows_absolute {
        return Cow::Borrowed(file);
    }

    if file.contains('\\') {
        let normalized = file.replace('\\', "/");
        return Cow::Owned(
            trim_source_root(&normalized)
                .unwrap_or(&normalized)
                .to_string(),
        );
    }

    Cow::Borrowed(trim_source_root(file).unwrap_or(file))
}

fn trim_source_root(file: &str) -> Option<&str> {
    let src = file.rfind("/src/")?;
    let crate_start = file[..src].rfind('/').map_or(0, |index| index + 1);
    Some(&file[crate_start..])
}

fn effective_level(target: &str, default_level: LevelFilter, rules: &[LogRule]) -> LevelFilter {
    rules
        .iter()
        .find(|rule| match_target(target, &rule.targets))
        .and_then(|rule| rule.level)
        .unwrap_or(default_level)
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
    level_filter(&level);
    Ok(level)
}

pub fn validate_optional_level<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let level = Option::<String>::deserialize(deserializer)?;
    if let Some(level) = &level {
        level_filter(level);
    }
    Ok(level)
}

#[cfg(test)]
mod tests {
    use log::LevelFilter;

    use super::{display_source_file, effective_level, parse_targets, LogRule};

    #[test]
    fn shortens_absolute_source_paths() {
        assert_eq!(
            display_source_file("/home/ubuntu20/code/rs/mv/github/epimore/gmv_pjsip/src/io.rs"),
            "gmv_pjsip/src/io.rs"
        );
        assert_eq!(
            display_source_file(r"C:\code\epimore\gmv_pjsip\src\io.rs"),
            "gmv_pjsip/src/io.rs"
        );
    }

    #[test]
    fn keeps_relative_source_paths() {
        assert_eq!(
            display_source_file("session/src/http/hook.rs"),
            "session/src/http/hook.rs"
        );
    }

    #[test]
    fn effective_level_uses_global_level_when_no_rule_matches() {
        let rules = vec![LogRule {
            targets: parse_targets("app::debug"),
            level: Some(LevelFilter::Debug),
        }];

        assert_eq!(
            effective_level("third_party::noise", LevelFilter::Info, &rules),
            LevelFilter::Info
        );
    }

    #[test]
    fn effective_level_inherits_global_level_when_rule_has_no_level() {
        let rules = vec![LogRule {
            targets: parse_targets("app::default"),
            level: None,
        }];

        assert_eq!(
            effective_level("app::default::module", LevelFilter::Warn, &rules),
            LevelFilter::Warn
        );
    }

    #[test]
    fn effective_level_uses_specified_level_for_matching_target() {
        let rules = vec![LogRule {
            targets: parse_targets("app::verbose"),
            level: Some(LevelFilter::Debug),
        }];

        assert_eq!(
            effective_level("app::verbose::module", LevelFilter::Info, &rules),
            LevelFilter::Debug
        );
    }
}
