//! Runs each design several times and prints the median memory of Pinfold's own processes.

use std::process::{Child, Command};
use std::time::Duration;
use std::{env, fs, process, thread};

use spike_process_split::footprint;

const RUNS: usize = 5;
const SETTLE: Duration = Duration::from_secs(5);

fn spawn(name: &str, args: &[&str]) -> Child {
    let dir = env::current_exe().expect("exe");
    Command::new(dir.with_file_name(name)).args(args).spawn().expect("spawn")
}

fn stop(mut children: Vec<Child>) {
    for child in &mut children {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn spread(values: &[f64]) -> f64 {
    let max = values.iter().copied().fold(f64::MIN, f64::max);
    let min = values.iter().copied().fold(f64::MAX, f64::min);
    max - min
}

/// Returns the footprint in MB of each child after settling.
fn sample(children: &[Child]) -> Vec<f64> {
    thread::sleep(SETTLE);
    children.iter().map(|child| footprint(child.id() as i32) as f64 / 1e6).collect()
}

fn main() {
    let dir = env::temp_dir().join(format!("pinfold-split-{}", process::id()));
    fs::create_dir_all(&dir).expect("dir");
    let socket = dir.join("core.sock");
    let socket = socket.to_str().expect("path");
    println!("# Platform: {} {}\n", env::consts::OS, env::consts::ARCH);
    println!("Median of {RUNS} runs after {} s idle, in MB (10^6 bytes). 80x24 panes, no scrollback, no fonts.\n", SETTLE.as_secs());
    println!("| Panes | Single | Core | Window | Core + window | Overhead | Core alone, no window | Run spread |");
    println!("| --- | --- | --- | --- | --- | --- | --- | --- |");
    let mut passed = true;
    for panes in [1, 8] {
        let count = panes.to_string();
        let (mut single, mut core, mut window, mut alone) = (vec![], vec![], vec![], vec![]);
        for _ in 0..RUNS {
            let children = vec![spawn("single", &[&count, socket])];
            let values = sample(&children);
            single.push(values[0]);
            stop(children);

            let children = vec![spawn("core", &[&count, socket])];
            thread::sleep(Duration::from_millis(300));
            alone.push(sample(&children)[0]);
            stop(children);

            let children = vec![spawn("core", &[&count, socket]), spawn("window", &[&count, socket])];
            let values = sample(&children);
            core.push(values[0]);
            window.push(values[1]);
            stop(children);
        }
        let split: Vec<f64> = core.iter().zip(&window).map(|(a, b)| a + b).collect();
        let (one, two) = (median(single.clone()), median(split.clone()));
        let overhead = two - one;
        passed &= overhead <= 2.0;
        println!(
            "| {panes} | {one:.2} | {:.2} | {:.2} | {two:.2} | {overhead:+.2} | {:.2} | {:.2} |",
            median(core), median(window), median(alone), spread(&single).max(spread(&split)),
        );
    }
    println!("\nOverhead within 2 MB at both pane counts: {passed}");
    fs::remove_dir_all(dir).ok();
}
