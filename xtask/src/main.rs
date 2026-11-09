//! Project automation, run as `cargo xtask <command>`.

#![allow(clippy::print_stdout, reason = "command-line output")]

use std::path::Path;
use std::process::{Command, ExitCode};
use std::{fs, io};

mod mem_gate;
mod soak;

use mem_gate::Side;

const HELP: &str = "\
Usage: cargo xtask <command>

Commands:
  mem-gate     Check every ADR-0021 memory budget
  soak         Run a long session and check memory, API round trips and lost panes
               [--minutes <n>, default 60]
  fuzz-smoke   Run each fuzz target briefly
  latency      Measure keystroke-to-echo latency through the core
  schema-gen   Generate API JSON Schemas (arrives in M3)
  release      Prepare a release (arrives in M5)
  help         Show this message
";

const FUZZ_SMOKE_SECONDS: u32 = 30;
/// Pinned here and in the workflows, so a broken nightly cannot fail every PR.
const FUZZ_TOOLCHAIN: &str = "+nightly-2026-10-15";
/// Runs as the window's shell, from the workspace root like the other paths here.
const HEAVY_SCREEN: &str = "tests/window/heavy-screen.sh";
/// Pages through the heavy screen's text, so the window's glyph cache fills.
const FULL_CACHE: &str = "tests/window/full-cache.sh";

#[derive(Debug, PartialEq)]
enum Task {
    Help,
    MemGate,
    Soak,
    FuzzSmoke,
    Latency,
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
        Some("soak") => Task::Soak,
        Some("fuzz-smoke") => Task::FuzzSmoke,
        Some("latency") => Task::Latency,
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
        // Naming a seed folder replaces cargo-fuzz's default corpus, so name that first.
        let corpus = format!("fuzz/corpus/{target}");
        let seeds = format!("fuzz/seeds/{target}");
        let mut corpora = Vec::new();
        if Path::new(&seeds).is_dir() {
            if let Err(error) = fs::create_dir_all(&corpus) {
                eprintln!("fuzz-smoke: cannot create {corpus}: {error}");
                return ExitCode::FAILURE;
            }
            corpora = vec![corpus, seeds];
        }
        let status = Command::new("cargo")
            .args([FUZZ_TOOLCHAIN, "fuzz", "run", &target])
            .args(&corpora)
            .arg("--")
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

/// The core starts the window and the tools beside it, and the two profiles build into separate
/// directories.
fn beside_the_core(target: &Path, binary: &str) -> io::Result<()> {
    fs::copy(
        target.join("release").join(binary),
        target.join("release-core").join(binary),
    )?;
    Ok(())
}

/// Runs git in `repo` with no hooks and no signing, as an author the gate names.
fn git(repo: &Path, args: &[&str]) -> io::Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=gate", "-c", "user.email=gate@localhost"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("git {} failed", args.join(" "))))
    }
}

/// Fills the tools' caps in `dir`, outside any repository so the file manager runs no git:
/// `reader.md`, `prose.md` and `defs.md` of exactly 5 MB, an `empty` directory, `entries` of
/// 10,000 files, a `repo` whose work tree's diff is over 8 MB, and `tool-screen.sh`, a window's
/// shell that runs the reader.
fn write_tool_inputs(dir: &Path, tools: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir.join("empty"))?;
    fs::create_dir_all(dir.join("entries"))?;
    fs::create_dir_all(dir.join("repo"))?;
    // `layout_time_at_the_byte_cap` in `md.rs` lays out the same two files.
    let markdown = |section: usize| {
        format!(
            "## Section {section}\n\n見出し 日本語 中文 한국어\n\nSome *emphasis*, **bold**, `code` \
             and a [link](https://pinfold.sh/{section}).\n\n- one\n- two\n\n\
             | a | b |\n| --- | --- |\n| 1 | 2 |\n\n```\nfn main() {{}}\n```\n\n"
        )
    };
    let prose = |paragraph: usize| {
        format!(
            "Paragraph {paragraph} of plain prose, with words enough to wrap across two lines at \
             eighty columns and no markup at all.\n\n"
        )
    };
    let defs = |definition: usize| format!("[a{definition}]: https://pinfold.sh/{definition}\n");
    let parts: [(&str, &dyn Fn(usize) -> String); 3] = [
        ("reader.md", &markdown),
        ("prose.md", &prose),
        ("defs.md", &defs),
    ];
    for (name, part) in parts {
        let mut bytes = Vec::new();
        for at in 0.. {
            if bytes.len() as u64 >= mem_gate::MARKDOWN_CAP {
                break;
            }
            bytes.extend(part(at).bytes());
        }
        // Cut at the cap, which may split a character; the reader shows that as hex.
        bytes.truncate(mem_gate::MARKDOWN_CAP as usize);
        fs::write(dir.join(name), bytes)?;
    }
    for entry in 0..mem_gate::ENTRY_CAP {
        fs::write(dir.join(format!("entries/{entry:05}.txt")), "")?;
    }
    let repo = dir.join("repo");
    let lines = |side: &str| -> String {
        (0..100_000)
            .map(|n| format!("{side} line {n:06}, long enough for an 8 MB diff\n"))
            .collect()
    };
    git(&repo, &["init", "-q"])?;
    fs::write(repo.join("a.txt"), lines("old"))?;
    git(&repo, &["add", "a.txt"])?;
    git(&repo, &["commit", "-q", "-m", "base"])?;
    fs::write(repo.join("a.txt"), lines("new"))?;
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', r"'\''"));
    let script = dir.join("tool-screen.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nexec {} md -- {}\n",
            quote(tools),
            quote(&dir.join("reader.md"))
        ),
    )?;
    fs::set_permissions(script, fs::Permissions::from_mode(0o755))
}

fn mem_gate_task() -> ExitCode {
    let target = std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into());
    let corpora = Path::new(&target).join("corpora");
    let inputs = std::env::temp_dir().join(format!("pinfold-tool-gate-{}", std::process::id()));
    if let Err(error) = build("release-core", "pinfold-core", &["--no-default-features"])
        .and_then(|()| build("release", "pinfold-window", &[]))
        .and_then(|()| build("release", "pinfold", &[]))
        .and_then(|()| beside_the_core(Path::new(&target), "pinfold-window"))
        .and_then(|()| beside_the_core(Path::new(&target), "pinfold"))
        .and_then(|()| write_corpora(&corpora))
        .and_then(|()| {
            let tools = fs::canonicalize(Path::new(&target).join("release-core/pinfold"))?;
            write_tool_inputs(&inputs, &tools)
        })
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
    let Some(inputs_text) = inputs.to_str() else {
        eprintln!("mem-gate: the temp directory is not valid UTF-8");
        return ExitCode::FAILURE;
    };
    let input = |name: &str| format!("{inputs_text}/{name}");
    let (markdown, prose, defs, empty, entries, repo) = (
        input("reader.md"),
        input("prose.md"),
        input("defs.md"),
        input("empty"),
        input("entries"),
        input("repo"),
    );
    let tool_screen = inputs.join("tool-screen.sh");
    let replay_typical = ["--replay", typical.as_str()];
    let replay_hostile = ["--replay", hostile.as_str()];
    let replay_baseline: &[&str] = &["--replay", "/dev/null"];
    let core = Path::new(&target).join("release-core/pinfold-core");
    let mut budgets = vec![
        mem_gate::Budget {
            metric: "Core, 8 idle panes",
            limit: mem_gate::CORE_LIMIT,
            program: &core,
            args: &["--idle", "8"],
            per_pane_over: None,
            settle: mem_gate::SETTLE,
            wait_for_line: false,
            screen: None,
            window: None,
            menu: false,
        },
        mem_gate::Budget {
            metric: "10k lines of typical text",
            limit: mem_gate::TYPICAL_TEXT_LIMIT,
            program: &core,
            args: &replay_typical,
            per_pane_over: Some((replay_baseline, 1)),
            settle: mem_gate::SETTLE,
            wait_for_line: true,
            screen: None,
            window: None,
            menu: false,
        },
        mem_gate::Budget {
            metric: "Scrollback at its cap",
            limit: mem_gate::SCROLLBACK_CAP,
            program: &core,
            args: &replay_hostile,
            per_pane_over: Some((replay_baseline, 1)),
            settle: mem_gate::SETTLE,
            wait_for_line: true,
            screen: None,
            window: None,
            menu: false,
        },
        mem_gate::Budget {
            metric: "Each idle pane",
            limit: mem_gate::PANE_LIMIT,
            program: &core,
            args: &["--idle", "8"],
            per_pane_over: Some((&["--idle", "1"], 7)),
            settle: mem_gate::SETTLE,
            wait_for_line: false,
            screen: None,
            window: None,
            menu: false,
        },
    ];
    // ADR-0045 section 5. Each tool runs in a pane of a core, which prints the tool's pid.
    let idle_tool = ["--tool", "1", "files", empty.as_str()];
    let reader = ["--tool", "1", "md", markdown.as_str()];
    let prose_reader = ["--tool", "1", "md", prose.as_str()];
    let defs_reader = ["--tool", "1", "md", defs.as_str()];
    let file_manager = ["--tool", "1", "files", entries.as_str()];
    let diff_viewer = ["--tool", "1", "diff", repo.as_str()];
    let readers = ["--tool", "8", "md", markdown.as_str()];
    let closed = ["--tool-closed", "md", markdown.as_str()];
    let tool = mem_gate::Budget {
        metric: "A tool process, nothing open (files of an empty directory)",
        limit: mem_gate::TOOL_LIMIT,
        program: &core,
        args: &idle_tool,
        per_pane_over: None,
        settle: mem_gate::TOOL_SETTLE,
        wait_for_line: true,
        screen: None,
        window: Some(Side::Window),
        menu: false,
    };
    budgets.extend([
        mem_gate::Budget { ..tool },
        mem_gate::Budget {
            metric: "Markdown reader, a 5 MB file",
            limit: mem_gate::READER_LIMIT,
            args: &reader,
            ..tool
        },
        mem_gate::Budget {
            metric: "Markdown reader, a 5 MB file of plain prose",
            limit: mem_gate::READER_LIMIT,
            args: &prose_reader,
            ..tool
        },
        mem_gate::Budget {
            metric: "Markdown reader, a 5 MB file of link definitions",
            limit: mem_gate::READER_LIMIT,
            args: &defs_reader,
            ..tool
        },
        mem_gate::Budget {
            metric: "File manager, 10,000 entries",
            limit: mem_gate::FILES_LIMIT,
            args: &file_manager,
            ..tool
        },
        mem_gate::Budget {
            metric: "Diff viewer, git output cut at 8 MB",
            limit: mem_gate::DIFF_LIMIT,
            args: &diff_viewer,
            ..tool
        },
        mem_gate::Budget {
            metric: "Each tool pane in the core (markdown reader)",
            limit: mem_gate::PANE_LIMIT,
            args: &readers,
            per_pane_over: Some((&reader, 7)),
            window: Some(Side::Core),
            ..tool
        },
        mem_gate::Budget {
            metric: "Core 2 s after a tool pane closes, its tool gone",
            limit: mem_gate::CORE_LIMIT,
            args: &closed,
            settle: mem_gate::CLOSED_SETTLE,
            window: Some(Side::Closed),
            ..tool
        },
    ]);
    if cfg!(target_os = "macos") {
        budgets.push(mem_gate::Budget {
            metric: "Window on macOS, 1512x1890",
            limit: mem_gate::WINDOW_LIMIT_HALF_RETINA,
            program: &core,
            args: &["--window", "1", "1512x1890"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: None,
            window: Some(Side::Window),
            menu: false,
        });
        budgets.push(mem_gate::Budget {
            metric: "Window on macOS with text, 1512x1890",
            limit: mem_gate::WINDOW_LIMIT_HALF_RETINA,
            program: &core,
            args: &["--window", "1", "1512x1890"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: Some(Path::new(HEAVY_SCREEN)),
            window: Some(Side::Window),
            menu: false,
        });
        budgets.push(mem_gate::Budget {
            metric: "Window on macOS with a full glyph cache, 1512x1890",
            limit: mem_gate::WINDOW_LIMIT_HALF_RETINA,
            program: &core,
            args: &["--window", "1", "1512x1890"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: Some(Path::new(FULL_CACHE)),
            window: Some(Side::Window),
            menu: false,
        });
        budgets.push(mem_gate::Budget {
            metric: "Window on macOS with 8 tabs, 1512x1890",
            limit: mem_gate::WINDOW_LIMIT_HALF_RETINA,
            program: &core,
            args: &["--window", "8", "1512x1890"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: None,
            window: Some(Side::Window),
            menu: false,
        });
        budgets.push(mem_gate::Budget {
            metric: "Window on macOS with its context menu open, 1512x1890",
            limit: mem_gate::WINDOW_LIMIT_HALF_RETINA,
            program: &core,
            args: &["--window", "1", "1512x1890"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: Some(Path::new(HEAVY_SCREEN)),
            window: Some(Side::Window),
            menu: true,
        });
        budgets.push(mem_gate::Budget {
            metric: "Window on macOS with the markdown reader, 1512x1890",
            limit: mem_gate::WINDOW_LIMIT_HALF_RETINA,
            program: &core,
            args: &["--window", "1", "1512x1890"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: Some(&tool_screen),
            window: Some(Side::Window),
            menu: false,
        });
        budgets.push(mem_gate::Budget {
            metric: "Core beside that window, 8 idle tabs",
            limit: mem_gate::CORE_LIMIT,
            program: &core,
            args: &["--window", "8", "1512x1890"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: None,
            window: Some(Side::Core),
            menu: false,
        });
    }
    if cfg!(target_os = "linux") {
        budgets.push(mem_gate::Budget {
            metric: "Window on X11, 1920x1080",
            limit: mem_gate::WINDOW_LIMIT_FULL_HD,
            program: &core,
            args: &["--window", "1", "1920x1080"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: None,
            window: Some(Side::Window),
            menu: false,
        });
        budgets.push(mem_gate::Budget {
            metric: "Window on X11 with text, 1920x1080",
            limit: mem_gate::WINDOW_LIMIT_FULL_HD,
            program: &core,
            args: &["--window", "1", "1920x1080"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: Some(Path::new(HEAVY_SCREEN)),
            window: Some(Side::Window),
            menu: false,
        });
        budgets.push(mem_gate::Budget {
            metric: "Window on X11 with a full glyph cache, 1920x1080",
            limit: mem_gate::WINDOW_LIMIT_FULL_HD,
            program: &core,
            args: &["--window", "1", "1920x1080"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: Some(Path::new(FULL_CACHE)),
            window: Some(Side::Window),
            menu: false,
        });
        budgets.push(mem_gate::Budget {
            metric: "Window on X11 with 8 tabs, 1920x1080",
            limit: mem_gate::WINDOW_LIMIT_FULL_HD,
            program: &core,
            args: &["--window", "8", "1920x1080"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: None,
            window: Some(Side::Window),
            menu: false,
        });
        budgets.push(mem_gate::Budget {
            metric: "Window on X11 with its context menu open, 1920x1080",
            limit: mem_gate::WINDOW_LIMIT_FULL_HD,
            program: &core,
            args: &["--window", "1", "1920x1080"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: Some(Path::new(HEAVY_SCREEN)),
            window: Some(Side::Window),
            menu: true,
        });
        budgets.push(mem_gate::Budget {
            metric: "Window on X11 with the markdown reader, 1920x1080",
            limit: mem_gate::WINDOW_LIMIT_FULL_HD,
            program: &core,
            args: &["--window", "1", "1920x1080"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: Some(&tool_screen),
            window: Some(Side::Window),
            menu: false,
        });
        budgets.push(mem_gate::Budget {
            metric: "Core beside that window, 8 idle tabs",
            limit: mem_gate::CORE_LIMIT,
            program: &core,
            args: &["--window", "8", "1920x1080"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: None,
            window: Some(Side::Core),
            menu: false,
        });
    }
    let passed = mem_gate::run(&budgets);
    let _ = fs::remove_dir_all(&inputs);
    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// `--minutes <n>` after `soak`, or the hour #224 asks for.
fn soak_minutes(args: &[String]) -> Option<u64> {
    match args {
        [] => Some(60),
        [flag, minutes] if flag == "--minutes" => minutes.parse().ok().filter(|&m| m > 0),
        _ => None,
    }
}

fn soak_task(args: &[String]) -> ExitCode {
    let Some(minutes) = soak_minutes(args) else {
        eprintln!("usage: cargo xtask soak [--minutes <n>]");
        return ExitCode::FAILURE;
    };
    let target = std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into());
    let passed = build("release-core", "pinfold-core", &["--no-default-features"])
        .and_then(|()| build("release", "pinfold-window", &[]))
        .and_then(|()| beside_the_core(Path::new(&target), "pinfold-window"))
        .and_then(|()| {
            soak::run(
                &Path::new(&target).join("release-core/pinfold-core"),
                minutes,
            )
        });
    match passed {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("soak: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Fsync delay of the slow-storage case, standing in for a slow disk.
const SLOW_FSYNC_MS: &str = "100";

/// Runs `benches/echo.rs` at rest, with every sync of the core delayed, and under API traffic,
/// as a table of milliseconds against the budget in `docs/design/architecture.md`.
fn latency_task() -> ExitCode {
    let target = std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into());
    for binary in ["pinfold-core", "pinfold"] {
        if let Err(error) = build("release-core", binary, &[]) {
            eprintln!("latency: {error}");
            return ExitCode::FAILURE;
        }
    }
    let core = Path::new(&target).join("release-core/pinfold-core");
    let pinfold = Path::new(&target).join("release-core/pinfold");
    let pinfold = pinfold.to_string_lossy();
    // `session.save` every 5 s on slow storage, over the 60 s #250 measured it in.
    let session_save = [
        "--fsync-ms",
        SLOW_FSYNC_MS,
        "--session-save",
        &pinfold,
        "--seconds",
        "60",
    ];
    println!(
        "| fsync delay ms | API traffic | session.save calls | samples | p50 ms | p99 ms | max ms | syncs | API reads/s | budget |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    let mut met = true;
    for case in [
        &[][..],
        &["--fsync-ms", SLOW_FSYNC_MS],
        &["--api"],
        &session_save,
    ] {
        let status = Command::new("cargo")
            .args(["run", "--locked", "--release", "--quiet"])
            .args([
                "--package",
                "pinfold-benches",
                "--bin",
                "echo",
                "--",
                "--core",
            ])
            .arg(&core)
            .args(case)
            .status();
        met &= status.is_ok_and(|status| status.success());
    }
    if met {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn main() -> ExitCode {
    // A soak core runs this as its panes' shell.
    if let Some(directory) = std::env::var_os(soak::DIRECTORY) {
        eprintln!("soak: {}", soak::shell(directory));
        return ExitCode::FAILURE;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = args.first().cloned();
    match parse(arg.as_deref()) {
        Task::Help => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Task::MemGate => mem_gate_task(),
        Task::Soak => soak_task(&args[1..]),
        Task::FuzzSmoke => fuzz_smoke(),
        Task::Latency => latency_task(),
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
    fn latency_is_a_command() {
        assert_eq!(parse(Some("latency")), Task::Latency);
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
    fn soak_takes_minutes_or_an_hour() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(soak_minutes(&args(&[])), Some(60));
        assert_eq!(soak_minutes(&args(&["--minutes", "10"])), Some(10));
        assert_eq!(soak_minutes(&args(&["--minutes", "0"])), None);
        assert_eq!(soak_minutes(&args(&["10"])), None);
    }

    #[test]
    fn missing_fuzz_directory_has_no_targets() {
        assert!(fuzz_targets(Path::new("does/not/exist")).is_empty());
    }
}
