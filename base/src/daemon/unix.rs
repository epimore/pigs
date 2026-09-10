use crate::daemon::{CommandReport, Daemon};
use crate::utils::rt::DAEMON_STOP_TIMEOUT_SECS;
use chrono::{DateTime, NaiveDateTime};
use daemonize::{Daemonize, Outcome};
use exception::{GlobalError, GlobalResult};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use std::{env, thread};

fn pid_file_path() -> GlobalResult<PathBuf> {
    env::current_exe()
        .map(|path| path.with_extension("pid"))
        .map_err(external_error)
}

fn read_pid() -> GlobalResult<Option<i32>> {
    let path = pid_file_path()?;
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(external_error(error)),
    };
    let pid = content.trim().parse::<i32>().map_err(external_error)?;
    if pid <= 0 {
        return Err(daemon_error("PID file contains a non-positive PID"));
    }
    Ok(Some(pid))
}

fn process_exists(pid: i32) -> GlobalResult<bool> {
    if pid <= 0 {
        return Ok(false);
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(external_error(error)),
    }
}

fn remove_pid_file() -> GlobalResult<()> {
    match fs::remove_file(pid_file_path()?) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(external_error(error)),
    }
}

#[cfg(target_os = "linux")]
fn process_executable(pid: i32) -> GlobalResult<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe")).map_err(external_error)
}

#[cfg(target_os = "macos")]
fn process_executable(pid: i32) -> GlobalResult<PathBuf> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .map_err(external_error)?;
    if !output.status.success() {
        return Err(daemon_error("query process executable failed"));
    }
    let path = String::from_utf8(output.stdout).map_err(external_error)?;
    let path = path.trim();
    if path.is_empty() {
        return Err(daemon_error("process executable is unavailable"));
    }
    Ok(PathBuf::from(path))
}

fn canonicalize_existing(path: &Path) -> GlobalResult<PathBuf> {
    fs::canonicalize(path).map_err(external_error)
}

fn verify_process_identity(pid: i32) -> GlobalResult<()> {
    let expected = canonicalize_existing(&env::current_exe().map_err(external_error)?)?;
    let actual = canonicalize_existing(&process_executable(pid)?)?;
    if actual != expected {
        return Err(daemon_error(&format!(
            "PID {pid} belongs to a different executable; refusing to signal it"
        )));
    }
    Ok(())
}

pub(super) fn status_service() -> GlobalResult<CommandReport> {
    let Some(pid) = read_pid()? else {
        return Ok(CommandReport::line(
            "Service is not running (no PID file found)",
        ));
    };
    if !process_exists(pid)? {
        return Ok(CommandReport::line(format!(
            "Service PID file is stale (PID {pid} is not running)"
        )));
    }
    verify_process_identity(pid)?;
    let mut report = CommandReport::line(format!("Service is running with PID: {pid}"));
    if let Ok(output) = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
    {
        if output.status.success() {
            let raw = String::from_utf8_lossy(&output.stdout);
            let raw = raw.trim().replace("  ", " ");
            if !raw.is_empty() {
                let start = format_start_time_friendly(&raw).unwrap_or(raw);
                report.push(format!("Started at: {start}"));
            }
        }
    }
    if let Ok(output) = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "user,%cpu,%mem,cmd"])
        .output()
    {
        if output.status.success() {
            let info = String::from_utf8_lossy(&output.stdout);
            let info = info.trim();
            if !info.is_empty() {
                report.push(format!("Process info:\n{}", info.trim_start_matches("PID")));
            }
        }
    }
    Ok(report)
}

fn format_start_time_friendly(value: &str) -> Option<String> {
    for format in [
        "%a %b %e %H:%M:%S %Y",
        "%a %b %d %H:%M:%S %Y",
        "%b %e %H:%M",
        "%b %d %H:%M",
    ] {
        if let Ok(datetime) = DateTime::parse_from_str(value, format) {
            return Some(datetime.format("%Y-%m-%d %H:%M:%S").to_string());
        }
    }
    NaiveDateTime::parse_from_str(value, "%a %b %e %H:%M:%S %Y")
        .ok()
        .map(|datetime| datetime.format("%Y-%m-%d %H:%M:%S").to_string())
}

pub(super) fn start_service<D, T>() -> GlobalResult<CommandReport>
where
    D: Daemon<T>,
{
    if let Some(pid) = read_pid()? {
        if process_exists(pid)? {
            verify_process_identity(pid)?;
            return Err(daemon_error(&format!(
                "service is already running with PID {pid}"
            )));
        }
        remove_pid_file()?;
    }

    let exe_path = env::current_exe().map_err(external_error)?;
    let working_directory = exe_path
        .parent()
        .ok_or_else(|| daemon_error("service executable has no parent directory"))?;
    let daemonize = Daemonize::new()
        .pid_file(exe_path.with_extension("pid"))
        .chown_pid_file(true)
        .working_directory(working_directory)
        .user(users::get_current_uid())
        .group(users::get_current_gid())
        .privileged_action(move || D::init_privilege());

    match daemonize.execute() {
        Outcome::Child(Ok(child)) => {
            let (daemon, bootstrap) = child.privileged_action_result?;
            daemon.run_app(bootstrap)?;
            Ok(CommandReport::default())
        }
        Outcome::Child(Err(error)) | Outcome::Parent(Err(error)) => Err(external_error(error)),
        Outcome::Parent(Ok(parent)) if parent.first_child_exit_code == 0 => {
            Ok(CommandReport::line("Service started successfully"))
        }
        Outcome::Parent(Ok(parent)) => Err(daemon_error(&format!(
            "daemon child exited with code {}",
            parent.first_child_exit_code
        ))),
    }
}

pub(super) fn stop_service() -> GlobalResult<CommandReport> {
    stop_service_with_timeout(Duration::from_secs(DAEMON_STOP_TIMEOUT_SECS))
}

fn stop_service_with_timeout(timeout: Duration) -> GlobalResult<CommandReport> {
    let Some(pid) = read_pid()? else {
        return Ok(CommandReport::line("Service is not running (no PID file)"));
    };
    if !process_exists(pid)? {
        remove_pid_file()?;
        return Ok(CommandReport::line(format!(
            "Removed stale PID file for PID {pid}"
        )));
    }
    verify_process_identity(pid)?;
    send_signal(pid, libc::SIGTERM)?;
    if wait_for_process_exit(pid, timeout)? {
        remove_pid_file()?;
        return Ok(CommandReport::line(format!(
            "Service stopped after SIGTERM (PID {pid})"
        )));
    }
    send_signal(pid, libc::SIGKILL)?;
    if !wait_for_process_exit(pid, Duration::from_secs(2))? {
        return Err(daemon_error(&format!("failed to kill service PID {pid}")));
    }
    remove_pid_file()?;
    Ok(CommandReport::line(format!(
        "Service stopped after SIGKILL (PID {pid})"
    )))
}

fn send_signal(pid: i32, signal: i32) -> GlobalResult<()> {
    if unsafe { libc::kill(pid, signal) } == 0 {
        Ok(())
    } else {
        Err(external_error(std::io::Error::last_os_error()))
    }
}

fn wait_for_process_exit(pid: i32, timeout: Duration) -> GlobalResult<bool> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if !process_exists(pid)? {
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(200));
    }
    Ok(false)
}

fn daemon_error(message: &str) -> GlobalError {
    GlobalError::new_sys_error(message, |_| {})
}

fn external_error<E>(error: E) -> GlobalError
where
    E: std::error::Error + Send + Sync + 'static,
{
    GlobalError::from_external_error(error, |_| {})
}

#[cfg(test)]
mod tests {
    use super::{
        format_start_time_friendly, pid_file_path, stop_service_with_timeout,
        verify_process_identity,
    };
    use std::env;
    use std::fs;
    use std::process::Command;
    use std::thread;
    use std::time::Duration;

    const IGNORE_TERM_HELPER_ENV: &str = "BASE_DAEMON_IGNORE_TERM_HELPER";

    #[test]
    fn formats_ps_start_time() {
        assert_eq!(
            format_start_time_friendly("Thu Dec 4 18:25:03 2025").as_deref(),
            Some("2025-12-04 18:25:03")
        );
    }

    #[test]
    fn refuses_pid_owned_by_a_different_executable() {
        let mut child = Command::new("sleep").arg("5").spawn().unwrap();
        let error = verify_process_identity(child.id() as i32).unwrap_err();
        assert!(error.to_string().contains("different executable"));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn sigkill_escalation_is_checked_and_removes_pid_file() {
        let mut child = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "daemon::unix::tests::ignore_sigterm_process_helper",
                "--nocapture",
            ])
            .env(IGNORE_TERM_HELPER_ENV, "1")
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_millis(100));
        fs::write(pid_file_path().unwrap(), child.id().to_string()).unwrap();
        let reaper = thread::spawn(move || child.wait().unwrap());

        let report = stop_service_with_timeout(Duration::from_millis(100)).unwrap();

        assert!(report.lines[0].contains("SIGKILL"));
        assert!(!pid_file_path().unwrap().exists());
        assert!(!reaper.join().unwrap().success());
    }

    #[test]
    fn ignore_sigterm_process_helper() {
        if env::var_os(IGNORE_TERM_HELPER_ENV).is_none() {
            return;
        }
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
        thread::sleep(Duration::from_secs(30));
    }
}
