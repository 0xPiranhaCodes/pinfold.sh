//! Project automation, run as `cargo xtask <command>`.

#![allow(clippy::print_stdout, reason = "command-line output")]

use std::path::Path;
use std::process::{Command, ExitCode};
use std::{fs, io};

mod mem_gate;

const HELP: &str = "\
Usage: cargo xtask <command>

Commands:
  mem-gate     Check every ADR-0021 memory budget (rest arrive in M1)
  fuzz-smoke   Run each fuzz target briefly
  schema-gen   Generate API JSON Schemas (arrives in M3)
  release      Prepare a release (arrives in M5)
  help         Show this message
";

const FUZZ_SMOKE_SECONDS: u32 = 30;

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
    for target in targets {
        let time = format!("-max_total_time={FUZZ_SMOKE_SECONDS}");
        let status = Command::new("cargo")
            .args(["+nightly", "fuzz", "run", &target, "--", &time])
            .status();
        if !status.is_ok_and(|status| status.success()) {
            eprintln!("fuzz-smoke: {target} failed");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

fn build_core() -> io::Result<()> {
    let status = Command::new("cargo")
        .args(["build", "--locked", "--profile", "release-core"])
        .args(["--package", "pinfold-cli", "--bin", "pinfold-core"])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("building pinfold-core failed"))
    }
}

fn mem_gate_task() -> ExitCode {
    if let Err(error) = build_core() {
        eprintln!("mem-gate: {error}");
        return ExitCode::FAILURE;
    }
    let target = std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into());
    let core = Path::new(&target).join("release-core/pinfold-core");
    let budgets = [
        mem_gate::Budget {
            metric: "Core, 8 idle panes",
            limit: mem_gate::CORE_LIMIT,
            program: &core,
            args: &["--idle", "8"],
            per_pane_over: None,
        },
        mem_gate::Budget {
            metric: "Each idle pane",
            limit: mem_gate::PANE_LIMIT,
            program: &core,
            args: &["--idle", "8"],
            per_pane_over: Some((&["--idle", "1"], 7)),
        },
    ];
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
