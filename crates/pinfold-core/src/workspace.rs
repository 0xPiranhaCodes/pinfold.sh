//! Which tabs a project's workspace opened, for `pinfold up` (ADR-0034 step 10).
//!
//! The layout comes from an untrusted repository, so every directory and cap is checked again
//! here, before anything opens. Each pane runs the user's shell and nothing else: `run` lines
//! wait for workspace trust. The record lives in memory only.

use std::fs;
use std::path::{Path, PathBuf};

use pinfold_mux::PaneId;
use pinfold_proto::link::{MAX_PANES, MAX_TABS, SplitWay};

use crate::io::Reactor;
use crate::tabs::FIRST_SIZE;

/// One pane of a workspace tab: no way for the tab's first pane, and for each later one the way
/// it splits the pane before it, with the absolute directory its shell starts in.
pub type Pane = (Option<SplitWay>, String);

/// The panes each open workspace opened, keyed by its canonical project directory.
#[derive(Debug, Default)]
pub struct Workspaces {
    opened: Vec<(PathBuf, Vec<usize>)>,
}

impl Workspaces {
    /// Opens `tabs` for the project at `root` and focuses the first, or, if tabs of that
    /// project are still open, focuses the first of them and opens nothing. Returns how many
    /// panes it opened.
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
        tabs: &[Vec<Pane>],
    ) -> Result<usize, String> {
        let root = directory(root).ok_or("root must be an existing directory's absolute path")?;
        let held = |pane: &usize| holding(reactor, *pane).is_some();
        for (_, panes) in &mut self.opened {
            panes.retain(held);
        }
        self.opened.retain(|(_, panes)| !panes.is_empty());
        if let Some((_, panes)) = self.opened.iter().find(|(open, _)| *open == root) {
            focus(reactor, panes[0]);
            return Ok(0);
        }
        check(tabs)?;
        let mut panes = Vec::new();
        if let Err(reason) = open_all(reactor, tabs, &mut panes) {
            for pane in panes {
                reactor.close_pane(pane);
            }
            return Err(reason);
        }
        focus(reactor, panes[0]);
        let count = panes.len();
        self.opened.push((root, panes));
        Ok(count)
    }
}

/// `path` made canonical, if it is an existing directory's absolute path.
fn directory(path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    let path = fs::canonicalize(path).ok().filter(|_| path.is_absolute())?;
    path.is_dir().then_some(path)
}

/// Refuses a layout with no tab, an empty tab, a tab that does not start a tab and then split,
/// more than the caps, or a cwd that is not a directory.
fn check(tabs: &[Vec<Pane>]) -> Result<(), String> {
    let count: usize = tabs.iter().map(Vec::len).sum();
    if tabs.is_empty() || tabs.len() > MAX_TABS || count > MAX_PANES {
        return Err(format!(
            "a workspace opens 1 to {MAX_TABS} tabs and at most {MAX_PANES} panes"
        ));
    }
    for tab in tabs {
        let shaped = tab.first().is_some_and(|first| first.0.is_none())
            && tab.iter().skip(1).all(|pane| pane.0.is_some());
        if !shaped {
            return Err("each tab starts with a pane and then splits".to_owned());
        }
        if tab.iter().any(|pane| directory(&pane.1).is_none()) {
            return Err("cwd must be an existing directory's absolute path".to_owned());
        }
    }
    Ok(())
}

/// Opens each tab and its splits in the background, noting each pane in `panes`.
fn open_all(
    reactor: &mut Reactor,
    tabs: &[Vec<Pane>],
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
        assert_eq!(workspaces.open(&mut reactor, "/", &tabs), Ok(3));
        assert_eq!(reactor.session.tabs().len(), 3);
        assert_eq!(reactor.session.active(), 1);

        // Attaching focuses the workspace's first tab and opens nothing.
        reactor.session.switch_tab(0).unwrap();
        assert_eq!(workspaces.open(&mut reactor, "/usr/..", &tabs), Ok(0));
        assert_eq!(
            (reactor.session.tabs().len(), reactor.session.active()),
            (3, 1)
        );

        // Once the person closes them, it opens again.
        for pane in [mine + 1, mine + 2, mine + 3] {
            assert!(reactor.close_pane(pane));
        }
        assert_eq!(workspaces.open(&mut reactor, "/", &tabs), Ok(3));
    }

    #[test]
    fn a_bad_root_cwd_shape_or_count_refuses_and_opens_nothing() {
        let mut reactor = Reactor::new().unwrap();
        reactor.shell = waiting;
        let mut workspaces = Workspaces::default();
        let refused = |workspaces: &mut Workspaces, reactor: &mut Reactor, root, tabs: &[_]| {
            assert!(workspaces.open(reactor, root, tabs).is_err());
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
    }
}
