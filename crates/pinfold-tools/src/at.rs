//! `@` in the file manager: the chosen paths typed into the last focused agent pane's prompt
//! (ADR-0045 section 4). The tool is an ordinary API caller with no extra rights, so this is a
//! `pane.send` to another pane, which the window approves as any cross-pane send is.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use pinfold_api::control;
use serde_json::{Value, json};

use crate::files::flagged;

/// The paths as an agent's prompt takes them: absolute, single-quoted when they hold anything
/// but `A-Z a-z 0-9 . _ / + - @`, each followed by a space and none by a newline, so nothing is
/// submitted. A name with a control or bidi character is refused rather than sent.
pub(crate) fn text(paths: &[PathBuf]) -> Result<String, String> {
    let mut text = String::new();
    for path in paths {
        let bytes = path.as_os_str().as_bytes();
        if flagged(bytes) {
            return Err("a name with a control or bidi character is not sent".to_owned());
        }
        let path = String::from_utf8_lossy(bytes);
        let plain = |c: char| c.is_ascii_alphanumeric() || "._/+-@".contains(c);
        if path.chars().all(plain) {
            text.push_str(&path);
        } else {
            text.push('\'');
            text.push_str(&path.replace('\'', r"'\''"));
            text.push('\'');
        }
        text.push(' ');
    }
    Ok(text)
}

/// The pane to send to in `pane.list`'s result: the most recently focused one that is not a
/// tool, not the caller `own`, and has had a status reported from inside it, as an agent's
/// hooks report one.
pub(crate) fn target(own: Option<u64>, list: &Value) -> Option<u64> {
    let panes = list["panes"].as_array()?;
    panes
        .iter()
        .filter(|pane| pane["kind"].is_null() && pane["source"] == "reported")
        .filter(|pane| pane["id"].as_u64().is_some() && pane["id"].as_u64() != own)
        .max_by_key(|pane| pane["focused"].as_u64().unwrap_or(0))
        .and_then(|pane| pane["id"].as_u64())
}

/// Lists the panes, then sends `text` to the target, waiting while the window asks the person.
/// Returns what to show either way.
pub(crate) fn send(text: &str) -> Result<String, String> {
    let socket = control::socket_path(&control::runtime_dir());
    let connect = || UnixStream::connect(&socket).map_err(|_| "PinFold is not running".to_owned());
    let refused = |error: control::Error| error.to_string();
    let (hello, listed) = control::list_panes(&connect()?).map_err(refused)?;
    let pane = target(hello["pane"].as_u64(), &listed).ok_or("no agent pane to send to")?;
    control::ask(
        &connect()?,
        "pane.send",
        &json!({"pane": pane, "text": text}),
    )
    .map_err(refused)?;
    Ok(format!("sent to pane {pane}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_quoted_spaced_and_never_end_a_line() {
        let paths = ["/w/src/main.rs", "/w/a b/it's.md", "/w/$(rm -rf ~)"].map(PathBuf::from);
        let expected = r"/w/src/main.rs '/w/a b/it'\''s.md' '/w/$(rm -rf ~)' ";
        assert_eq!(text(&paths).unwrap(), expected);
        for hostile in ["/w/a\nb", "/w/a\u{202e}b", "/w/\x1b[2J"] {
            assert!(text(&[PathBuf::from(hostile)]).is_err(), "{hostile:?}");
        }
    }

    #[test]
    fn the_target_is_the_last_focused_agent_pane_but_never_a_tool_or_the_caller() {
        let pane = |id: u64, source: &str, focused: u64| json!({"id": id, "source": source, "focused": focused});
        let mut tool = pane(4, "reported", 9);
        tool["kind"] = json!("tool");
        let list = json!({"panes": [
            pane(0, "reported", 3), pane(1, "marks", 8), pane(2, "reported", 5),
            pane(3, "reported", 7), tool,
        ]});
        assert_eq!(target(Some(3), &list), Some(2));
        assert_eq!(target(None, &list), Some(3));
        assert_eq!(
            target(Some(0), &json!({"panes": [pane(1, "marks", 1)]})),
            None
        );
    }
}
