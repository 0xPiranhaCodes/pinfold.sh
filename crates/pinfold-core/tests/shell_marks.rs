//! OSC 133 marks and the OSC 7 working directory, from bytes to what the engine reports.

use std::fmt::Write;

use pinfold_core::vt::{AlacrittyEngine, MAX_SEQUENCE, VtEngine};
use proptest::prelude::*;

fn feed(engine: &mut AlacrittyEngine, bytes: &[u8]) {
    let mut replies = Vec::new();
    engine.feed(bytes, &mut replies);
    assert!(replies.is_empty(), "{bytes:?}");
}

/// Feeds each string as an OSC body after `seed`, and lists what the engine then reports.
fn report(seed: &[u8], cases: &[&str], end: &str) -> String {
    let mut lines = String::new();
    for case in cases {
        let mut engine = AlacrittyEngine::new(4, 20);
        feed(&mut engine, seed);
        feed(&mut engine, format!("\x1b]{case}{end}").as_bytes());
        let marks = engine.shell_marks();
        let cwd = engine.cwd().map(|cwd| (&cwd.host, &cwd.path));
        let shown = format!("{:?} {:?} {:?}", marks.phase, marks.finished, marks.exit);
        let _ = writeln!(lines, "{case:?} => {shown} {cwd:?}");
    }
    lines
}

const BELL: &str = "\x07";

#[test]
fn well_formed_marks() {
    let cases = [
        "133;A",
        "133;A;cl=m;aid=1",
        "133;B",
        "133;C",
        "133;C;cmdline=ls",
        "133;D",
        "133;D;0",
        "133;D;255",
        "133;D;007",
        "133;D;130;aid=1",
        "133;D;aid=1",
    ];
    insta::assert_snapshot!(report(b"", &cases, BELL), @r#"
    "133;A" => Prompt 0 None None
    "133;A;cl=m;aid=1" => Prompt 0 None None
    "133;B" => Input 0 None None
    "133;C" => Running 0 None None
    "133;C;cmdline=ls" => Running 0 None None
    "133;D" => Finished 1 None None
    "133;D;0" => Finished 1 Some(0) None
    "133;D;255" => Finished 1 Some(255) None
    "133;D;007" => Finished 1 Some(7) None
    "133;D;130;aid=1" => Finished 1 Some(130) None
    "133;D;aid=1" => Finished 1 None None
    "#);
}

#[test]
fn malformed_marks_change_nothing() {
    let cases = [
        "133",
        "133;",
        "133;a",
        "133;AB",
        "133;Z",
        "133;A\n",
        "133;\u{e9}",
        "1330;A",
        "0133;A",
        "13;A",
        " 133;A",
        "133;D;",
        "133;D;256",
        "133;D;-1",
        "133;D;+1",
        "133;D;1x",
        "133;D;99999999999999999999",
    ];
    insta::assert_snapshot!(report(b"\x1b]133;D;9\x07\x1b]133;B\x07", &cases, BELL), @r#"
    "133" => Input 1 Some(9) None
    "133;" => Input 1 Some(9) None
    "133;a" => Input 1 Some(9) None
    "133;AB" => Input 1 Some(9) None
    "133;Z" => Input 1 Some(9) None
    "133;A\n" => Input 1 Some(9) None
    "133;é" => Input 1 Some(9) None
    "1330;A" => Input 1 Some(9) None
    "0133;A" => Input 1 Some(9) None
    "13;A" => Input 1 Some(9) None
    " 133;A" => Input 1 Some(9) None
    "133;D;" => Input 1 Some(9) None
    "133;D;256" => Input 1 Some(9) None
    "133;D;-1" => Input 1 Some(9) None
    "133;D;+1" => Input 1 Some(9) None
    "133;D;1x" => Input 1 Some(9) None
    "133;D;99999999999999999999" => Input 1 Some(9) None
    "#);
}

#[test]
fn well_formed_directories() {
    let cases = [
        "7;file:///",
        "7;file:///tmp",
        "7;file:///tmp/",
        "7;file://localhost/tmp",
        "7;file://my-mac_1.local/Users/me",
        "7;file:///a%20b",
        "7;file:///%E6%BC%A2%e5%ad%97",
        "7;file:///漢字",
        "7;file:///a;b",
        "7;file:///a%2Fb",
        "7;file:///100%25",
        "7;file:///%2541",
        "7;file:///a/..b/c..",
    ];
    insta::assert_snapshot!(report(b"", &cases, BELL), @r#"
    "7;file:///" => Unknown 0 None Some(("", "/"))
    "7;file:///tmp" => Unknown 0 None Some(("", "/tmp"))
    "7;file:///tmp/" => Unknown 0 None Some(("", "/tmp/"))
    "7;file://localhost/tmp" => Unknown 0 None Some(("localhost", "/tmp"))
    "7;file://my-mac_1.local/Users/me" => Unknown 0 None Some(("my-mac_1.local", "/Users/me"))
    "7;file:///a%20b" => Unknown 0 None Some(("", "/a b"))
    "7;file:///%E6%BC%A2%e5%ad%97" => Unknown 0 None Some(("", "/漢字"))
    "7;file:///漢字" => Unknown 0 None Some(("", "/漢字"))
    "7;file:///a;b" => Unknown 0 None Some(("", "/a;b"))
    "7;file:///a%2Fb" => Unknown 0 None Some(("", "/a/b"))
    "7;file:///100%25" => Unknown 0 None Some(("", "/100%"))
    "7;file:///%2541" => Unknown 0 None Some(("", "/%41"))
    "7;file:///a/..b/c.." => Unknown 0 None Some(("", "/a/..b/c.."))
    "#);
}

#[test]
fn malformed_directories_keep_the_last_good_one() {
    let long_host = format!("7;file://{}/x", "h".repeat(256));
    let cases = [
        "7",
        "7;",
        "07;file:///x",
        "71;file:///x",
        "7;/tmp",
        "7;tmp",
        "7;file:",
        "7;file:/tmp",
        "7;file://",
        "7;file://tmp",
        "7;FILE:///x",
        "7;http://e.com/x",
        "7;file://user@host/x",
        "7;file://host:22/x",
        "7;file://h%6Fst/x",
        "7;file://漢/x",
        "7;file:///x?y",
        "7;file:///x#y",
        "7;file:///%",
        "7;file:///%2",
        "7;file:///%zz",
        "7;file:///%+1",
        "7;file:///%é",
        "7;file:///%ff",
        "7;file:///%c0%ae",
        "7;file:///a%00b",
        "7;file:///a%0Ab",
        "7;file:///a\nb",
        "7;file:///a%7Fb",
        "7;file:///a%C2%9Bb",
        "7;file:///a/../b",
        "7;file:///..",
        "7;file:///a/%2e%2E/b",
        "7;file:///a%2F..%2Fb",
        &long_host,
    ];
    let lines = report(b"\x1b]7;file://good/ok\x07", &cases, BELL);
    let kept = r#"Unknown 0 None Some(("good", "/ok"))"#;
    for line in lines.lines() {
        assert!(line.ends_with(kept), "{line}");
    }
    assert_eq!(lines.lines().count(), cases.len());
}

#[test]
fn terminators() {
    let body = ["133;D;3"];
    let snapshot = ["\x07", "\x1b\\", "\x1bx", "\x18", "\x1a", "\u{9c}"]
        .map(|end| format!("{end:?} {}", report(b"", &body, end)))
        .concat();
    insta::assert_snapshot!(snapshot, @r#"
    "\u{7}" "133;D;3" => Finished 1 Some(3) None
    "\u{1b}\\" "133;D;3" => Finished 1 Some(3) None
    "\u{1b}x" "133;D;3" => Finished 1 Some(3) None
    "\u{18}" "133;D;3" => Unknown 0 None None
    "\u{1a}" "133;D;3" => Unknown 0 None None
    "\u{9c}" "133;D;3" => Unknown 0 None None
    "#);
}

#[test]
fn a_command_that_ended_is_still_seen_after_the_next_prompt() {
    let mut engine = AlacrittyEngine::new(4, 20);
    feed(&mut engine, b"\x1b]133;C\x07");
    feed(&mut engine, b"\x1b]133;D;1\x07\x1b]133;A\x07");
    feed(&mut engine, b"\x1b]133;C\x07\x1b]133;D\x07\x1b]133;A\x07");
    let marks = engine.shell_marks();
    assert_eq!((marks.finished, marks.exit), (2, None));
}

#[test]
fn a_string_at_the_limit_is_dropped_whole() {
    let url = |length: usize| {
        let path = "a".repeat(length - b"7;file:///".len());
        format!("\x1b]7;file:///{path}\x07")
    };
    let mut engine = AlacrittyEngine::new(4, 20);
    feed(&mut engine, url(MAX_SEQUENCE - 1).as_bytes());
    let path = engine.cwd().map(|cwd| cwd.path.len());
    assert_eq!(path, Some(MAX_SEQUENCE - 1 - "7;file://".len()));

    feed(&mut engine, b"\x1b]7;file:///short\x07");
    feed(&mut engine, url(MAX_SEQUENCE).as_bytes());
    feed(&mut engine, url(MAX_SEQUENCE + 1).as_bytes());
    assert_eq!(engine.cwd().map(|cwd| cwd.path.as_str()), Some("/short"));
}

#[test]
fn a_sequence_can_arrive_a_byte_at_a_time() {
    let mut engine = AlacrittyEngine::new(4, 20);
    for &byte in b"\x1b]133;D;3\x1b\\\x1b]7;file://h/a%20b\x07" {
        feed(&mut engine, &[byte]);
    }
    assert_eq!(engine.shell_marks().exit, Some(3));
    let cwd = engine
        .cwd()
        .map(|cwd| (cwd.host.as_str(), cwd.path.as_str()));
    assert_eq!(cwd, Some(("h", "/a b")));
}

#[test]
fn marks_never_reach_the_screen() {
    let mut engine = AlacrittyEngine::new(4, 20);
    feed(&mut engine, b"\x1b]133;A\x07$ \x1b]7;file:///x\x07ls");
    assert_eq!(engine.line_text(0), "$ ls");
}

proptest! {
    #[test]
    fn a_split_read_reports_the_same(
        pieces in prop::collection::vec(
            prop_oneof![
                Just(b"\x1b]133;D;7\x1b\\".to_vec()),
                Just(b"\x1b]133;A\x07".to_vec()),
                Just(b"\x1b]7;file://h/a%20b\x07".to_vec()),
                Just(b"\x1b]7;file:///%zz\x07".to_vec()),
                Just(b"\x1b]2;title\x07".to_vec()),
                Just(b"\x1b\x18".to_vec()),
                prop::collection::vec(any::<u8>(), 0..16),
            ],
            0..32,
        ),
        cut in any::<prop::sample::Index>(),
    ) {
        let bytes = pieces.concat();
        let cut = cut.index(bytes.len() + 1);
        let mut whole = AlacrittyEngine::new(4, 20);
        let mut split = AlacrittyEngine::new(4, 20);
        whole.feed(&bytes, &mut Vec::new());
        split.feed(&bytes[..cut], &mut Vec::new());
        split.feed(&bytes[cut..], &mut Vec::new());
        prop_assert_eq!(whole.shell_marks(), split.shell_marks());
        prop_assert_eq!(whole.cwd(), split.cwd());
    }
}
