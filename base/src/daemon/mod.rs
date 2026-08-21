pub mod signal;
#[cfg(unix)]
mod unix;

use cfg_lib::CliBasic;
use exception::GlobalResult;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process;
use std::sync::Once;
use std::{env, fs};

//todo 优化，运行前检查是否已有进程运行：当前即使未再次运行成功也会重写meta数据
pub trait Daemon<T> {
    fn cli_basic() -> CliBasic;
    fn init_privilege() -> GlobalResult<(Self, T)>
    where
        Self: Sized;
    fn run_app(self, t: T) -> GlobalResult<()>;
}

pub fn install_sanitized_panic_hook() {
    static INSTALL: Once = Once::new();

    INSTALL.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let current_thread = std::thread::current();
            let thread_name = current_thread.name().unwrap_or("unnamed");
            let message = if let Some(message) = info.payload().downcast_ref::<&str>() {
                *message
            } else if let Some(message) = info.payload().downcast_ref::<String>() {
                message.as_str()
            } else {
                "Box<dyn Any>"
            };

            if let Some(location) = info.location() {
                eprintln!(
                    "thread '{thread_name}' panicked at {}:{}:{}:\n{message}",
                    crate::logger::display_source_file(location.file()),
                    location.line(),
                    location.column(),
                );
            } else {
                eprintln!("thread '{thread_name}' panicked:\n{message}");
            }
        }));
    });
}

fn run_foreground<D, T>() -> Result<(), String>
where
    D: Daemon<T>,
{
    let (daemon, bootstrap) =
        D::init_privilege().map_err(|error| format!("App init error: {error}"))?;
    daemon
        .run_app(bootstrap)
        .map_err(|error| format!("App runtime error: {error}"))
}

#[derive(Serialize, Deserialize)]
struct DaemonMeta {
    config_path: String,
    daemon: bool,
}
impl DaemonMeta {
    fn get_meta_file_path() -> PathBuf {
        let exe_path = env::current_exe().expect("Failed to get current exe path");
        exe_path.with_extension("meta")
    }

    fn save_meta(&self) {
        let meta_path = Self::get_meta_file_path();
        let content = serde_json::to_string(self).expect("Failed to serialize meta");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(meta_path)
            .expect("Failed to open meta file");
        file.write_all(content.as_bytes())
            .expect("Failed to write meta");
    }

    fn load_meta() -> Self {
        let meta_path = Self::get_meta_file_path();
        let content = fs::read_to_string(meta_path).expect("Failed to read meta file");
        serde_json::from_str(&content).expect("Failed to deserialize meta")
    }
}

pub fn run<D, T>()
where
    D: Daemon<T>,
{
    install_sanitized_panic_hook();
    let app_info = D::cli_basic();
    let arg_matches = cfg_lib::conf::get_arg_cmd(app_info);
    match arg_matches.subcommand() {
        Some(("start", args)) => {
            let config_path = args
                .try_get_one::<String>("config")
                .expect("get config failed")
                .expect("not found config")
                .to_string();
            cfg_lib::conf::init_cfg(config_path.clone());
            let daemon = args.get_flag("daemon");
            let meta = DaemonMeta {
                config_path,
                daemon,
            };
            meta.save_meta();
            if daemon && (cfg!(target_os = "linux") || cfg!(target_os = "macos")) {
                #[cfg(unix)]
                {
                    unix::start_service::<D, T>();
                }
                return;
            }
            if daemon {
                eprintln!("The daemon only supports macOS, and Linux");
            }
            if let Err(error) = run_foreground::<D, T>() {
                eprintln!("{error}");
                process::exit(1);
            }
        }
        Some(("stop", _)) => {
            let daemon_meta = DaemonMeta::load_meta();
            if daemon_meta.daemon {
                if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
                    #[cfg(unix)]
                    {
                        unix::stop_service();
                    }
                } else {
                    eprintln!("The daemon only supports macOS, and Linux");
                }
            } else {
                eprintln!("Not running daemon mode");
            }
        }
        Some(("restart", _)) => {
            let daemon_meta = DaemonMeta::load_meta();
            if daemon_meta.daemon {
                let config_path = daemon_meta.config_path;
                cfg_lib::conf::init_cfg(config_path);
                if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
                    #[cfg(unix)]
                    {
                        unix::restart_service::<D, T>();
                    }
                } else {
                    eprintln!("The daemon only supports macOS, and Linux");
                }
            } else {
                eprintln!("Not running daemon mode");
            }
        }
        Some(("status", _)) => {
            if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
                #[cfg(unix)]
                {
                    unix::status_service();
                }
            } else {
                eprintln!("The status only supports macOS, and Linux");
            }
        }
        _other => {
            eprintln!("Please add subcommands to operate: [start|stop|restart]")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exception::GlobalError;
    use std::process::Command;

    const PANIC_HELPER_ENV: &str = "BASE_SANITIZED_PANIC_HELPER";

    struct InitFailure;

    impl Daemon<()> for InitFailure {
        fn cli_basic() -> CliBasic {
            unreachable!()
        }

        fn init_privilege() -> GlobalResult<(Self, ())> {
            Err(GlobalError::new_sys_error(
                "bind session grpc 127.0.0.1:19081 failed: Address already in use (os error 98)",
                |_| {},
            ))
        }

        fn run_app(self, _bootstrap: ()) -> GlobalResult<()> {
            unreachable!()
        }
    }

    struct RuntimeFailure;

    impl Daemon<()> for RuntimeFailure {
        fn cli_basic() -> CliBasic {
            unreachable!()
        }

        fn init_privilege() -> GlobalResult<(Self, ())> {
            Ok((Self, ()))
        }

        fn run_app(self, _bootstrap: ()) -> GlobalResult<()> {
            Err(GlobalError::new_sys_error("runtime stopped", |_| {}))
        }
    }

    #[test]
    fn foreground_init_error_preserves_diagnostics_without_source_path() {
        let error = run_foreground::<InitFailure, ()>().unwrap_err();

        assert_eq!(
            error,
            "App init error: bind session grpc 127.0.0.1:19081 failed: Address already in use (os error 98)"
        );
        assert!(!error.contains(env!("CARGO_MANIFEST_DIR")));
    }

    #[test]
    fn foreground_runtime_error_is_returned_instead_of_panicking() {
        assert_eq!(
            run_foreground::<RuntimeFailure, ()>().unwrap_err(),
            "App runtime error: runtime stopped"
        );
    }

    #[test]
    fn sanitized_panic_process_helper() {
        if env::var_os(PANIC_HELPER_ENV).is_some() {
            install_sanitized_panic_hook();
            panic!("panic hook diagnostic");
        }
    }

    #[test]
    fn panic_output_hides_build_source_root() {
        let output = Command::new(env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "daemon::tests::sanitized_panic_process_helper",
                "--nocapture",
            ])
            .env(PANIC_HELPER_ENV, "1")
            .output()
            .expect("run panic hook helper");

        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).expect("panic output is UTF-8");
        assert!(stderr.contains("panic hook diagnostic"));
        assert!(stderr.contains("base/src/daemon/mod.rs:"));
        assert!(!stderr.contains(env!("CARGO_MANIFEST_DIR")));
    }
}
