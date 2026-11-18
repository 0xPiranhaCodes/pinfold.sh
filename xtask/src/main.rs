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
/// 10,000 files, `tree` of 100,000 for the checkpoint take, a `repo` whose work tree's diff is
/// over 8 MB, and `tool-screen.sh`, a window's
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
    // The take rows' tree: 100 directories of 1,000 small files.
    for file in 0..mem_gate::TAKE_FILES {
        let sub = dir.join(format!("tree/{:03}", file / 1000));
        if file % 1000 == 0 {
            fs::create_dir_all(&sub)?;
        }
        fs::write(sub.join(format!("{file:06}.txt")), format!("file {file}\n"))?;
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
    fs::set_permissions(script, fs::Permissions::from_mode(0o755))?;
    // Every pane says its agent needs you, which lists it in the inbox (ADR-0051), then draws
    // the heavy screen from a repository, so an open inbox reads each pane's branch.
    let script = dir.join("agents-screen.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\ncd {} || exit 1\n{} ctl status needs-you 'Allow edit to src/config.rs?'\nexec {}\n",
            quote(&repo),
            quote(tools),
            quote(&fs::canonicalize(HEAVY_SCREEN)?)
        ),
    )?;
    fs::set_permissions(script, fs::Permissions::from_mode(0o755))?;
    // ADR-0051 §11's inbox rows: agents that say they need you from a repository and wait, with
    // the default config, so the core's own cost of reading their branches shows.
    let agents = dir.join("agents");
    fs::create_dir_all(&agents)?;
    let script = agents.join("agent.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\ncd {} || exit 1\n{} ctl status needs-you 'Allow edit to src/config.rs?'\nexec /bin/sleep 86400\n",
            quote(&repo),
            quote(tools)
        ),
    )?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    // ADR-0051 §7's rows: the same agents with `[inbox] agent_files` on, whose home holds a
    // 64 MiB Claude Code session for the repository, which the peek row reads too.
    let files = dir.join("agent-files");
    fs::create_dir_all(files.join("pinfold"))?;
    fs::write(
        files.join("pinfold/pinfold.toml"),
        "[inbox]\nagent_files = true\n",
    )?;
    fs::copy(&script, files.join("agent.sh"))?;
    // ADR-0051 §8's row: the window's agents with `[inbox] agent_files` on, whose home holds 16
    // Claude Code sessions running elsewhere, which each pane touches as it starts so that
    // they changed in the last 10 minutes.
    let outside = dir.join("outside");
    fs::create_dir_all(outside.join("pinfold"))?;
    fs::write(
        outside.join("pinfold/pinfold.toml"),
        "[inbox]\nagent_files = true\n",
    )?;
    for index in 0..16 {
        let folder = outside.join(format!(".claude/projects/-elsewhere-agent-{index}"));
        fs::create_dir_all(&folder)?;
        let line = format!(r#"{{"type":"user","cwd":"/elsewhere/agent-{index}"}}"#);
        fs::write(folder.join("session.jsonl"), line + "\n")?;
    }
    let script = outside.join("agent.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\ntouch {}/.claude/projects/*/session.jsonl\ncd {} || exit 1\n{} ctl status needs-you 'Allow edit to src/config.rs?'\nexec {}\n",
            quote(&outside),
            quote(&repo),
            quote(tools),
            quote(&fs::canonicalize(HEAVY_SCREEN)?)
        ),
    )?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    write_peek_session(&files, &fs::canonicalize(&repo)?)
}

/// Writes, under `home`, a Claude Code session of [`mem_gate::PEEK_SESSION`] bytes for `dir`,
/// for the peek row: long lines of tool calls and their results, as an agent's file grows.
fn write_peek_session(home: &Path, dir: &Path) -> io::Result<()> {
    use std::io::Write;
    let name: String = (dir.as_os_str().to_string_lossy().encode_utf16())
        .map(|unit| match u8::try_from(unit) {
            Ok(byte) if byte.is_ascii_alphanumeric() => char::from(byte),
            _ => '-',
        })
        .collect();
    let folder = home.join(".claude/projects").join(name);
    fs::create_dir_all(&folder)?;
    let mut file = io::BufWriter::new(fs::File::create(folder.join("session.jsonl"))?);
    let call = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}],"usage":{"input_tokens":2,"cache_read_input_tokens":128000}}}"#;
    let result = format!(
        r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","content":"{}"}}]}}}}"#,
        "output line ".repeat(400)
    );
    let mut written = 0;
    while written < mem_gate::PEEK_SESSION {
        for line in [call, result.as_str()] {
            writeln!(file, "{line}")?;
            written += line.len() as u64 + 1;
        }
    }
    file.flush()
}

/// Writes, for each of ADR-0051's webhook rows, a folder holding the config the measured core
/// reads, with `url` as its webhook, and `agent.sh`, every pane's shell, which reports `state`
/// through `tools` and waits. With `one`, only the first pane of each core reports needs you.
fn write_webhook_inputs(dir: &Path, tools: &Path, rows: &[(&str, &str, &str)]) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', r"'\''"));
    for (row, url, state) in rows {
        let folder = dir.join(row);
        fs::create_dir_all(folder.join("pinfold"))?;
        fs::write(
            folder.join("pinfold/pinfold.toml"),
            format!("[alerts]\nwebhook = \"{url}\"\n"),
        )?;
        // The core is each pane's parent, so one pane of each run takes the lock.
        let state = match *state {
            "one" => format!(
                "$(mkdir {}/alerted-$PPID 2>/dev/null && echo needs-you || echo running)",
                quote(&folder)
            ),
            state => state.to_owned(),
        };
        let script = folder.join("agent.sh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n{} ctl status {state} 'Allow edit to src/config.rs?'\nexec /bin/sleep 86400\n",
                quote(tools)
            ),
        )?;
        fs::set_permissions(script, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Writes `on`, `off` and `burst` under `dir` for the run-record rows (ADR-0050): each a config
/// with `[rewind]` on or off and a profile per pane, `quick<n>`, whose agent exits at once, a
/// `state` folder the core keeps its checkpoints in, a scratch `work` folder, and `agent.sh`,
/// every pane's shell. It reports running and launches 8 agents in tabs, in `on` and `off` each
/// once the last one's pane is gone, so both rows have the same churn whether a launch waits
/// for its checkpoint or not, and in `burst` back to back (#476). Then it takes and lists a
/// checkpoint of each run through `pinfold mcp`, the other panes' runs refused, and waits. 8
/// panes make 64 closed runs.
fn write_rewind_inputs(dir: &Path, tools: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', r"'\''"));
    for (row, enabled, paced) in [
        ("on", true, true),
        ("off", false, true),
        ("burst", false, false),
    ] {
        let folder = dir.join(row);
        for sub in ["pinfold", "state", "work"] {
            fs::create_dir_all(folder.join(sub))?;
        }
        fs::write(folder.join("work/a.txt"), "a\n")?;
        let mut config = format!("[rewind]\nenabled = {enabled}\n");
        for pane in 0..8 {
            // Its own program, so the shell can tell its agent's pane by its command.
            let quick = folder.join(format!("quick{pane}.sh"));
            fs::write(&quick, "#!/bin/sh\nexit 0\n")?;
            fs::set_permissions(&quick, fs::Permissions::from_mode(0o755))?;
            config += &format!(
                "[agents.quick{pane}]\ncommand = [{:?}]\n",
                quick.display().to_string()
            );
        }
        fs::write(folder.join("pinfold/pinfold.toml"), config)?;
        let call = |name: &str| {
            format!(
                "printf '{{\"jsonrpc\":\"2.0\",\"id\":%s,\"method\":\"tools/call\",\
                 \"params\":{{\"name\":\"{name}\",\"arguments\":{{\"pane\":%s}}}}}}\\n' $p $p"
            )
        };
        let tools = quote(tools);
        // A launched agent is not in its launcher's reach for pane.wait, so this polls the list
        // until the agent's pane is gone.
        let pace = if paced {
            format!(
                "  id=$({tools} ctl list --json | tr '}}' '\\n' | grep -F \"/quick$me.sh\\\"\" | \
                 sed 's/.*\"id\":\\([0-9]*\\).*/\\1/')\n\
                 \x20 while [ -n \"$id\" ] && {tools} ctl list --json | grep -q \"\\\"id\\\":$id,\"; do sleep 0.1; done\n"
            )
        } else {
            String::new()
        };
        let script = folder.join("agent.sh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ncd {work} || exit 1\n{tools} ctl status running 'checkpoints'\n\
                 me=$({tools} ctl list | grep -o '^[0-9]*\\*' | tr -d '*')\n\
                 i=0; while [ $i -lt 8 ]; do\n\
                 \x20 {tools} agent quick$me --tab 2>/dev/null\n{pace}\
                 \x20 i=$((i + 1))\ndone\n\
                 {{ echo '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{}}}}'\n\
                 p=8; while [ $p -lt 72 ]; do {take}; {list}; p=$((p + 1)); done; }} | {tools} mcp > /dev/null\n\
                 exec /bin/sleep 86400\n",
                work = quote(&folder.join("work")),
                take = call("checkpoint_take_v1"),
                list = call("checkpoint_list_v1"),
            ),
        )?;
        fs::set_permissions(script, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Takes webhook posts on loopback and answers each 204 at once, for the burst row. Returns its
/// port. Std only: it reads a request's headers and the body they announce, and nothing else.
fn answer_posts() -> io::Result<u16> {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let (mut request, mut buffer) = (Vec::new(), [0_u8; 4096]);
            while let Ok(read @ 1..) = stream.read(&mut buffer) {
                request.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&request);
                let Some(end) = text.find("\r\n\r\n") else {
                    continue;
                };
                let length = text[..end]
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
            let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
        }
    });
    Ok(port)
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
            write_tool_inputs(&inputs, &tools)?;
            let burst = format!("http://127.0.0.1:{}/", answer_posts()?);
            write_webhook_inputs(
                &inputs.join("webhook"),
                &tools,
                &[
                    ("idle", "https://127.0.0.1:9", "running"),
                    ("one", "https://127.0.0.1:9", "one"),
                    ("burst", &burst, "needs-you"),
                ],
            )?;
            write_rewind_inputs(&inputs.join("rewind"), &tools)
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
    let agents_screen = inputs.join("agents-screen.sh");
    let listed_agents = inputs.join("agents");
    let agent_files = inputs.join("agent-files");
    let outside = inputs.join("outside");
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
            inbox: None,
            agents: None,
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
            inbox: None,
            agents: None,
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
            inbox: None,
            agents: None,
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
            inbox: None,
            agents: None,
        },
    ];
    // ADR-0051 section 11: 8 listed agent panes and a webhook, measured once the worker thread
    // has ended. A post to port 9 fails at once, so one alert takes three tries over 35 s.
    let (idle_hook, one_hook, burst_hook) = (
        inputs.join("webhook/idle"),
        inputs.join("webhook/one"),
        inputs.join("webhook/burst"),
    );
    let webhook = mem_gate::Budget {
        metric: "Core, 8 agent panes listed, a webhook configured, idle",
        limit: mem_gate::CORE_LIMIT,
        program: &core,
        args: &["--idle", "8"],
        per_pane_over: None,
        settle: mem_gate::TOOL_SETTLE,
        wait_for_line: false,
        screen: None,
        window: None,
        menu: false,
        inbox: None,
        agents: Some(&idle_hook),
    };
    // ADR-0050 section 7 and slice 5: 64 run records over 8 agent panes, and no checkpoint list
    // kept, under #476's ceiling. The same launches with `[rewind]` off, paced and back to back,
    // are shown, not gated: off minus on was -0.85 and -0.90 MB in CI, too noisy to bound 3 KiB.
    let (records_on, records_off) = (
        "Core, 8 agent panes plus 64 run records",
        "Core, 8 agent panes, 64 paced launches, [rewind] off",
    );
    let rewind_inputs = inputs.join("rewind");
    let (rewind_on, rewind_off, rewind_burst) = (
        rewind_inputs.join("on"),
        rewind_inputs.join("off"),
        rewind_inputs.join("burst"),
    );
    let records = mem_gate::Budget {
        metric: records_on,
        limit: mem_gate::LAUNCH_LIMIT,
        settle: mem_gate::REWIND_SETTLE,
        agents: Some(&rewind_on),
        ..webhook
    };
    budgets.extend([
        mem_gate::Budget {
            metric: "Core, 8 agent panes listed, a webhook after one alert",
            settle: mem_gate::WEBHOOK_SETTLE,
            agents: Some(&one_hook),
            ..webhook
        },
        mem_gate::Budget {
            metric: "Core, 8 agent panes listed, a webhook after a burst of 8 alerts",
            settle: mem_gate::WEBHOOK_SETTLE,
            agents: Some(&burst_hook),
            ..webhook
        },
        webhook,
    ]);
    budgets.extend([
        mem_gate::Budget {
            metric: records_off,
            limit: mem_gate::INFORMATION,
            agents: Some(&rewind_off),
            ..records
        },
        mem_gate::Budget {
            metric: "Core, 8 agent panes, 64 launches back to back, [rewind] off (#476)",
            limit: mem_gate::INFORMATION,
            agents: Some(&rewind_burst),
            ..records
        },
        records,
    ]);
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
        inbox: None,
        agents: None,
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
            inbox: None,
            agents: None,
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
            inbox: None,
            agents: None,
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
            inbox: None,
            agents: None,
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
            inbox: None,
            agents: None,
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
            inbox: None,
            agents: None,
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
            inbox: None,
            agents: None,
        });
        let agents = mem_gate::Budget {
            metric: "Window on macOS with 8 agents needing you, 1512x1890",
            limit: mem_gate::WINDOW_LIMIT_HALF_RETINA,
            program: &core,
            args: &["--window", "8", "1512x1890"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: Some(&agents_screen),
            window: Some(Side::Window),
            menu: false,
            inbox: None,
            agents: None,
        };
        budgets.push(mem_gate::Budget {
            metric: "Window on macOS with the inbox open over 8 agents, 1512x1890",
            inbox: Some("open"),
            ..agents
        });
        budgets.push(mem_gate::Budget {
            metric: "Window on macOS after the inbox closed over 8 agents, 1512x1890",
            inbox: Some("closed"),
            ..agents
        });
        budgets.push(mem_gate::Budget {
            metric: "Window on macOS with an answer prompt over the inbox, 8 agents, 1512x1890",
            inbox: Some("answer"),
            ..agents
        });
        // ADR-0051 §8: the same inbox with 16 agents outside PinFold after its own rows.
        budgets.push(mem_gate::Budget {
            metric: "Window on macOS with the inbox open over 8 agents and 16 outside, 1512x1890",
            inbox: Some("open"),
            agents: Some(&outside),
            ..agents
        });
        budgets.push(agents);
        let beside = mem_gate::Budget {
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
            inbox: None,
            agents: None,
        };
        // ADR-0051 §11: branches are read while the inbox is open and kept by nobody, so the
        // three rows match.
        for (metric, inbox) in [
            (
                "Core beside that window, 8 agent panes listed with branches",
                None,
            ),
            (
                "Core with the inbox open over 8 agent panes with branches",
                Some("open"),
            ),
            (
                "Core after the inbox closed over 8 agent panes with branches",
                Some("closed"),
            ),
        ] {
            budgets.push(mem_gate::Budget {
                metric,
                inbox,
                agents: Some(&listed_agents),
                ..beside
            });
        }
        // ADR-0051 §11: the peek's lines are forwarded and kept by nobody.
        for (metric, inbox) in [
            (
                "Core with the inbox open over 8 agent panes, agent files on",
                Some("open"),
            ),
            (
                "Core after the inbox closed over 8 agent panes, agent files on",
                Some("closed"),
            ),
        ] {
            budgets.push(mem_gate::Budget {
                metric,
                inbox,
                agents: Some(&agent_files),
                ..beside
            });
        }
        budgets.push(beside);
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
            inbox: None,
            agents: None,
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
            inbox: None,
            agents: None,
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
            inbox: None,
            agents: None,
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
            inbox: None,
            agents: None,
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
            inbox: None,
            agents: None,
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
            inbox: None,
            agents: None,
        });
        let agents = mem_gate::Budget {
            metric: "Window on X11 with 8 agents needing you, 1920x1080",
            limit: mem_gate::WINDOW_LIMIT_FULL_HD,
            program: &core,
            args: &["--window", "8", "1920x1080"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: Some(&agents_screen),
            window: Some(Side::Window),
            menu: false,
            inbox: None,
            agents: None,
        };
        budgets.push(mem_gate::Budget {
            metric: "Window on X11 with the inbox open over 8 agents, 1920x1080",
            inbox: Some("open"),
            ..agents
        });
        budgets.push(mem_gate::Budget {
            metric: "Window on X11 after the inbox closed over 8 agents, 1920x1080",
            inbox: Some("closed"),
            ..agents
        });
        budgets.push(agents);
        let beside = mem_gate::Budget {
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
            inbox: None,
            agents: None,
        };
        // ADR-0051 §11: branches are read while the inbox is open and kept by nobody, so the
        // three rows match.
        for (metric, inbox) in [
            (
                "Core beside that window, 8 agent panes listed with branches",
                None,
            ),
            (
                "Core with the inbox open over 8 agent panes with branches",
                Some("open"),
            ),
            (
                "Core after the inbox closed over 8 agent panes with branches",
                Some("closed"),
            ),
        ] {
            budgets.push(mem_gate::Budget {
                metric,
                inbox,
                agents: Some(&listed_agents),
                ..beside
            });
        }
        // ADR-0051 §11: the peek's lines are forwarded and kept by nobody.
        for (metric, inbox) in [
            (
                "Core with the inbox open over 8 agent panes, agent files on",
                Some("open"),
            ),
            (
                "Core after the inbox closed over 8 agent panes, agent files on",
                Some("closed"),
            ),
        ] {
            budgets.push(mem_gate::Budget {
                metric,
                inbox,
                agents: Some(&agent_files),
                ..beside
            });
        }
        budgets.push(beside);
    }
    let passed = mem_gate::run(&budgets);
    let tools = Path::new(&target).join("release-core/pinfold");
    let tree = inputs.join("tree");
    let passed = mem_gate::run_takes(&tools, &tree, &inputs.join("state")) && passed;
    let (files, repo) = (
        inputs.join("agent-files"),
        fs::canonicalize(inputs.join("repo")),
    );
    let passed = repo.is_ok_and(|repo| mem_gate::run_peek(&tools, &files, &repo)) && passed;
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
    fn the_run_record_rows_write_shells_sh_reads_and_pace_only_on_and_off() {
        let dir = std::env::temp_dir().join(format!("xtask-rewind-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        write_rewind_inputs(&dir, Path::new("/opt/it's/pinfold")).expect("inputs");
        for (row, paced) in [("on", true), ("off", true), ("burst", false)] {
            let script = dir.join(row).join("agent.sh");
            let checked = Command::new("sh").arg("-n").arg(&script).status();
            assert!(checked.expect("sh").success(), "{row}");
            let text = fs::read_to_string(&script).expect("script");
            assert_eq!(text.contains("sleep 0.1"), paced, "{text}");
            let config = fs::read_to_string(dir.join(row).join("pinfold/pinfold.toml"));
            assert!(config.expect("config").contains("[agents.quick7]"));
        }
        let _ = fs::remove_dir_all(&dir);
    }

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
