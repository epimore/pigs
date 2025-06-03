use std::{env, thread};
use std::fs::File;
use std::io::Read;
use std::process::{exit, Command};
use std::time::Duration;
use daemonize::{Daemonize, Outcome};
use crate::daemon::{Daemon};

// 在32位系统中，32768是pid_max的最大值。64位系统，pid_max最大可达2^22。（PID_MAX_LIMIT，大小是4194304）
// cat /proc/sys/kernel/pid_max
fn read_pid() -> Option<i32> {
    let exe_path = env::current_exe().expect("Failed to get current executable path");
    let pid_file_path = exe_path.with_extension("pid");
    if let Ok(mut file) = File::open(pid_file_path) {
        let mut pid_str = String::new();
        file.read_to_string(&mut pid_str).expect("读取pid信息失败");
        let pid = pid_str.trim().parse::<i32>().expect("invalid pid");
        return Some(pid);
    }
    None
}

pub(super) fn start_service<D, T>()
where
    D: Daemon<T>,
{
    let exe_path = env::current_exe().expect("Failed to get current executable path");
    let wd = exe_path.parent().expect("invalid path");
    // 获取当前用户和组的 ID
    let uid = users::get_current_uid();
    let gid = users::get_current_gid();


    //在 Unix 系统中，fork() 调用会复制当前进程的资源，包括代码、内存、文件描述符等。父子进程在 fork() 后共享代码，但会根据 fork() 的返回值进入不同的逻辑分支：
    let daemonize = Daemonize::new()
        .pid_file(exe_path.with_extension("pid"))
        .chown_pid_file(true)
        .working_directory(wd)
        .user(uid) // 设置用户权限
        .group(gid)
        .privileged_action(move || {
            D::init_privilege()
        });

    match daemonize.execute() {
        Outcome::Child(Ok(child)) => {
            match child.privileged_action_result {
                Ok((d, t)) => {
                    match d.run_app(t) {
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("App start run error: {}", e);
                        }
                    }
                }
                Err(err) => {
                    eprintln!("App init error: {}", err);
                }
            }
        }
        Outcome::Child(Err(err)) => {
            eprintln!("Child process error: {}", err);
        }
        Outcome::Parent(Err(err)) => {
            eprintln!("Parent process error: {}", err);
        }
        Outcome::Parent(Ok(parent)) => {
            println!("... Successfully started");
            exit(parent.first_child_exit_code);
        }
    };
}


pub(super) fn stop_service() -> bool {
    let mut b = false;
    match read_pid() {
        None => {
            eprintln!("Service is not running\n   ...failed");
        }
        Some(pid) => {
            if let Err(e) = send_terminate_signal(pid) {
                eprintln!("Failed to stop the service: {}", e);
            } else {
                eprintln!("stop...\n   ...success");
                b = true;
            }
        }
    }
    b
}

pub(super) fn restart_service<D, T>()
where
    D: Daemon<T>,
{
    println!("restart ...");
    if stop_service() {
        thread::sleep(Duration::from_secs(1));
        start_service::<D, T>();
    }
}


fn send_terminate_signal(pid: i32) -> Result<(), std::io::Error> {
    Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .map(|status| {
            if !status.success() {
                Err(std::io::Error::new(std::io::ErrorKind::Other, "Failed to send TERM signal\nThe service may be down."))
            } else {
                Ok(())
            }
        })?
}
