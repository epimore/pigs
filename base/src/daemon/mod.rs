#[cfg(unix)]
mod unix;

use std::{env, fs};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use exception::GlobalResult;

pub trait Daemon<T> {
    fn init_privilege() -> GlobalResult<(Self, T)>
    where
        Self: Sized;
    fn run_app(self, t: T) -> GlobalResult<()>;
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
        file.write_all(content.as_bytes()).expect("Failed to write meta");
    }

    fn load_meta() -> Self {
        let meta_path = Self::get_meta_file_path();
        let content = fs::read_to_string(meta_path).ok().expect("Failed to read meta file");
        serde_json::from_str(&content).expect("Failed to deserialize meta")
    }
}

pub fn run<D, T>()
where
    D: Daemon<T>,
{
    let arg_matches = cfg_lib::conf::get_arg_match();
    match arg_matches.subcommand() {
        Some(("start", args)) => {
            let config_path = args.try_get_one::<String>("config").expect("get config failed").expect("not found config").to_string();
            cfg_lib::conf::init_cfg(config_path.clone());
            let daemon = args.get_flag("daemon");
            let meta = DaemonMeta {
                config_path,
                daemon,
            };
            meta.save_meta();
            if daemon {
                if cfg!(target_os = "linux") || cfg!(target_os = "macos") {
                    #[cfg(unix)]
                    {
                        unix::start_service::<D, T>();
                    }
                } else {
                    eprintln!("The daemon only supports macOS, and Linux");
                    match D::init_privilege() {
                        Ok((d, t)) => {
                            d.run_app(t).expect("Failed to run app");
                        }
                        Err(err) => {
                            panic!("App init error: {}", err);
                        }
                    }
                }
            } else {
                match D::init_privilege() {
                    Ok((d, t)) => {
                        d.run_app(t).expect("Failed to run app");
                    }
                    Err(err) => {
                        panic!("App init error: {}", err);
                    }
                }
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
        _other => {
            eprintln!("Please add subcommands to operate: [start|stop|restart]")
        }
    }
}
