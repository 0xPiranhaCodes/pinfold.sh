//! `workspace.open`'s parameters, their shape checked. Directories and caps are the reactor's to
//! check.

use serde_json::Value;

use crate::spawn::Place;

/// `workspace.open`: a project's layout as shells only. A pane has no command or `run` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Open {
    /// The project's absolute directory, which names the workspace.
    pub root: String,
    /// Each tab's panes: [`Place::Tab`] for the first, and a split of the pane before for each
    /// later one, with the absolute directory its shell starts in.
    pub tabs: Vec<Vec<(Place, String)>>,
}

/// Reads `workspace.open`'s parameters, refusing any key it does not know, such as `run`.
pub(crate) fn params(params: Option<&Value>) -> Option<Open> {
    let params = params?.as_object()?;
    if params.keys().any(|key| key != "root" && key != "layout") {
        return None;
    }
    let root = absolute(params.get("root")?)?;
    let layout = params.get("layout")?.as_object()?;
    if layout.keys().any(|key| key != "tabs") {
        return None;
    }
    let tabs = layout.get("tabs")?.as_array()?.iter().map(|tab| {
        let tab = tab
            .as_object()
            .filter(|tab| tab.keys().all(|key| key == "panes"));
        let panes = tab?.get("panes")?.as_array()?.iter().enumerate();
        panes.map(|(index, pane)| pane_of(index, pane)).collect()
    });
    let tabs = tabs.collect::<Option<_>>()?;
    Some(Open { root, tabs })
}

/// A pane: the first of a tab has no `split`, and each later one splits `right` or `down`.
fn pane_of(index: usize, pane: &Value) -> Option<(Place, String)> {
    let pane = pane.as_object()?;
    if pane.keys().any(|key| key != "split" && key != "cwd") {
        return None;
    }
    let place = match (index, pane.get("split").and_then(Value::as_str)) {
        (0, None) => Place::Tab,
        (1.., Some("right")) => Place::SplitRight,
        (1.., Some("down")) => Place::SplitDown,
        _ => return None,
    };
    Some((place, absolute(pane.get("cwd")?)?))
}

fn absolute(path: &Value) -> Option<String> {
    path.as_str()
        .filter(|path| path.starts_with('/'))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_workspace_takes_a_root_and_tabs_of_cwds_and_splits_and_never_a_command() {
        let open = |params: Value| super::params(Some(&params));
        let layout = json!({"tabs": [
            {"panes": [{"cwd": "/p"}]},
            {"panes": [{"cwd": "/p/a"}, {"split": "down", "cwd": "/p/b"}]},
        ]});
        let tabs = vec![
            vec![(Place::Tab, "/p".to_owned())],
            vec![
                (Place::Tab, "/p/a".to_owned()),
                (Place::SplitDown, "/p/b".to_owned()),
            ],
        ];
        let root = "/p".to_owned();
        assert_eq!(
            open(json!({"root": "/p", "layout": layout})),
            Some(Open { root, tabs })
        );
        let pane = |pane: Value| json!({"root": "/p", "layout": {"tabs": [{"panes": pane}]}});
        let bad = [
            json!(null),
            json!({"root": "/p"}),
            json!({"root": "p", "layout": {"tabs": []}}),
            json!({"root": "/p", "layout": {"tabs": []}, "run": "make"}),
            json!({"root": "/p", "layout": {"tabs": [], "name": "x"}}),
            pane(json!([{"cwd": "p"}])),
            pane(json!([{"cwd": "/p", "run": "make"}])),
            pane(json!([{"cwd": "/p", "command": "sh"}])),
            pane(json!([{"split": "right", "cwd": "/p"}])),
            pane(json!([{"cwd": "/p"}, {"cwd": "/p"}])),
            pane(json!([{"cwd": "/p"}, {"split": "left", "cwd": "/p"}])),
            pane(json!([{}])),
        ];
        for params in bad {
            assert!(open(params.clone()).is_none(), "{params}");
        }
        assert!(super::params(None).is_none());
    }
}
