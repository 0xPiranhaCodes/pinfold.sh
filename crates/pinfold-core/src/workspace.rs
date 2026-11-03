//! Which tabs a project's workspace opened, for `pinfold up` (ADR-0034 step 10).
//!
//! The layout comes from an untrusted repository, so every directory and cap is checked again
//! here, before anything opens. Each pane runs the user's shell and nothing else: the caller
//! types trusted `run` lines into it. The record lives in memory and in snapshots.

use std::fs;
use std::path::{Path, PathBuf};

use pinfold_mux::PaneId;
use pinfold_proto::link::{MAX_PANES, MAX_TABS, SplitWay};

use crate::io::Reactor;
use crate::snapshot::Workspace;
use crate::tabs::FIRST_SIZE;

/// One pane of a workspace tab: no way for the tab's first pane, and for each later one the way
/// it splits the pane before it, with the absolute directory its shell starts in.
pub type Pane = (Option<SplitWay>, String);

/// An open workspace: its canonical project directory, its layout's hash if the caller gave
/// one, and the panes it opened in the layout's order, none for one a restore could not keep.
pub type Opened = (PathBuf, Option<String>, Vec<Option<usize>>);

/// The panes each open workspace opened, keyed by its canonical project directory.
#[derive(Debug, Default)]
pub struct Workspaces {
    opened: Vec<Opened>,
}

impl Workspaces {
    /// Opens `tabs` for the project at `root` and focuses the first, or, if tabs of that
    /// project are still open, focuses the first of them and opens nothing. Returns the panes it
    /// opened, in the layout's order.
    ///
    /// # Errors
    ///
    /// Returns why not, and leaves no pane of this call open: `root` or a cwd is not an
    /// existing directory's absolute path, the shape is wrong, a cap is met or a shell will not
    /// start.
    pub fn open(
        &mut self,
        reactor: &mut Reactor,
        root: &str,
        sha256: Option<&str>,
        tabs: &[Vec<Pane>],
    ) -> Result<Vec<usize>, String> {
        let root = directory(root).ok_or("root must be an existing directory's absolute path")?;
        // Pane ids are never reused, so a closed pane keeps its place in the layout's order.
        let held = |pane: &Option<usize>| pane.is_some_and(|pane| holding(reactor, pane).is_some());
        self.opened.retain(|(_, _, panes)| panes.iter().any(held));
        let open = self.opened.iter().find(|(open, _, _)| *open == root);
        if let Some(&Some(pane)) =
            open.and_then(|(_, _, panes)| panes.iter().find(|pane| held(pane)))
        {
            focus(reactor, pane);
            return Ok(Vec::new());
        }
        let room = (
            MAX_TABS.saturating_sub(reactor.session.tabs().len()),
            MAX_PANES.saturating_sub(reactor.pane_count()),
        );
        let tabs = check(&root, tabs, room)?;
        let mut panes = Vec::new();
        if let Err(reason) = open_all(reactor, &tabs, &mut panes) {
            for pane in panes {
                reactor.close_pane(pane);
            }
            return Err(reason);
        }
        focus(reactor, panes[0]);
        let sha256 = sha256.map(str::to_owned);
        self.opened
            .push((root, sha256, panes.iter().copied().map(Some).collect()));
        Ok(panes)
    }

    /// The hashed workspaces, for a snapshot, each pane by its index in `reactor`'s layout.
    #[must_use]
    pub fn saved(&self, reactor: &Reactor) -> Vec<Workspace> {
        let (_, ids) = reactor.session.to_tree();
        let index = |pane: &Option<usize>| ids.iter().position(|id| Some(*id as usize) == *pane);
        let saved = self.opened.iter().filter_map(|(root, sha256, panes)| {
            Some(Workspace {
                root: root.to_str()?.to_owned(),
                sha256: sha256.clone()?,
                panes: panes.iter().map(index).collect(),
            })
        });
        saved.collect()
    }

    /// Notes the workspaces of a snapshot `reactor` just restored, and returns them with each
    /// pane's new id.
    pub fn restored(&mut self, reactor: &Reactor, saved: &[Workspace]) -> &[Opened] {
        let (_, ids) = reactor.session.to_tree();
        let id = |index: &Option<usize>| {
            index
                .and_then(|index| ids.get(index))
                .map(|id| *id as usize)
        };
        self.opened = saved
            .iter()
            .map(|saved| {
                let panes = saved.panes.iter().map(id).collect();
                (
                    PathBuf::from(&saved.root),
                    Some(saved.sha256.clone()),
                    panes,
                )
            })
            .collect();
        &self.opened
    }
}

/// `path` made canonical, if it is an existing directory's absolute path.
fn directory(path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    let path = fs::canonicalize(path).ok().filter(|_| path.is_absolute())?;
    path.is_dir().then_some(path)
}

/// Refuses a layout with no tab, an empty tab, a tab that does not start a tab and then split,
/// more tabs or panes than the session has `room` for, or a cwd that is not a directory under `root`. Returns the tabs with each
/// cwd canonical, as checked, so a later swap of the caller's path cannot redirect a shell.
fn check(
    root: &Path,
    tabs: &[Vec<Pane>],
    (free_tabs, free_panes): (usize, usize),
) -> Result<Vec<Vec<Checked>>, String> {
    let count: usize = tabs.iter().map(Vec::len).sum();
    if tabs.is_empty() || tabs.len() > free_tabs || count > free_panes {
        return Err(format!(
            "a workspace opens 1 tab or more, and room is left for {free_tabs} tabs and {free_panes} panes"
        ));
    }
    for tab in tabs {
        let shaped = tab.first().is_some_and(|first| first.0.is_none())
            && tab.iter().skip(1).all(|pane| pane.0.is_some());
        if !shaped {
            return Err("each tab starts with a pane and then splits".to_owned());
        }
    }
    let pane = |(way, cwd): &Pane| {
        let cwd = directory(cwd).filter(|cwd| cwd.starts_with(root));
        let reason = "cwd must be an existing directory in the project, its absolute path";
        cwd.map(|cwd| (*way, cwd)).ok_or_else(|| reason.to_owned())
    };
    tabs.iter()
        .map(|tab| tab.iter().map(pane).collect())
        .collect()
}

/// A pane whose cwd is canonical and under the project's root.
type Checked = (Option<SplitWay>, PathBuf);

/// Opens each tab and its splits in the background, noting each pane in `panes`.
fn open_all(
    reactor: &mut Reactor,
    tabs: &[Vec<Checked>],
    panes: &mut Vec<usize>,
) -> Result<(), String> {
    for tab in tabs {
        let mut before = None;
        for (way, cwd) in tab {
            let mut command = (reactor.shell)();
            command.current_dir(cwd);
            let size = reactor.area.unwrap_or(FIRST_SIZE);
            let pane = match (before, *way) {
                (Some(before), Some(way)) => reactor.split_with(before, command, way, false),
                _ => reactor.open_tab_with(command, size.rows, size.cols, false),
            };
            let pane = pane.map_err(|refusal| refusal.to_string())?;
            panes.push(pane);
            before = Some(pane);
        }
    }
    Ok(())
}

/// The index of the tab holding `pane`.
fn holding(reactor: &Reactor, pane: usize) -> Option<usize> {
    let mut tabs = reactor.session.tabs().iter();
    tabs.position(|tab| tab.panes().any(|held| held == pane as PaneId))
}

/// Makes the tab holding `pane` active and focuses `pane` in it.
fn focus(reactor: &mut Reactor, pane: usize) {
    if let Some(index) = holding(reactor, pane) {
        let _ = reactor.session.switch_tab(index);
        let _ = reactor.session.focus(pane as PaneId);
        reactor.lay_out();
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn waiting() -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "read line"]);
        command
    }

    fn tab(cwd: &str, splits: &[SplitWay]) -> Vec<Pane> {
        let mut panes = vec![(None, cwd.to_owned())];
        panes.extend(splits.iter().map(|way| (Some(*way), cwd.to_owned())));
        panes
    }

    #[test]
    fn a_workspace_opens_once_and_then_attaches_until_its_tabs_close() {
        let mut reactor = Reactor::new().unwrap();
        reactor.shell = waiting;
        let mine = reactor.open_tab(waiting(), 24, 80).unwrap();
        let mut workspaces = Workspaces::default();
        let tabs = [tab("/", &[]), tab("/tmp", &[SplitWay::Down])];
        assert_eq!(
            workspaces
                .open(&mut reactor, "/", None, &tabs)
                .map(|panes| panes.len()),
            Ok(3)
        );
        assert_eq!(reactor.session.tabs().len(), 3);
        assert_eq!(reactor.session.active(), 1);

        // Attaching focuses the workspace's first tab and opens nothing.
        reactor.session.switch_tab(0).unwrap();
        assert_eq!(
            workspaces
                .open(&mut reactor, "/usr/..", None, &tabs)
                .map(|panes| panes.len()),
            Ok(0)
        );
        assert_eq!(
            (reactor.session.tabs().len(), reactor.session.active()),
            (3, 1)
        );

        // Once the person closes them, it opens again.
        for pane in [mine + 1, mine + 2, mine + 3] {
            assert!(reactor.close_pane(pane));
        }
        assert_eq!(
            workspaces
                .open(&mut reactor, "/", None, &tabs)
                .map(|panes| panes.len()),
            Ok(3)
        );
    }

    #[test]
    fn a_snapshot_keeps_each_hashed_workspace_pane_by_place_and_a_restore_maps_it_back() {
        let mut reactor = Reactor::new().unwrap();
        reactor.shell = waiting;
        let mut workspaces = Workspaces::default();
        let sha256 = "0".repeat(64);
        let tabs = [tab("/", &[SplitWay::Right, SplitWay::Down])];
        let panes = workspaces.open(&mut reactor, "/", Some(&sha256), &tabs);
        let panes = panes.unwrap();
        workspaces
            .open(&mut reactor, "/tmp", None, &[tab("/tmp", &[])])
            .unwrap();
        assert!(reactor.close_pane(panes[1]));
        let saved = workspaces.saved(&reactor);
        let expected = Workspace {
            root: "/".to_owned(),
            sha256,
            panes: vec![Some(0), None, Some(1)],
        };
        assert_eq!(saved, [expected]);

        let mut restored = Reactor::new().unwrap();
        restored.shell = waiting;
        restored.restore(&reactor.snapshot()).unwrap();
        let mut again = Workspaces::default();
        let opened = again.restored(&restored, &saved);
        let first = restored.session.to_tree().1[0] as usize;
        assert_eq!(opened[0].2, [Some(first), None, Some(first + 1)]);
        // Restored, it attaches rather than opening again.
        assert_eq!(again.open(&mut restored, "/", None, &tabs), Ok(Vec::new()));
    }

    #[test]
    fn a_bad_root_cwd_shape_or_count_refuses_and_opens_nothing() {
        let mut reactor = Reactor::new().unwrap();
        reactor.shell = waiting;
        let mut workspaces = Workspaces::default();
        let refused = |workspaces: &mut Workspaces, reactor: &mut Reactor, root, tabs: &[_]| {
            assert!(workspaces.open(reactor, root, None, tabs).is_err());
            assert_eq!(reactor.pane_count(), 0);
        };
        let good = [tab("/", &[])];
        for root in ["relative", "/no/such/place", "/etc/hosts"] {
            refused(&mut workspaces, &mut reactor, root, &good);
        }
        for cwd in ["relative", "/no/such/place", "/etc/hosts"] {
            refused(&mut workspaces, &mut reactor, "/", &[tab(cwd, &[])]);
        }
        let split_first = vec![(Some(SplitWay::Right), "/".to_owned())];
        let tab_later = vec![(None, "/".to_owned()), (None, "/".to_owned())];
        for tabs in [vec![], vec![vec![]], vec![split_first], vec![tab_later]] {
            refused(&mut workspaces, &mut reactor, "/", &tabs);
        }
        refused(
            &mut workspaces,
            &mut reactor,
            "/",
            &vec![tab("/", &[]); MAX_TABS + 1],
        );
        let wide = vec![tab("/", &[SplitWay::Right; MAX_PANES]); 1];
        refused(&mut workspaces, &mut reactor, "/", &wide);

        // Within the caps alone, but not beside the pane already open.
        reactor.open_tab(waiting(), 24, 80).unwrap();
        let full = vec![tab("/", &[]); MAX_TABS];
        assert!(workspaces.open(&mut reactor, "/", None, &full).is_err());
        assert_eq!(reactor.pane_count(), 1);
    }

    #[test]
    fn a_cwd_outside_the_project_or_a_symlink_out_of_it_refuses() {
        let mut reactor = Reactor::new().unwrap();
        reactor.shell = waiting;
        let mut workspaces = Workspaces::default();
        let root = std::env::temp_dir().join(format!("pinfold-ws-{}", std::process::id()));
        fs::create_dir_all(root.join("inside")).unwrap();
        std::os::unix::fs::symlink("/etc", root.join("out")).unwrap();
        let text = |path: PathBuf| path.to_str().unwrap().to_owned();
        let at = text(root.clone());
        for cwd in [
            "/etc".to_owned(),
            text(std::env::temp_dir()),
            text(root.join("out")),
        ] {
            assert!(
                workspaces
                    .open(&mut reactor, &at, None, &[tab(&cwd, &[])])
                    .is_err(),
                "{cwd}"
            );
            assert_eq!(reactor.pane_count(), 0);
        }
        let inside = [tab(&text(root.join("inside")), &[])];
        assert_eq!(
            workspaces
                .open(&mut reactor, &at, None, &inside)
                .map(|panes| panes.len()),
            Ok(1)
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
