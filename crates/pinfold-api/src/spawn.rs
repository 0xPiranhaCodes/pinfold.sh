//! `pane.spawn`'s parameters, their shape checked. Scope is the reactor's to check.

use serde_json::Value;

/// Where `pane.spawn` puts a pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// `split-right`.
    SplitRight,
    /// `split-down`.
    SplitDown,
    /// `tab`: a new tab after the others.
    Tab,
}

/// `pane.spawn`: a pane running the user's shell. There is no command: a caller cannot name a
/// program, as with `window.open`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawn {
    /// Where the pane goes.
    pub place: Place,
    /// The pane to split, or nothing for the caller's own.
    pub of: Option<usize>,
    /// An absolute path the shell starts in, which the reactor checks is a directory.
    pub cwd: Option<String>,
}

/// Reads `pane.spawn`'s parameters, refusing any it does not know, such as a command.
pub(crate) fn params(params: Option<&Value>) -> Result<Spawn, &'static str> {
    const SHAPE: &str = "pane.spawn takes where, of and an absolute cwd, and runs the user's shell";
    let params = params.and_then(Value::as_object).ok_or(SHAPE)?;
    if params
        .keys()
        .any(|key| !matches!(key.as_str(), "where" | "of" | "cwd"))
    {
        return Err(SHAPE);
    }
    let place = match params.get("where").and_then(Value::as_str) {
        Some("split-right") => Place::SplitRight,
        Some("split-down") => Place::SplitDown,
        Some("tab") => Place::Tab,
        _ => return Err(SHAPE),
    };
    let of = match params.get("of") {
        None | Some(Value::Null) => None,
        Some(of) => Some(
            of.as_u64()
                .and_then(|of| usize::try_from(of).ok())
                .ok_or(SHAPE)?,
        ),
    };
    let cwd = match params.get("cwd") {
        None | Some(Value::Null) => None,
        Some(Value::String(cwd)) if cwd.starts_with('/') => Some(cwd.clone()),
        Some(_) => return Err(SHAPE),
    };
    Ok(Spawn { place, of, cwd })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_spawn_takes_a_place_a_pane_and_a_directory_and_never_a_command() {
        let spawn = |params: Value| super::params(Some(&params));
        assert_eq!(
            spawn(json!({"where": "tab"})),
            Ok(Spawn {
                place: Place::Tab,
                of: None,
                cwd: None
            })
        );
        assert_eq!(
            spawn(json!({"where": "split-down", "of": 3, "cwd": "/tmp"})),
            Ok(Spawn {
                place: Place::SplitDown,
                of: Some(3),
                cwd: Some("/tmp".to_owned())
            })
        );
        let bad = [
            json!(null),
            json!({}),
            json!({"where": "split-left"}),
            json!({"where": "tab", "of": -1}),
            json!({"where": "tab", "cwd": "tmp"}),
            json!({"where": "tab", "cwd": 1}),
            json!({"where": "tab", "command": "sh"}),
            json!({"where": "tab", "argv": ["sh"]}),
        ];
        for params in bad {
            assert!(spawn(params.clone()).is_err(), "{params}");
        }
        assert!(super::params(None).is_err());
    }
}
