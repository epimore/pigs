#![cfg(unix)]

use base::daemon::signal::ExitSignal;
use base::utils::rt::{GlobalRuntime, RuntimeType};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const HELPER_ENV: &str = "BASE_GRACEFUL_SHUTDOWN_HELPER";

#[test]
fn graceful_shutdown_process_helper() {
    if std::env::var_os(HELPER_ENV).is_none() {
        return;
    }
    let runtime = GlobalRuntime::register_default(RuntimeType::CommonNetwork)
        .expect("register helper runtime");
    let cancel = runtime.cancel.clone();
    runtime
        .spawn("process-helper", async move { cancel.cancelled().await })
        .expect("spawn helper task");
    println!("READY");
    std::io::stdout().flush().expect("flush helper readiness");
    let report = GlobalRuntime::order_shutdown(&[RuntimeType::CommonNetwork]);
    assert_eq!(report.signal, ExitSignal::Terminate);
    assert!(report.is_graceful());
    assert!(report.elapsed < Duration::from_millis(750));
}

#[test]
fn sigterm_exits_after_managed_shutdown() {
    if std::env::var_os(HELPER_ENV).is_some() {
        return;
    }
    let mut child = Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "graceful_shutdown_process_helper", "--nocapture"])
        .env(HELPER_ENV, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn graceful shutdown helper");
    let stdout = child.stdout.take().expect("helper stdout");
    let (line_tx, line_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let ready_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = ready_deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "helper did not become ready");
        let line = line_rx
            .recv_timeout(remaining)
            .expect("read helper readiness");
        if line == "READY" {
            break;
        }
    }
    std::thread::sleep(Duration::from_secs(1));
    let signal_status = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(signal_status, 0, "send SIGTERM to helper");

    let exit_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().expect("wait for helper") {
            assert!(status.success(), "helper exited with {status}");
            break;
        }
        if Instant::now() >= exit_deadline {
            child.kill().expect("kill stuck helper");
            let _ = child.wait();
            panic!("helper did not exit after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
