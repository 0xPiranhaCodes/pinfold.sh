//! Recorded sessions of real agent CLIs, replayed through the VT engine and status engine.
//!
//! Each `agent_sessions/NAME.rec` holds the PTY output of a shell running an agent, as
//! `DELAY_MS LENGTH\n` and LENGTH raw bytes per read. `NAME.states` lists the statuses the
//! pane shows, in order. `agent_sessions/README.md` says how each was recorded.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use pinfold_core::status::Tracker;
use pinfold_core::vt::{AlacrittyEngine, VtEngine};
use regex_lite::Regex;

/// The size every session was recorded at.
const ROWS: u16 = 24;
const COLUMNS: u16 = 80;

/// The reads of a recording, each with the delay before it.
#[cfg(test)]
fn chunks(data: &[u8]) -> Vec<(Duration, &[u8])> {
    let mut chunks = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let end = rest.iter().position(|&byte| byte == b'\n').unwrap();
        let header = std::str::from_utf8(&rest[..end]).unwrap();
        let (delay, length) = header.split_once(' ').unwrap();
        let (delay, length): (u64, usize) = (delay.parse().unwrap(), length.parse().unwrap());
        rest = &rest[end + 1..];
        chunks.push((Duration::from_millis(delay), &rest[..length]));
        rest = &rest[length..];
    }
    chunks
}

/// The statuses a pane shows while it replays the recording, without repeats.
fn replay(data: &[u8]) -> Vec<&'static str> {
    let mut engine = AlacrittyEngine::new(ROWS, COLUMNS);
    let mut tracker = Tracker::default();
    let mut now = Instant::now();
    let mut shown = vec![tracker.status().name()];
    for (delay, chunk) in chunks(data) {
        now += delay;
        engine.feed(chunk, &mut Vec::new());
        tracker.observe_shell(engine.shell_marks(), now);
        let status = tracker.status().name();
        if shown.last() != Some(&status) {
            shown.push(status);
        }
    }
    shown
}

/// Fails if a recording holds something that identifies a person or machine, or a key.
#[cfg(test)]
fn assert_scrubbed(name: &str, data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    for pattern in [
        r"/Users/",
        r"/home/",
        r"\bsk-[\w-]{8}",
        r"[\w.+-]+@[\w-]+\.[\w.]+",
    ] {
        let found = Regex::new(pattern).unwrap().find(&text);
        assert!(found.is_none(), "{name} holds {found:?}");
    }
    let host = Command::new("hostname").output().map(|out| out.stdout);
    let host = String::from_utf8_lossy(host.as_deref().unwrap_or_default()).to_string();
    let user = std::env::var("USER").unwrap_or_default();
    for word in [host.trim().split('.').next().unwrap_or_default(), &user] {
        assert!(
            word.len() < 3 || !text.contains(word),
            "{name} holds {word:?}"
        );
    }
}

#[test]
fn recorded_agent_sessions_show_the_expected_statuses() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/agent_sessions");
    let mut replayed = 0;
    for entry in fs::read_dir(&directory).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|extension| extension != "rec") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let data = fs::read(&path).unwrap();
        assert!(data.len() < 200_000, "{name} is {} bytes", data.len());
        assert_scrubbed(&name, &data);
        let expected = fs::read_to_string(path.with_extension("states")).unwrap();
        assert_eq!(
            replay(&data),
            expected.lines().collect::<Vec<_>>(),
            "{name}"
        );
        replayed += 1;
    }
    assert!(
        replayed >= 2,
        "{replayed} sessions in {}",
        directory.display()
    );
}

#[test]
fn reads_come_back_as_written() {
    let data = b"5 3\nabc0 0\n12 1\n\n";
    let read: Vec<_> = chunks(data)
        .into_iter()
        .map(|(d, c)| (d.as_millis(), c))
        .collect();
    assert_eq!(read, [(5, &b"abc"[..]), (0, b""), (12, b"\n")]);
}
