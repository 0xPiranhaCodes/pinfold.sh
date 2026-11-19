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
/// over 8 MB, `checkpoint`, a store holding that diff between two checkpoints, and
/// `tool-screen.sh`, a window's
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
    // ADR-0050 slice 7's row: a run's store whose launch and view checkpoints differ by the same
    // diff, and `key`, the store's name, for `pinfold diff --checkpoint`.
    let rewind = dir.join("checkpoint");
    fs::create_dir_all(rewind.join("state"))?;
    fs::create_dir_all(rewind.join("work"))?;
    let work = rewind.join("work");
    let mut key = String::new();
    for (side, reason) in [("old", "launch"), ("new", "view")] {
        fs::write(work.join("a.txt"), lines(side))?;
        let taken = Command::new(tools)
            .args(["checkpoint", "take", "r1-0", reason, "--"])
            .arg(&work)
            .env("HOME", rewind.join("state"))
            .env("XDG_STATE_HOME", rewind.join("state"))
            .output()?;
        let line: serde_json::Value = serde_json::from_slice(&taken.stdout)
            .map_err(|_| io::Error::other("checkpoint take printed no JSON line"))?;
        key = line["store"]
            .as_str()
            .ok_or_else(|| io::Error::other(format!("checkpoint take failed: {line}")))?
            .to_owned();
    }
    fs::write(rewind.join("key"), key)?;
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
    // ADR-0051 §9's `toast: true` row: the heavy screen with program notifications on, whose
    // program toasts 120 cells 5 s in, so the toast is drawn when the window is read at 7 s.
    let toasts = dir.join("toasts");
    fs::create_dir_all(toasts.join("pinfold"))?;
    fs::write(
        toasts.join("pinfold/pinfold.toml"),
        "[alerts]\nprogram_notifications = true\n",
    )?;
    let script = toasts.join("agent.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n(sleep 5; printf '\\033]9;%s\\007' '{}') &\nexec {}\n",
            "Build finished ".repeat(8),
            quote(&fs::canonicalize(HEAVY_SCREEN)?)
        ),
    )?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    // ADR-0051 §9's progress row: every tab's program reports OSC 9;4 progress, then draws the
    // heavy screen, with the default config, so each tab draws its bar and the Dock tile one.
    let progress = dir.join("progress");
    fs::create_dir_all(&progress)?;
    let script = progress.join("agent.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '\\033]9;4;1;40\\007'\nexec {}\n",
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

/// Writes `on`, `exited` and `off` under `dir` for ADR-0052's rows: each a config with
/// `[timeline]` on or off, `[rewind]` off so no checkpoint is taken, an agent profile `fill`
/// whose program prints 1,100 OSC 133 command pairs, so every ring of 1,024 fills and wraps,
/// and then waits or, in `exited`, exits; `polling` and `polling-off`, on and off, then call
/// `pinfold ctl list` 1,000 times from each agent, which adds no entry (slice 2), and wait;
/// `reading`, on, where one agent then reads its own full ring with `pinfold ctl timeline
/// --json` 100 times, 1,000 entries a read (slice 3), and `listing`, the same with 100 `pinfold
/// ctl list` calls, so the two serve as many calls; a `state` folder for the ring files; and `agent.sh`,
/// every pane's shell, which launches one `fill` in a tab and waits. 8 panes make 8 runs.
fn write_timeline_inputs(dir: &Path, tools: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', r"'\''"));
    let wait = "exec /bin/sleep 86400";
    let calls = |count: u32, call: &str| {
        format!(
            "i=0; while [ $i -lt {count} ]; do {} ctl {call} >/dev/null; i=$((i + 1)); done\n{wait}",
            quote(tools)
        )
    };
    // One agent makes the 100 calls, one after another: pane 8, the first agent, as the core's
    // 8 shells are panes 0 to 7. Each finds its own pane first, so each read is a `hello` and a
    // call, as each `ctl list` is, and both rows add as many audit entries.
    let one = |call: &str| {
        format!(
            "me=$({0} ctl list | grep -o '^[0-9]*\\*' | tr -d '*')\n\
             if [ \"$me\" = 8 ]; then\n\
             i=0; while [ $i -lt 100 ]; do {0} ctl {call} >/dev/null; i=$((i + 1)); done\nfi\n{wait}",
            quote(tools)
        )
    };
    let (poll, read, list) = (calls(1000, "list"), one("timeline $me --json"), one("list"));
    for (row, enabled, end) in [
        ("on", true, wait),
        ("exited", true, "exit 0"),
        ("off", false, wait),
        ("polling", true, poll.as_str()),
        ("polling-off", false, poll.as_str()),
        ("reading", true, read.as_str()),
        ("listing", true, list.as_str()),
    ] {
        let folder = dir.join(row);
        for sub in ["pinfold", "state"] {
            fs::create_dir_all(folder.join(sub))?;
        }
        let fill = folder.join("fill.sh");
        fs::write(
            &fill,
            format!(
                "#!/bin/sh\ni=0; while [ $i -lt 1100 ]; do\n\
                 \x20 printf '\\033]133;A\\007$ \\033]133;B\\007true\\r\\n\\033]133;C\\007\\033]133;D;0\\007'\n\
                 \x20 i=$((i + 1))\ndone\n{end}\n"
            ),
        )?;
        fs::set_permissions(&fill, fs::Permissions::from_mode(0o755))?;
        fs::write(
            folder.join("pinfold/pinfold.toml"),
            format!(
                "[timeline]\nenabled = {enabled}\n[rewind]\nenabled = false\n\
                 [agents.fill]\ncommand = [{:?}]\n",
                fill.display().to_string()
            ),
        )?;
        let script = folder.join("agent.sh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n{} agent fill --tab\nexec /bin/sleep 86400\n",
                quote(tools)
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
            write_rewind_inputs(&inputs.join("rewind"), &tools)?;
            write_timeline_inputs(&inputs.join("timeline"), &tools)
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
    let toasts = inputs.join("toasts");
    let progress = inputs.join("progress");
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
            rewind: false,
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
            rewind: false,
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
            rewind: false,
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
            rewind: false,
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
        rewind: false,
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
    // ADR-0052 slice 1: 8 agent runs whose rings each wrapped, live and after their programs
    // exited, under #476's launch ceiling, as the run-record row: main without timelines reads
    // 4.38 MB here. Off is the same output with no timeline, shown beside them: 56 B a run is far
    // under a sample's noise, so the type sizes are asserted in the code.
    let timeline_inputs = inputs.join("timeline");
    let (timeline_on, timeline_exited, timeline_off) = (
        timeline_inputs.join("on"),
        timeline_inputs.join("exited"),
        timeline_inputs.join("off"),
    );
    let timelines = mem_gate::Budget {
        metric: "Core, 8 agent panes with full timelines",
        limit: mem_gate::LAUNCH_LIMIT,
        settle: mem_gate::TIMELINE_SETTLE,
        agents: Some(&timeline_on),
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
    // ADR-0052 slice 2: each agent then polls `pane.list` 1,000 times, which writes no entry,
    // so these two read alike. Serving the calls keeps about 1.7 MB in the core with timelines
    // on or off, on main as here, which puts both over the ceiling (#502): information only.
    let (timeline_polling, timeline_polling_off) = (
        timeline_inputs.join("polling"),
        timeline_inputs.join("polling-off"),
    );
    budgets.extend([
        mem_gate::Budget {
            metric: "Core, 8 agent panes with full timelines, a polling agent each (#502)",
            limit: mem_gate::INFORMATION,
            settle: mem_gate::POLLING_SETTLE,
            agents: Some(&timeline_polling),
            ..timelines
        },
        mem_gate::Budget {
            metric: "Core, the same polling, [timeline] off (#502)",
            limit: mem_gate::INFORMATION,
            settle: mem_gate::POLLING_SETTLE,
            agents: Some(&timeline_polling_off),
            ..timelines
        },
    ]);
    // ADR-0052 slice 3: one agent then reads its own ring of 1,024 with `run.timeline` 100
    // times, 1,000 entries a read, which the core streams from the file and drops with the
    // reply. Beside it the same number of `pane.list` calls, as serving any call keeps memory
    // (#502): the reads should cost what the calls do. Information, as the polling rows are.
    let (timeline_reading, timeline_listing) = (
        timeline_inputs.join("reading"),
        timeline_inputs.join("listing"),
    );
    budgets.extend([
        mem_gate::Budget {
            metric: "Core, 8 agent panes with full timelines, one reads its own 100 times, 1,000 entries a read",
            limit: mem_gate::INFORMATION,
            settle: mem_gate::POLLING_SETTLE,
            agents: Some(&timeline_reading),
            ..timelines
        },
        mem_gate::Budget {
            metric: "Core, the same, 100 pane.list calls instead",
            limit: mem_gate::INFORMATION,
            settle: mem_gate::POLLING_SETTLE,
            agents: Some(&timeline_listing),
            ..timelines
        },
    ]);
    budgets.extend([
        mem_gate::Budget {
            metric: "Core, 8 agent panes, the same output, [timeline] off",
            limit: mem_gate::INFORMATION,
            agents: Some(&timeline_off),
            ..timelines
        },
        mem_gate::Budget {
            metric: "Core, 8 agent panes with full timelines, their programs exited",
            agents: Some(&timeline_exited),
            ..timelines
        },
        timelines,
    ]);
    // ADR-0045 section 5. Each tool runs in a pane of a core, which prints the tool's pid.
    let idle_tool = ["--tool", "1", "files", empty.as_str()];
    let reader = ["--tool", "1", "md", markdown.as_str()];
    let prose_reader = ["--tool", "1", "md", prose.as_str()];
    let defs_reader = ["--tool", "1", "md", defs.as_str()];
    let file_manager = ["--tool", "1", "files", entries.as_str()];
    let diff_viewer = ["--tool", "1", "diff", repo.as_str()];
    let rewind_folder = inputs.join("checkpoint");
    let (rewind_work, rewind_key) = (
        input("checkpoint/work"),
        fs::read_to_string(rewind_folder.join("key")).unwrap_or_default(),
    );
    let rewind_viewer = [
        "--tool",
        "1",
        "rewind",
        rewind_work.as_str(),
        rewind_key.as_str(),
        "r1-0/0",
        "r1-0/1",
    ];
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
        rewind: false,
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
        // ADR-0050's memory row: the same diff read from a run's store with `--checkpoint`.
        mem_gate::Budget {
            metric: "Diff viewer on a checkpoint, git output cut at 8 MB",
            limit: mem_gate::DIFF_LIMIT,
            args: &rewind_viewer,
            agents: Some(&rewind_folder),
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
            rewind: false,
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
            rewind: false,
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
            rewind: false,
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
            rewind: false,
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
            rewind: false,
            inbox: None,
            agents: None,
        });
        // ADR-0050: the prompt at its largest, 10 paths of 256 bytes, within the window budget.
        budgets.push(mem_gate::Budget {
            metric: "Window on macOS with a Rewind prompt, 1512x1890",
            limit: mem_gate::WINDOW_LIMIT_HALF_RETINA,
            program: &core,
            args: &["--window", "1", "1512x1890"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: None,
            window: Some(Side::Window),
            menu: false,
            rewind: true,
            inbox: None,
            agents: None,
        });
        budgets.push(mem_gate::Budget {
            metric: "Window on macOS with a program's toast, 1512x1890",
            limit: mem_gate::WINDOW_LIMIT_HALF_RETINA,
            program: &core,
            args: &["--window", "1", "1512x1890"],
            per_pane_over: None,
            settle: mem_gate::WINDOW_SETTLE,
            wait_for_line: true,
            screen: Some(Path::new(HEAVY_SCREEN)),
            window: Some(Side::Window),
            menu: false,
            rewind: false,
            inbox: None,
            agents: Some(&toasts),
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
            rewind: false,
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
            rewind: false,
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
        // ADR-0051 §9: eight tabs each drawing a progress bar, and the Dock tile its bar.
        budgets.push(mem_gate::Budget {
            metric: "Window on macOS with progress on 8 tabs, 1512x1890",
            screen: None,
            agents: Some(&progress),
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
            rewind: false,
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
            rewind: false,
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
            rewind: false,
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
            rewind: false,
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
            rewind: false,
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
            rewind: false,
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
            rewind: false,
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
            rewind: false,
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
            rewind: false,
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
    fn the_timeline_rows_fill_every_ring_and_only_off_turns_it_off() {
        let dir = std::env::temp_dir().join(format!("xtask-timeline-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        write_timeline_inputs(&dir, Path::new("/opt/it's/pinfold")).expect("inputs");
        for (row, enabled) in [
            ("on", true),
            ("exited", true),
            ("off", false),
            ("polling", true),
            ("polling-off", false),
        ] {
            let folder = dir.join(row);
            let checked = Command::new("sh")
                .arg("-n")
                .arg(folder.join("agent.sh"))
                .status();
            assert!(checked.expect("sh").success(), "{row}");
            let config = fs::read_to_string(folder.join("pinfold/pinfold.toml")).expect("config");
            let want = format!("[timeline]\nenabled = {enabled}\n[rewind]\nenabled = false\n");
            assert!(
                config.starts_with(&want) && config.contains("[agents.fill]"),
                "{config}"
            );
        }
        let fill = dir.join("exited/fill.sh");
        let output = Command::new(&fill).output().expect("fill");
        let ends = output
            .stdout
            .windows(9)
            .filter(|w| w == b"\x1b]133;D;0")
            .count();
        assert_eq!(ends, 1100);
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
