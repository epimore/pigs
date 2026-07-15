#![warn(unsafe_code)]

#[cfg(target_os = "linux")]
use std::fs;
use std::io;
use std::time::{Duration, Instant};

#[cfg(not(target_os = "linux"))]
use sysinfo::{Networks, ProcessesToUpdate, System, get_current_pid};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostMetrics {
    pub cpu_usage_percent: f64,
    pub load_average_1m: f64,
    pub load_average_5m: f64,
    pub load_average_15m: f64,
    pub memory_total_bytes: u64,
    pub memory_used_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub disk_read_bytes_per_sec: u64,
    pub disk_write_bytes_per_sec: u64,
    pub network_receive_bytes_per_sec: u64,
    pub network_transmit_bytes_per_sec: u64,
    pub process_resident_memory_bytes: u64,
    pub process_threads: u32,
}

#[derive(Debug, Clone, Copy, Default)]
struct Counters {
    #[cfg(target_os = "linux")]
    cpu_total: u64,
    #[cfg(target_os = "linux")]
    cpu_idle: u64,
    #[cfg(target_os = "linux")]
    disk_read_bytes: u64,
    #[cfg(target_os = "linux")]
    disk_write_bytes: u64,
    network_receive_bytes: u64,
    network_transmit_bytes: u64,
}

#[derive(Debug)]
pub struct HostMetricsCollector {
    previous: Option<(Instant, Counters)>,
    #[cfg(not(target_os = "linux"))]
    system: System,
    #[cfg(not(target_os = "linux"))]
    networks: Networks,
    #[cfg(not(target_os = "linux"))]
    current_pid: Option<sysinfo::Pid>,
}

impl Default for HostMetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl HostMetricsCollector {
    #[cfg(target_os = "linux")]
    #[must_use]
    pub const fn new() -> Self {
        Self { previous: None }
    }

    #[cfg(not(target_os = "linux"))]
    #[must_use]
    pub fn new() -> Self {
        Self {
            previous: None,
            system: System::new(),
            networks: Networks::new_with_refreshed_list(),
            current_pid: get_current_pid().ok(),
        }
    }

    /// Samples the current host metrics.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when a required Linux procfs metric cannot be read
    /// or parsed.
    pub fn sample(&mut self) -> io::Result<HostMetrics> {
        #[cfg(target_os = "linux")]
        {
            sample_from(&ProcReader, &mut self.previous)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(sample_from_sysinfo(
                &mut self.system,
                &mut self.networks,
                self.current_pid,
                &mut self.previous,
            ))
        }
    }
}

#[cfg(target_os = "linux")]
trait MetricsReader {
    fn read(&self, path: &str) -> io::Result<String>;
}

#[cfg(target_os = "linux")]
struct ProcReader;

#[cfg(target_os = "linux")]
impl MetricsReader for ProcReader {
    fn read(&self, path: &str) -> io::Result<String> {
        fs::read_to_string(path)
    }
}

#[cfg(target_os = "linux")]
fn sample_from(
    reader: &impl MetricsReader,
    previous: &mut Option<(Instant, Counters)>,
) -> io::Result<HostMetrics> {
    let now = Instant::now();
    let (cpu_total, cpu_idle) = parse_cpu(&reader.read("/proc/stat")?)?;
    let load_average = parse_load(&reader.read("/proc/loadavg")?)?;
    let memory = parse_memory(&reader.read("/proc/meminfo")?);
    let (disk_read_bytes, disk_write_bytes) = parse_disk(&reader.read("/proc/diskstats")?);
    let (network_receive_bytes, network_transmit_bytes) =
        parse_network(&reader.read("/proc/net/dev")?);
    let (process_resident_memory_bytes, process_threads) =
        parse_process(&reader.read("/proc/self/status")?);
    let current = Counters {
        cpu_total,
        cpu_idle,
        disk_read_bytes,
        disk_write_bytes,
        network_receive_bytes,
        network_transmit_bytes,
    };
    let mut metrics = HostMetrics {
        load_average_1m: load_average.0,
        load_average_5m: load_average.1,
        load_average_15m: load_average.2,
        memory_total_bytes: memory.0,
        memory_used_bytes: memory.0.saturating_sub(memory.1),
        swap_total_bytes: memory.2,
        swap_used_bytes: memory.2.saturating_sub(memory.3),
        process_resident_memory_bytes,
        process_threads,
        ..HostMetrics::default()
    };
    if let Some((previous_at, before)) = previous {
        let elapsed = now.duration_since(*previous_at);
        if !elapsed.is_zero() {
            let total_delta = current.cpu_total.saturating_sub(before.cpu_total);
            let idle_delta = current.cpu_idle.saturating_sub(before.cpu_idle);
            if let Some(busy_basis_points) = total_delta
                .saturating_sub(idle_delta)
                .saturating_mul(10_000)
                .checked_div(total_delta)
            {
                metrics.cpu_usage_percent =
                    f64::from(u32::try_from(busy_basis_points).unwrap_or(10_000)) / 100.0;
            }
            metrics.disk_read_bytes_per_sec =
                rate(current.disk_read_bytes, before.disk_read_bytes, elapsed);
            metrics.disk_write_bytes_per_sec =
                rate(current.disk_write_bytes, before.disk_write_bytes, elapsed);
            metrics.network_receive_bytes_per_sec = rate(
                current.network_receive_bytes,
                before.network_receive_bytes,
                elapsed,
            );
            metrics.network_transmit_bytes_per_sec = rate(
                current.network_transmit_bytes,
                before.network_transmit_bytes,
                elapsed,
            );
        }
    }
    *previous = Some((now, current));
    Ok(metrics)
}

#[cfg(not(target_os = "linux"))]
fn sample_from_sysinfo(
    system: &mut System,
    networks: &mut Networks,
    current_pid: Option<sysinfo::Pid>,
    previous: &mut Option<(Instant, Counters)>,
) -> HostMetrics {
    let now = Instant::now();
    system.refresh_memory();
    system.refresh_cpu_usage();
    if let Some(pid) = current_pid {
        let pids = [pid];
        system.refresh_processes(ProcessesToUpdate::Some(&pids), true);
    }
    networks.refresh(true);

    let load = System::load_average();
    let (process_resident_memory_bytes, process_threads) = current_pid
        .and_then(|pid| system.process(pid))
        .map_or((0, 0), |process| {
            let threads = process
                .tasks()
                .map_or(0, |tasks| tasks.len().min(u32::MAX as usize) as u32);
            (process.memory(), threads)
        });

    let mut metrics = HostMetrics {
        cpu_usage_percent: f64::from(system.global_cpu_usage()),
        load_average_1m: load.one,
        load_average_5m: load.five,
        load_average_15m: load.fifteen,
        memory_total_bytes: system.total_memory(),
        memory_used_bytes: system.used_memory(),
        swap_total_bytes: system.total_swap(),
        swap_used_bytes: system.used_swap(),
        process_resident_memory_bytes,
        process_threads,
        ..HostMetrics::default()
    };

    let current = Counters {
        network_receive_bytes: networks.iter().map(|(_, data)| data.total_received()).sum(),
        network_transmit_bytes: networks
            .iter()
            .map(|(_, data)| data.total_transmitted())
            .sum(),
        ..Counters::default()
    };
    if let Some((previous_at, before)) = previous {
        let elapsed = now.duration_since(*previous_at);
        if !elapsed.is_zero() {
            metrics.network_receive_bytes_per_sec = rate(
                current.network_receive_bytes,
                before.network_receive_bytes,
                elapsed,
            );
            metrics.network_transmit_bytes_per_sec = rate(
                current.network_transmit_bytes,
                before.network_transmit_bytes,
                elapsed,
            );
        }
    }
    *previous = Some((now, current));
    metrics
}

fn rate(current: u64, previous: u64, elapsed: Duration) -> u64 {
    let elapsed_nanos = elapsed.as_nanos();
    if elapsed_nanos == 0 {
        return 0;
    }

    let bytes = u128::from(current.saturating_sub(previous));
    let per_second = bytes
        .saturating_mul(1_000_000_000)
        .saturating_add(elapsed_nanos / 2)
        / elapsed_nanos;
    u64::try_from(per_second).unwrap_or(u64::MAX)
}

#[cfg(target_os = "linux")]
fn parse_cpu(input: &str) -> io::Result<(u64, u64)> {
    let line = input
        .lines()
        .find(|line| line.starts_with("cpu "))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing aggregate cpu line"))?;
    let values = line
        .split_whitespace()
        .skip(1)
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let total = values.iter().sum();
    let idle = values
        .get(3)
        .copied()
        .unwrap_or(0)
        .saturating_add(values.get(4).copied().unwrap_or(0));
    Ok((total, idle))
}

#[cfg(target_os = "linux")]
fn parse_load(input: &str) -> io::Result<(f64, f64, f64)> {
    let values = input
        .split_whitespace()
        .take(3)
        .map(str::parse::<f64>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if values.len() != 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid loadavg",
        ));
    }
    Ok((values[0], values[1], values[2]))
}

#[cfg(target_os = "linux")]
fn parse_memory(input: &str) -> (u64, u64, u64, u64) {
    let value = |name: &str| {
        input
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
            .saturating_mul(1024)
    };
    (
        value("MemTotal:"),
        value("MemAvailable:"),
        value("SwapTotal:"),
        value("SwapFree:"),
    )
}

#[cfg(target_os = "linux")]
fn parse_disk(input: &str) -> (u64, u64) {
    input
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let name = *fields.get(2)?;
            if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("fd") {
                return None;
            }
            let read_sectors = fields.get(5)?.parse::<u64>().ok()?;
            let write_sectors = fields.get(9)?.parse::<u64>().ok()?;
            Some((
                read_sectors.saturating_mul(512),
                write_sectors.saturating_mul(512),
            ))
        })
        .fold((0u64, 0u64), |acc, value| {
            (acc.0.saturating_add(value.0), acc.1.saturating_add(value.1))
        })
}

#[cfg(target_os = "linux")]
fn parse_network(input: &str) -> (u64, u64) {
    input
        .lines()
        .skip(2)
        .filter_map(|line| {
            let (name, values) = line.split_once(':')?;
            if name.trim() == "lo" {
                return None;
            }
            let fields = values.split_whitespace().collect::<Vec<_>>();
            Some((
                fields.first()?.parse::<u64>().ok()?,
                fields.get(8)?.parse::<u64>().ok()?,
            ))
        })
        .fold((0u64, 0u64), |acc, value| {
            (acc.0.saturating_add(value.0), acc.1.saturating_add(value.1))
        })
}

#[cfg(target_os = "linux")]
fn parse_process(input: &str) -> (u64, u32) {
    let rss = input
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_mul(1024);
    let threads = input
        .lines()
        .find_map(|line| line.strip_prefix("Threads:"))
        .and_then(|rest| rest.trim().parse::<u32>().ok())
        .unwrap_or(0);
    (rss, threads)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculates_rounded_integer_rate_without_float_conversion() {
        assert_eq!(rate(1_500, 500, Duration::from_secs(2)), 500);
        assert_eq!(rate(2, 0, Duration::from_secs(3)), 1);
        assert_eq!(rate(2, 1, Duration::ZERO), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_linux_proc_shapes() {
        assert_eq!(
            parse_cpu(
                "cpu  100 2 30 400 10 0 0 0
"
            )
            .unwrap(),
            (542, 410)
        );
        assert_eq!(
            parse_load(
                "1.25 0.75 0.50 1/100 1
"
            )
            .unwrap(),
            (1.25, 0.75, 0.5)
        );
        assert_eq!(
            parse_memory(
                "MemTotal: 1000 kB
MemAvailable: 400 kB
SwapTotal: 200 kB
SwapFree: 50 kB
"
            ),
            (1_024_000, 409_600, 204_800, 51_200)
        );
        assert_eq!(
            parse_network(
                "h1
h2
 eth0: 100 0 0 0 0 0 0 0 200 0 0 0 0 0 0 0
 lo: 9 0 0 0 0 0 0 0 9 0 0 0 0 0 0 0
"
            ),
            (100, 200)
        );
        assert_eq!(
            parse_process(
                "VmRSS: 42 kB
Threads: 7
"
            ),
            (43_008, 7)
        );
    }

    #[test]
    fn samples_current_host() {
        let sample = HostMetricsCollector::new().sample().unwrap();
        assert!(sample.memory_total_bytes > 0);
        #[cfg(target_os = "linux")]
        assert!(sample.process_threads > 0);
    }
}
