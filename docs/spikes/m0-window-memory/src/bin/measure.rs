//! Runs each window case three times and prints the median memory.

use std::process::Command;
use std::time::{Duration, Instant};
use std::{env, fs, process, thread};

const RUNS: usize = 3;
const SETTLE: Duration = Duration::from_secs(3);

#[cfg(target_os = "linux")]
fn footprint(pid: i32) -> f64 {
    let text = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).unwrap_or_default();
    let line = text.lines().find(|line| line.starts_with("Pss:")).unwrap_or("Pss: 0 kB");
    line.split_whitespace().nth(1).and_then(|kb| kb.parse::<f64>().ok()).unwrap_or(0.0) * 1024.0 / 1e6
}

#[cfg(target_os = "macos")]
fn footprint(pid: i32) -> f64 {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, (&raw mut info).cast()) };
    if result == 0 { info.ri_phys_footprint as f64 / 1e6 } else { 0.0 }
}

#[cfg(target_os = "macos")]
const MODES: [&str; 4] = ["base", "cpu", "cpu-relief", "metal"];
#[cfg(target_os = "macos")]
const SIZES: [(&str, u32, u32); 2] = [("half", 1512, 1890), ("full", 3024, 1890)];
#[cfg(target_os = "linux")]
const MODES: [&str; 3] = ["base", "cpu", "cpu-relief"];
#[cfg(target_os = "linux")]
const SIZES: [(&str, u32, u32); 2] = [("half", 960, 1080), ("full", 1920, 1080)];

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// One run: returns client memory, compositor growth and the size actually drawn.
fn run(mode: &str, width: u32, height: u32, compositor: Option<i32>) -> (f64, f64, String) {
    let size_file = env::temp_dir().join(format!("pinfold-window-{}", process::id()));
    let _ = fs::remove_file(&size_file);
    let before = compositor.map_or(0.0, footprint);
    let exe = env::current_exe().expect("exe").with_file_name("spike-window-memory");
    let mut child = Command::new(exe)
        .args([mode, &width.to_string(), &height.to_string(), size_file.to_str().expect("path")])
        .spawn()
        .expect("spawn");
    let start = Instant::now();
    while !size_file.exists() {
        assert!(start.elapsed() < Duration::from_secs(30), "{mode} never finished drawing");
        thread::sleep(Duration::from_millis(50));
    }
    thread::sleep(SETTLE);
    let client = footprint(child.id() as i32);
    let server = compositor.map_or(0.0, footprint) - before;
    let drawn = fs::read_to_string(&size_file).unwrap_or_default();
    let _ = child.kill();
    let _ = child.wait();
    thread::sleep(Duration::from_millis(300));
    (client, server, drawn)
}

fn main() {
    let session = env::var("SESSION").unwrap_or_else(|_| "macOS Retina".to_owned());
    let compositor: Option<i32> = env::var("COMPOSITOR_PID").ok().and_then(|pid| pid.parse().ok());
    println!("# {session}, {} {}\n", env::consts::OS, env::consts::ARCH);
    println!("Median of {RUNS} runs after 60 full-window frames and {} s idle, in MB (10^6 bytes). Budget is ADR-0008's 6 MB + 2 x 4 B x pixels.\n", SETTLE.as_secs());
    println!("| Size | Mode | Drawn | Client | Compositor growth | Total | Budget |");
    println!("| --- | --- | --- | --- | --- | --- | --- |");
    for (label, width, height) in SIZES {
        for mode in MODES {
            let results: Vec<_> = (0..RUNS).map(|_| run(mode, width, height, compositor)).collect();
            let drawn = results[0].2.clone();
            let client = median(results.iter().map(|r| r.0).collect());
            let server = median(results.iter().map(|r| r.1).collect());
            let pixels: f64 = drawn.split('x').filter_map(|v| v.parse::<f64>().ok()).product();
            let budget = 6.0 + 2.0 * 4.0 * pixels / 1e6;
            println!("| {label} | {mode} | {drawn} | {client:.2} | {server:+.2} | {:.2} | {budget:.2} |", client + server);
        }
    }
}
