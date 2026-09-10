pub mod signal;
#[cfg(unix)]
mod unix;

use cfg_lib::CliBasic;
use exception::{GlobalError, GlobalResult};
use std::process;
use std::sync::Once;

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

fn run_foreground<D, T>() -> GlobalResult<()>
where
    D: Daemon<T>,
{
    let (daemon, bootstrap) = D::init_privilege()?;
    daemon.run_app(bootstrap)
}

#[derive(Debug, Default)]
pub(super) struct CommandReport {
    lines: Vec<String>,
}

impl CommandReport {
    pub(super) fn line(line: impl Into<String>) -> Self {
        Self {
            lines: vec![line.into()],
        }
    }

    pub(super) fn push(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }
}

fn config_path(args: &cfg_lib::ArgMatches) -> GlobalResult<String> {
    args.try_get_one::<String>("config")
        .map_err(|error| GlobalError::from_external_error(error, |_| {}))?
        .cloned()
        .ok_or_else(|| daemon_error("configuration path is missing"))
}

fn init_config(path: &str) -> GlobalResult<()> {
    cfg_lib::conf::try_init_cfg(path)
        .map_err(|error| GlobalError::from_external_error(error, |_| {}))
}

fn run_command<D, T>(arg_matches: &cfg_lib::ArgMatches) -> GlobalResult<CommandReport>
where
    D: Daemon<T>,
{
    match arg_matches.subcommand() {
        Some(("start", args)) => {
            init_config(&config_path(args)?)?;
            let daemon = args.get_flag("daemon");
            if daemon && (cfg!(target_os = "linux") || cfg!(target_os = "macos")) {
                #[cfg(unix)]
                {
                    return unix::start_service::<D, T>();
                }
            }
            if daemon {
                return Err(daemon_error("daemon mode only supports macOS and Linux"));
            }
            run_foreground::<D, T>()?;
            Ok(CommandReport::default())
        }
        Some(("stop", _)) => {
            #[cfg(unix)]
            {
                unix::stop_service()
            }
            #[cfg(not(unix))]
            {
                Err(daemon_error("daemon mode only supports macOS and Linux"))
            }
        }
        Some(("restart", args)) => {
            #[cfg(unix)]
            {
                let config_path = config_path(args)?;
                unix::stop_service()?;
                init_config(&config_path)?;
                unix::start_service::<D, T>()
            }
            #[cfg(not(unix))]
            {
                let _ = args;
                Err(daemon_error("daemon mode only supports macOS and Linux"))
            }
        }
        Some(("status", _)) => {
            #[cfg(unix)]
            {
                unix::status_service()
            }
            #[cfg(not(unix))]
            {
                Err(daemon_error("service status only supports macOS and Linux"))
            }
        }
        _ => Err(daemon_error(
            "a service command is required: start, stop, restart, or status",
        )),
    }
}

pub fn run<D, T>()
where
    D: Daemon<T>,
{
    install_sanitized_panic_hook();
    let command = cfg_lib::conf::command(D::cli_basic());
    let matches = match command.try_get_matches_from(std::env::args_os()) {
        Ok(matches) => matches,
        Err(error) => {
            let exit_code = error.exit_code();
            let _ = error.print();
            process::exit(exit_code);
        }
    };
    let result = run_command::<D, T>(&matches);
    match result {
        Ok(report) => {
            for line in report.lines {
                println!("{line}");
            }
        }
        Err(error) => {
            eprintln!("service command failed: {error}");
            process::exit(1);
        }
    }
}

fn daemon_error(message: &str) -> GlobalError {
    GlobalError::new_sys_error(message, |_| {})
}

#[cfg(test)]
mod tests {
    use super::*;
    use exception::GlobalError;
    use std::env;
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
        let error = run_foreground::<InitFailure, ()>().unwrap_err().to_string();

        assert_eq!(
            error,
            "bind session grpc 127.0.0.1:19081 failed: Address already in use (os error 98)"
        );
        assert!(!error.contains(env!("CARGO_MANIFEST_DIR")));
    }

    #[test]
    fn foreground_runtime_error_is_returned_instead_of_panicking() {
        assert_eq!(
            run_foreground::<RuntimeFailure, ()>()
                .unwrap_err()
                .to_string(),
            "runtime stopped"
        );
    }

    #[test]
    fn restart_uses_default_or_explicit_config_without_meta_state() {
        let default = cfg_lib::conf::command(CliBasic {
            name: "test-service",
            version: "1",
            author: "test",
            about: "test",
        })
        .try_get_matches_from(["test-service", "restart"])
        .unwrap();
        let (_, args) = default.subcommand().unwrap();
        assert_eq!(config_path(args).unwrap(), "./config.yml");

        let explicit = cfg_lib::conf::command(CliBasic {
            name: "test-service",
            version: "1",
            author: "test",
            about: "test",
        })
        .try_get_matches_from(["test-service", "restart", "-c", "custom.yml"])
        .unwrap();
        let (_, args) = explicit.subcommand().unwrap();
        assert_eq!(config_path(args).unwrap(), "custom.yml");
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
