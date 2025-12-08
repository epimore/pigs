use crate::daemon::Daemon;
use daemonize::{Daemonize, Outcome};
use std::fs::File;
use std::io::Read;
use std::process::{exit, Command};
use std::time::{Duration, Instant};
use std::{env, thread};
use chrono::{DateTime, NaiveDateTime};

// ----------------------------
// Helper: 读取 PID 文件
// ----------------------------
fn read_pid() -> Option<i32> {
    let exe_path = env::current_exe().expect("Failed to get current executable path");
    let pid_file_path = exe_path.with_extension("pid");
    if let Ok(mut file) = File::open(pid_file_path) {
        let mut pid_str = String::new();
        file.read_to_string(&mut pid_str).expect("读取pid信息失败");
        let pid = pid_str.trim().parse::<i32>().ok()?;
        if pid > 0 {
            Some(pid)
        } else {
            None
        }
    } else {
        None
    }
}

// ----------------------------
// Helper: 检查进程是否仍在运行
// ----------------------------
fn is_process_running(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(pid, 0) == 0 }
}

// ----------------------------
// Helper: 删除 PID 文件（静默）
// ----------------------------
fn remove_pid_file() {
    if let Ok(exe) = env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("pid"));
    }
}

// ----------------------------
// 状态检查
// ----------------------------
pub(super) fn status_service() {
    match read_pid() {
        Some(pid) => {
            if is_process_running(pid) {
                println!("Service is running with PID: {}", pid);

                // === 获取并格式化启动时间为 YYYY-MM-DD HH:MM:SS ===
                let mut printed_start_time = false;
                if let Ok(output) = Command::new("ps")
                    .args(&["-p", &pid.to_string(), "-o", "lstart="])
                    .output()
                {
                    if output.status.success() {
                        if let Ok(raw) = String::from_utf8(output.stdout) {
                            let raw = raw.trim();
                            if !raw.is_empty() {
                                // 尝试解析并格式化
                                if let Ok(friendly_time) = format_start_time_friendly(&raw.replace("  ", " ")) {
                                    println!("Started at: {}", friendly_time);
                                    printed_start_time = true;
                                }
                            }
                        }
                    }
                }

                // 如果解析失败，回退到原始输出（保持兼容）
                if !printed_start_time {
                    if let Ok(output) = Command::new("ps")
                        .args(&["-p", &pid.to_string(), "-o", "lstart="])
                        .output()
                    {
                        if let Ok(raw) = String::from_utf8(output.stdout) {
                            println!("Started at: {}", raw.trim());
                        }
                    }
                }

                // === Process info 表格===
                if let Ok(output) = Command::new("ps")
                    .arg("-p")
                    .arg(pid.to_string())
                    .arg("-o")
                    .arg("user,%cpu,%mem,cmd")
                    .output()
                {
                    let info = String::from_utf8_lossy(&output.stdout);
                    let trimmed = info.trim();
                    if !trimmed.is_empty() {
                        // 移除首行 "PID ..." 中的 "PID" 前缀
                        println!("Process info:\n{}", trimmed.trim_start_matches("PID"));
                    }
                }
            } else {
                println!("Service PID file exists (PID {}) but process is not running", pid);
                println!("This may indicate a stale PID file. You can run 'stop' to clean it up.");
            }
        }
        None => {
            println!("Service is not running (no PID file found)");
        }
    }
}

// ----------------------------
// 将 ps 的 lstart 字符串转为 "2025-12-04 10:23:15"
// ----------------------------
fn format_start_time_friendly(s: &str) -> Result<String, Box<dyn std::error::Error>> {
    let formats = [
        "%a %b %e %H:%M:%S %Y", // Thu Dec  4 18:25:03 2025 (注意这里的 %e 对于单数字日有前导空格)
        "%a %b %d %H:%M:%S %Y", // Thu Dec 04 18:25:03 2025
        "%b %e %H:%M",          // Dec  4 18:25 (无年份，适用于今年的进程)
        "%b %d %H:%M",          // Dec 04 18:25 (无年份，适用于今年的进程)
    ];
    let mut parsed_datetime = None;
    for format in &formats {
        if let Ok(dt) = DateTime::parse_from_str(&s, format) {
            parsed_datetime = Some(dt);
            break;
        }
    }

    match parsed_datetime {
        Some(dt) => {
            // 转换为目标格式
            Ok(dt.format("%Y-%m-%d %H:%M:%S").to_string())
        }
        None => {
            // 尝试使用 NaiveDateTime 解析
            if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%a %b %e %H:%M:%S %Y") {
                Ok(dt.format("%Y-%m-%d %H:%M:%S").to_string())
            } else {
                Err("unsupported time format".into())
            }
        }
    }
}

// ----------------------------
// 启动服务
// ----------------------------
pub(super) fn start_service<D, T>()
where
    D: Daemon<T>,
{
    // 检查是否已在运行
    if let Some(pid) = read_pid() {
        if is_process_running(pid) {
            eprintln!("Service already running with PID {}", pid);
            exit(1);
        } else {
            eprintln!("Stale PID file found (PID {} not running). Removing...", pid);
            remove_pid_file();
        }
    }

    let exe_path = env::current_exe().expect("Failed to get current executable path");
    let wd = exe_path.parent().expect("Invalid working directory");
    let uid = users::get_current_uid();
    let gid = users::get_current_gid();

    let daemonize = Daemonize::new()
        .pid_file(exe_path.with_extension("pid"))
        .chown_pid_file(true)
        .working_directory(wd)
        .user(uid)
        .group(gid)
        .privileged_action(move || D::init_privilege());

    match daemonize.execute() {
        Outcome::Child(Ok(child)) => {
            match child.privileged_action_result {
                Ok((d, t)) => {
                    if let Err(e) = d.run_app(t) {
                        eprintln!("App runtime error: {}", e);
                    }
                }
                Err(err) => {
                    eprintln!("Privileged action failed: {}", err);
                }
            }
        }
        Outcome::Child(Err(err)) => {
            eprintln!("Daemonize child error: {}", err);
        }
        Outcome::Parent(Err(err)) => {
            eprintln!("Daemonize parent error: {}", err);
        }
        Outcome::Parent(Ok(parent)) => {
            println!("... Successfully started");
            exit(parent.first_child_exit_code);
        }
    };
}

// ----------------------------
// 停止服务
// ----------------------------
pub(super) fn stop_service() -> bool {
    let pid = match read_pid() {
        Some(p) => p,
        None => {
            println!("Service is not running (no PID file)");
            return true; // 视为已停止
        }
    };

    if !is_process_running(pid) {
        println!("PID {} is not running. Cleaning up stale PID file.", pid);
        remove_pid_file();
        return true;
    }

    println!("Stopping service (PID {})...", pid);

    // 发送 SIGTERM
    if let Err(e) = send_terminate_signal(pid) {
        eprintln!("Failed to send SIGTERM: {}", e);
        return false;
    }

    // 等待优雅退出（最多 10 秒）
    if wait_for_process_exit(pid, 10) {
        println!("Service stopped gracefully.");
        remove_pid_file();
        return true;
    }

    // 超时，强制杀死
    eprintln!("Graceful shutdown timed out. Sending SIGKILL...");
    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
    thread::sleep(Duration::from_millis(200));

    if !is_process_running(pid) {
        println!("Service killed forcefully.");
        remove_pid_file();
        true
    } else {
        eprintln!("ERROR: Failed to kill process {}!", pid);
        false
    }
}

// ----------------------------
// 重启服务
// ----------------------------
pub(super) fn restart_service<D, T>()
where
    D: Daemon<T>,
{
    println!("Restarting service...");
    if stop_service() {
        // 小延迟确保资源释放（可选）
        thread::sleep(Duration::from_millis(300));
        println!("Starting new instance...");
        start_service::<D, T>();
    } else {
        eprintln!("Failed to stop service. Restart aborted.");
    }
}

// ----------------------------
// 内部工具函数
// ----------------------------

fn send_terminate_signal(pid: i32) -> Result<(), std::io::Error> {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("exec kill: {}", e)))?;

    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("kill -TERM {} failed (exit code: {:?})", pid, status.code()),
        ))
    }
}

fn wait_for_process_exit(pid: i32, timeout_secs: u64) -> bool {
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    while start.elapsed() < timeout {
        if !is_process_running(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(200));
    }
    false
}