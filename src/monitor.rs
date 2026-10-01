//! Read-only host-side dashboard. Never launches agents or holds session leases.
use crate::{backend::Backend, project};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use std::{
    collections::HashMap,
    io::{IsTerminal, Read, Write},
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const INTERVAL: Duration = Duration::from_secs(1);
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct View {
    id: String,
    project: String,
    state: String,
    ports: Vec<project::Port>,
    cpu: Option<f64>,
    memory: Option<u64>,
    limit: Option<u64>,
    pids: Option<u64>,
    processes: Vec<Process>,
    warning: Option<String>,
}
struct Process {
    pid: u32,
    parent: u32,
    seconds: u64,
    cpu: f64,
    memory: f64,
    command: String,
}

/// Monitor subprocesses are bounded and interruptible. Killing one only ends a
/// stats/list/ps client, never an agent session or the container itself.
fn capture(b: &Backend, args: &[&str], stop: &AtomicBool) -> Result<Vec<u8>> {
    if stop.load(Ordering::Relaxed) {
        bail!("monitor interrupted");
    }
    let mut child = b
        .command(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run Apple container")?;
    let mut stdout = child.stdout.take().context("missing stdout")?;
    let mut stderr = child.stderr.take().context("missing stderr")?;
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if stop.load(Ordering::Relaxed) || started.elapsed() >= TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out.join();
            let _ = err.join();
            bail!("monitor query interrupted or timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let bytes = out
        .join()
        .map_err(|_| anyhow::anyhow!("stdout reader failed"))??;
    let errors = err
        .join()
        .map_err(|_| anyhow::anyhow!("stderr reader failed"))??;
    if !status.success() {
        bail!(
            "container query failed: {}",
            String::from_utf8_lossy(&errors).trim()
        );
    }
    Ok(bytes)
}

fn state(
    b: &Backend,
    record: &project::Record,
    list: &[Value],
    stop: &AtomicBool,
) -> Result<String> {
    let Some(entry) = list.iter().find(|entry| {
        entry
            .get("id")
            .or_else(|| entry.pointer("/configuration/id"))
            .and_then(Value::as_str)
            == Some(record.id.as_str())
    }) else {
        return Ok("missing".into());
    };
    let expected = record.root.to_string_lossy();
    let label = entry
        .pointer("/configuration/labels/io.jail.project")
        .or_else(|| entry.pointer("/labels/io.jail.project"));
    if label.and_then(Value::as_str) != Some(expected.as_ref()) {
        let inspection: Value =
            serde_json::from_slice(&capture(b, &["inspect", &record.id], stop)?)?;
        let inspection = inspection
            .as_array()
            .and_then(|a| a.first())
            .unwrap_or(&inspection);
        let label = inspection
            .pointer("/configuration/labels/io.jail.project")
            .or_else(|| inspection.pointer("/labels/io.jail.project"));
        if label.and_then(Value::as_str) != Some(expected.as_ref()) {
            bail!("ownership label mismatch; refusing to monitor this container");
        }
    }
    entry
        .pointer("/status/state")
        .or_else(|| entry.get("status"))
        .or_else(|| entry.get("state"))
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .context("container state is missing")
}

fn processes(bytes: &[u8]) -> Vec<Process> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let parent = fields.next()?.parse().ok()?;
            let uid: u32 = fields.next()?.parse().ok()?;
            let seconds = fields.next()?.parse().ok()?;
            let cpu = fields.next()?.parse().ok()?;
            let memory = fields.next()?.parse().ok()?;
            // Hide the trusted supervisor/proxy and the monitoring ps process.
            if uid == 0 {
                return None;
            }
            let command = fields.collect::<Vec<_>>().join(" ");
            if command.is_empty() {
                return None;
            }
            Some(Process {
                pid,
                parent,
                seconds,
                cpu,
                memory,
                command,
            })
        })
        .collect()
}

fn cpu_usage(old: Option<&(u64, Instant)>, current: u64, now: Instant) -> Option<f64> {
    let (counter, time) = old?;
    let elapsed = now.checked_duration_since(*time)?.as_secs_f64();
    if elapsed <= 0.0 || current < *counter {
        return None;
    }
    Some((current - counter) as f64 / (elapsed * 1_000_000.0) * 100.0)
}

fn sample(
    b: &Backend,
    records: &[project::Record],
    previous: &mut HashMap<String, (u64, Instant)>,
    stop: &AtomicBool,
) -> Result<Vec<View>> {
    if records.is_empty() {
        previous.clear();
        return Ok(Vec::new());
    }
    let list: Vec<Value> =
        serde_json::from_slice(&capture(b, &["list", "--all", "--format", "json"], stop)?)?;
    let mut views = records
        .iter()
        .map(|record| {
            let mut view = View {
                id: record.id.clone(),
                project: record.root.display().to_string(),
                ports: record.ports.clone(),
                ..View::default()
            };
            match state(b, record, &list, stop) {
                Ok(state) => view.state = state,
                Err(error) => {
                    view.state = "unavailable".into();
                    view.warning = Some(error.to_string());
                }
            }
            view
        })
        .collect::<Vec<_>>();
    let running = views
        .iter()
        .filter(|v| v.state == "running")
        .map(|v| v.id.as_str())
        .collect::<Vec<_>>();
    previous.retain(|id, _| running.contains(&id.as_str()));
    if running.is_empty() {
        return Ok(views);
    }
    let mut args = vec!["stats", "--no-stream", "--format", "json"];
    args.extend(running);
    let stats: Result<Vec<Value>> =
        capture(b, &args, stop).and_then(|bytes| Ok(serde_json::from_slice(&bytes)?));
    let sampled = Instant::now();
    for view in views.iter_mut().filter(|v| v.state == "running") {
        match &stats {
            Ok(stats) => {
                if let Some(stat) = stats.iter().find(|s| s["id"].as_str() == Some(&view.id)) {
                    view.memory = stat["memoryUsageBytes"].as_u64();
                    view.limit = stat["memoryLimitBytes"].as_u64();
                    view.pids = stat["numProcesses"].as_u64();
                    if let Some(counter) = stat["cpuUsageUsec"].as_u64() {
                        view.cpu = cpu_usage(previous.get(&view.id), counter, sampled);
                        previous.insert(view.id.clone(), (counter, sampled));
                    }
                }
            }
            Err(error) => view.warning = Some(error.to_string()),
        }
        // Executable names only: argv may contain prompts, API keys or tokens.
        match capture(
            b,
            &[
                "exec",
                "--user",
                "root",
                &view.id,
                "ps",
                "-eo",
                "pid=,ppid=,uid=,etimes=,pcpu=,pmem=,comm=",
            ],
            stop,
        ) {
            Ok(bytes) => view.processes = processes(&bytes),
            Err(error) => view.warning = Some(error.to_string()),
        }
    }
    Ok(views)
}

fn memory(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}
fn elapsed(seconds: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

// Control characters from paths, process names or errors must never become
// terminal escape sequences. Account for common wide/combining characters.
fn char_width(c: char) -> usize {
    match c as u32 {
        0x0300..=0x036f | 0xfe00..=0xfe0f | 0x200d => 0,
        0x1100..=0x115f
        | 0x2329..=0x232a
        | 0x2e80..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe10..=0xfe19
        | 0xfe30..=0xfe6f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1faff
        | 0x20000..=0x3fffd => 2,
        _ => 1,
    }
}
fn clipped(line: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for c in line.chars() {
        let c = if c.is_control() { ' ' } else { c };
        let size = char_width(c);
        if used + size > width {
            break;
        }
        result.push(c);
        used += size;
    }
    result
}

fn lines(
    views: &[View],
    age: Duration,
    sample_age: Option<Duration>,
    warning: Option<&str>,
) -> Vec<String> {
    let freshness = sample_age
        .map(|age| format!("sample {:.1}s ago", age.as_secs_f64()))
        .unwrap_or("loading samples".into());
    let mut lines = vec![format!(
        "JAIL LIVE | {} | screen 1s | {freshness}",
        elapsed(age.as_secs()),
    )];
    if let Some(warning) = warning {
        lines.push(format!("! {warning}"));
    }
    if views.is_empty() && warning.is_none() {
        lines.push("No jail container yet. Start jail claude/codex in this project.".into());
    }
    for view in views {
        lines.push(String::new());
        lines.push(format!(
            "{}  [{}]",
            view.id,
            view.state.to_ascii_uppercase()
        ));
        lines.push(format!("Project: {}", view.project));
        for port in &view.ports {
            lines.push(format!("Port: {}", port.description()));
        }
        if view.state != "running" {
            continue;
        }
        let cpu = view.cpu.map(|v| format!("{v:.1}%")).unwrap_or("--".into());
        let memory = match (view.memory, view.limit) {
            (Some(used), Some(limit)) if limit > 0 => {
                let ratio = used as f64 / limit as f64;
                let fill = ((ratio.clamp(0.0, 1.0) * 10.0).round() as usize).min(10);
                format!(
                    "[{}{}] {} / {} ({:.1}%)",
                    "#".repeat(fill),
                    "-".repeat(10 - fill),
                    memory(used),
                    memory(limit),
                    ratio * 100.0
                )
            }
            _ => "--".into(),
        };
        lines.push(format!(
            "CPU {cpu:>6}  MEM {memory}  PIDS {}",
            view.pids.map(|n| n.to_string()).unwrap_or("--".into())
        ));
        if let Some(warning) = &view.warning {
            lines.push(format!("! {warning}"));
        }
        lines.push("   PID   CPU%   MEM%   ELAPSED  COMMAND".into());
        if view.processes.is_empty() {
            lines.push("   (no agent processes / idle)".into());
        }
        for process in &view.processes {
            let mut parent = process.parent;
            let mut depth = 0;
            while depth < 4 {
                let Some(ancestor) = view
                    .processes
                    .iter()
                    .find(|p| p.pid == parent && p.pid != process.pid)
                else {
                    break;
                };
                parent = ancestor.parent;
                depth += 1;
            }
            lines.push(format!(
                "{:>6} {:>6.1} {:>6.1}  {}  {}{}",
                process.pid,
                process.cpu,
                process.memory,
                elapsed(process.seconds),
                "  ".repeat(depth),
                process.command
            ));
        }
    }
    lines
}

struct Screen;
impl Screen {
    fn enter() -> Result<Self> {
        // RAII restores the terminal even when a query or write fails.
        let screen = Self;
        let mut out = std::io::stdout().lock();
        write!(out, "\x1b[?1049h\x1b[?25l")?;
        out.flush()?;
        Ok(screen)
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        let mut out = std::io::stdout().lock();
        let _ = write!(out, "\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = out.flush();
    }
}
fn dimensions() -> (usize, usize) {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
        && size.ws_col > 0
        && size.ws_row > 0
    {
        (size.ws_col as usize, size.ws_row as usize)
    } else {
        (80, 24)
    }
}
fn draw(lines: &[String]) -> Result<()> {
    let (width, height) = dimensions();
    // Leave the last column free: writing it can wrap and scroll the screen.
    let width = width.saturating_sub(1);
    let content_rows = height.saturating_sub(2);
    let mut frame = String::from("\x1b[H");
    for (i, line) in lines.iter().take(content_rows).enumerate() {
        frame.push_str("\x1b[2K");
        if i == 0 {
            frame.push_str("\x1b[1;36m");
        }
        frame.push_str(&clipped(line, width));
        frame.push_str("\x1b[0m\r\n");
    }
    frame.push_str("\x1b[2K\x1b[2m");
    let hidden = lines.len().saturating_sub(content_rows);
    let footer = if hidden > 0 {
        format!("Ctrl+C: exit monitor | {hidden} rows hidden; enlarge terminal")
    } else {
        "Ctrl+C: exit monitor | CPU 100%=1 core | command args hidden".into()
    };
    frame.push_str(&clipped(&footer, width));
    frame.push_str("\x1b[0m\x1b[J");
    let mut out = std::io::stdout().lock();
    out.write_all(frame.as_bytes())?;
    out.flush()?;
    Ok(())
}

pub fn run(b: &Backend, data: &Path, project: &project::Project, all: bool) -> Result<i32> {
    let stop = Arc::new(AtomicBool::new(false));
    let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP])?;
    let handle = signals.handle();
    let signaled = stop.clone();
    let thread = std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            signaled.store(true, Ordering::Relaxed);
        }
    });
    let result = (|| {
        let live =
            std::io::stdout().is_terminal() && std::env::var("TERM").as_deref() != Ok("dumb");
        let _screen = if live { Some(Screen::enter()?) } else { None };
        let started = Instant::now();
        let records = || -> Result<Vec<project::Record>> {
            if all {
                project::records(data)
            } else {
                Ok(project.record(data)?.into_iter().collect())
            }
        };
        if !live {
            let views = sample(b, &records()?, &mut HashMap::new(), &stop)?;
            if stop.load(Ordering::Relaxed) {
                return Ok(0);
            }
            let mut out = std::io::stdout().lock();
            for line in lines(&views, started.elapsed(), Some(Duration::ZERO), None) {
                writeln!(out, "{}", clipped(&line, usize::MAX))?;
            }
            return Ok(0);
        }
        // Apple's JSON stats command samples over two seconds. Keep queries
        // off the render loop so the screen/resize/freshness still update each
        // second, without inventing metrics between real samples.
        std::thread::scope(|scope| {
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            let sampler_stop = stop.clone();
            let worker = scope.spawn(move || {
                let mut previous = HashMap::new();
                while !sampler_stop.load(Ordering::Relaxed) {
                    let tick = Instant::now();
                    let snapshot = records()
                        .and_then(|records| sample(b, &records, &mut previous, &sampler_stop));
                    let _ = sender.try_send((snapshot, Instant::now()));
                    while tick.elapsed() < INTERVAL && !sampler_stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            });
            let result = (|| {
                let mut views = Vec::new();
                let mut warning = Some("Collecting resource/process snapshots...".to_owned());
                let mut sampled = None;
                loop {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let tick = Instant::now();
                    if let Ok((snapshot, time)) = receiver.try_recv() {
                        match snapshot {
                            Ok(next) => {
                                views = next;
                                warning = None;
                                sampled = Some(time);
                            }
                            Err(error) => warning = Some(error.to_string()),
                        }
                    }
                    draw(&lines(
                        &views,
                        started.elapsed(),
                        sampled.map(|time: Instant| time.elapsed()),
                        warning.as_deref(),
                    ))?;
                    while tick.elapsed() < INTERVAL && !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
                Ok(0)
            })();
            stop.store(true, Ordering::Relaxed);
            let _ = worker.join();
            result
        })
    })();
    handle.close();
    let _ = thread.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_uses_counter_delta_and_handles_restart() {
        let time = Instant::now();
        assert_eq!(
            cpu_usage(Some(&(1_000_000, time)), 1_500_000, time + INTERVAL),
            Some(50.0)
        );
        assert_eq!(
            cpu_usage(Some(&(1_000_000, time)), 1, time + INTERVAL),
            None
        );
        assert_eq!(cpu_usage(None, 1, time), None);
    }
    #[test]
    fn processes_exclude_root_and_render_tree_without_argv() {
        let p = processes(
            b"1 0 0 60 0.0 0.1 jail-guest\n10 0 501 30 1.5 3.0 codex\n11 10 501 2 5.0 0.2 git\n",
        );
        assert_eq!(p.len(), 2);
        let view = View {
            id: "jail-test".into(),
            state: "running".into(),
            processes: p,
            memory: Some(1024 * 1024),
            limit: Some(2 * 1024 * 1024),
            ..View::default()
        };
        let lines = lines(&[view], Duration::ZERO, Some(Duration::ZERO), None);
        assert!(lines.iter().any(|s| s.contains("50.0%")));
        assert!(lines.iter().any(|s| s.ends_with("  git")));
        assert!(!lines.iter().any(|s| s.contains("jail-guest")));
    }
    #[test]
    fn unsafe_terminal_text_is_neutralized_and_wide_paths_fit() {
        assert!(!clipped("x\x1b[2J\nsecret", 100).contains(['\x1b', '\n']));
        assert_eq!(clipped("日本語abc", 7), "日本語a");
        assert_eq!(clipped("hello", 0), "");
    }
}
