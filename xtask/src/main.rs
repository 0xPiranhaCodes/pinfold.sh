//! Project automation, run as `cargo xtask <command>`.

#![allow(clippy::print_stdout, reason = "command-line output")]

use std::path::Path;
use std::process::{Command, ExitCode};
use std::{fs, io};

mod mem_gate;

const HELP: &str = "\
Usage: cargo xtask <command>

Commands:
  mem-gate     Check every ADR-0021 memory budget
  fuzz-smoke   Run each fuzz target briefly
  schema-gen   Generate API JSON Schemas (arrives in M3)
  release      Prepare a release (arrives in M5)
  help         Show this message
";

const FUZZ_SMOKE_SECONDS: u32 = 30;
/// Pinned here and in the workflows, so a broken nightly cannot fail every PR.
const FUZZ_TOOLCHAIN: &str = "+nightly-2026-10-15";

#[derive(Debug, PartialEq)]
enum Task {
    Help,
    MemGate,
    FuzzSmoke,
    NotYet {
        name: String,
        milestone: &'static str,
    },
    Unknown(String),
}

fn parse(arg: Option<&str>) -> Task {
    match arg {
        None | Some("help" | "--help" | "-h") => Task::Help,
        Some("mem-gate") => Task::MemGate,
        Some("fuzz-smoke") => Task::FuzzSmoke,
        Some(name @ "schema-gen") => Task::NotYet {
            name: name.into(),
            milestone: "M3",
        },
        Some(name @ "release") => Task::NotYet {
            name: name.into(),
            milestone: "M5",
        },
        Some(other) => Task::Unknown(other.into()),
    }
}

/// Names of the `.rs` files in `dir`, sorted. A missing directory has no targets.
fn fuzz_targets(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .filter_map(|path| Some(path.file_stem()?.to_str()?.to_owned()))
        .collect();
    names.sort();
    names
}

fn fuzz_smoke() -> ExitCode {
    let targets = fuzz_targets(Path::new("fuzz/fuzz_targets"));
    if targets.is_empty() {
        println!("fuzz-smoke: SKIPPED, no fuzz targets exist yet");
    }
    let seconds = std::env::var("FUZZ_SECONDS").unwrap_or_else(|_| FUZZ_SMOKE_SECONDS.to_string());
    for target in targets {
        let time = format!("-max_total_time={seconds}");
        let status = Command::new("cargo")
            .args([FUZZ_TOOLCHAIN, "fuzz", "run", &target, "--"])
            .args(["-dict=fuzz/vt.dict", "-timeout=10", &time])
            .status();
        if !status.is_ok_and(|status| status.success()) {
            eprintln!("fuzz-smoke: {target} failed");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

fn build(profile: &str, binary: &str, features: &[&str]) -> io::Result<()> {
    let status = Command::new("cargo")
        .args(["build", "--locked", "--profile", profile])
        .args(features)
        .args(["--package", "pinfold-cli", "--bin", binary])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("building {binary} failed")))
    }
}

/// Saves the scrollback bench's corpora in `dir`.
fn write_corpora(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let status = Command::new("cargo")
        .args(["run", "--locked", "--release"])
        .args(["--package", "pinfold-benches", "--bin", "scrollback"])
        .arg("--")
        .arg("--write")
        .arg(dir)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("writing the corpora failed"))
    }
}

fn mem_gate_task() -> ExitCode {
    let target = std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into());
    let corpora = Path::new(&target).join("corpora");
    if let Err(error) = build("release-core", "pinfold-core", &["--no-default-features"])
        .and_then(|()| build("release", "pinfold", &[]))
        .and_then(|()| write_corpora(&corpora))
    {
        eprintln!("mem-gate: {error}");
        return ExitCode::FAILURE;
    }
    let (Some(typical), Some(hostile)) = (
        corpora.join("plain-log").to_str().map(str::to_owned),
        corpora.join("truecolor").to_str().map(str::to_owned),
    ) else {
        eprintln!("mem-gate: the target directory is not valid UTF-8");
        return ExitCode::FAILURE;
    };
    let replay_typical = ["--replay", typical.as_str()];
    let replay_hostile = ["--replay", hostile.as_str()];
    let replay_baseline: &[&str] = &["--replay", "/dev/null"];
    let core = Path::new(&target).join("release-core/pinfold-core");
    let window = Path::new(&target).join("release/pinfold");
    let mut budgets = vec![
        mem_gate::Budget {
            metric: "Core, 8 idle panes",
            limit: mem_gate::CORE_LIMIT,
            program: &core,
            args: &["--idle", "8"],
            per_pane_over: None,
            settle: mem_gate::SETTLE,
            wait_for_line: false,
        },
        mem_gate::Budget {
            metric: "10k lines of typical text",
            limit: mem_gate::TYPICAL_TEXT_LIMIT,
            program: &core,
            args: &replay_typical,
            per_pane_over: Some((replay_baseline, 1)),
            settle: mem_gate::SETTLE,
            wait_for_line: true,
        },
        mem_gate::Budget {
            metric: "Scrollback at its cap",
            limit: mem_gate::SCROLLBACK_CAP,
            program: &core,
            args: &replay_hostile,
            per_pane_over: Some((replay_baseline, 1)),
            settle: mem_gate::SETTLE,
            wait_for_line: true,
        },
        mem_gate::Budget {
            metric: "Each idle pane",
            limit: mem_gate::PANE_LIMIT,
            program: &core,
            args: &["--idle", "8"],
            per_pane_over: Some((&["--idle", "1"], 7)),
            settle: mem_gate::SETTLE,
            wait_for_line: false,
        },
    ];
    if cfg!(target_os = "macos") {
        budgets.push(mem_gate::Budget {
            metric: "Window on macOS, 1512x1890",
            limit: mem_gate::WINDOW_LIMIT_HALF_RETINA,
            program: &window,
            args: &["--size", "1512x1890"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: false,
        });
    }
    if cfg!(target_os = "linux") {
        budgets.push(mem_gate::Budget {
            metric: "Window on X11, 1920x1080",
            limit: mem_gate::WINDOW_LIMIT_FULL_HD,
            program: &window,
            args: &["--size", "1920x1080"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: false,
        });
    }
    if mem_gate::run(&budgets) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn main() -> ExitCode {
    let arg = std::env::args().nth(1);
    match parse(arg.as_deref()) {
        Task::Help => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Task::MemGate => mem_gate_task(),
        Task::FuzzSmoke => fuzz_smoke(),
        Task::NotYet { name, milestone } => {
            eprintln!("{name}: not implemented until {milestone}");
            ExitCode::FAILURE
        }
        Task::Unknown(name) => {
            eprintln!("unknown command: {name}\n\n{HELP}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_argument_shows_help() {
        assert_eq!(parse(None), Task::Help);
    }

    #[test]
    fn unknown_commands_are_not_silently_accepted() {
        assert_eq!(parse(Some("mem-gat")), Task::Unknown("mem-gat".into()));
    }

    #[test]
    fn unbuilt_commands_name_their_milestone() {
        assert_eq!(
            parse(Some("release")),
            Task::NotYet {
                name: "release".into(),
                milestone: "M5"
            }
        );
    }

    #[test]
    fn missing_fuzz_directory_has_no_targets() {
        assert!(fuzz_targets(Path::new("does/not/exist")).is_empty());
    }
}
