//! Which pane spawned which, for the API's `children` scope (ADR-0032 step 5).
//!
//! A caller fully controls the panes it spawned and theirs in turn, and nothing else without a
//! grant (threat model, Authorization). A spawned pane runs the user's shell, never a program
//! the caller names, and goes through the same session code as the window's keys, so a window
//! shows it like any other. It takes neither focus nor the active tab, so the keyboard stays
//! where the person left it.
//!
//! A sandboxed agent and every pane under it start only sandboxed agents in its own working
//! directory, so nothing it spawns runs outside its sandbox (threat model, Sandbox).

use std::path::{Path, PathBuf};
use std::process::Command;

pub use pinfold_proto::link::SplitWay;

use crate::io::Reactor;
use crate::tabs::FIRST_SIZE;

/// Why a pane is out of a caller's reach.
pub const NOT_YOURS: &str = "only the caller's own pane and the panes it spawned can be reached";

/// Why a sandboxed caller may not start a pane.
pub const CONTAINED: &str = "a sandboxed agent and the panes under it start only sandboxed agents, \
                             in its own working directory";

/// Which pane spawned which. Pane numbers are never used again, so an entry cannot come to
/// name another pane.
#[derive(Debug, Default)]
pub struct Family {
    /// `(child, parent)`, one for each spawned pane that is still open. A parent always has the
    /// lower number, as it was there first.
    parents: Vec<(usize, usize)>,
    /// `(pane, working directory)`, one for each open pane that runs under a sandbox.
    sandboxes: Vec<(usize, PathBuf)>,
}

impl Family {
    /// Returns `target` if `caller`, the caller's own pane, may reach it: it is the caller's
    /// own pane, or one the caller spawned, or one they spawned, and so on. A caller in no pane
    /// reaches nothing.
    ///
    /// # Errors
    ///
    /// Returns why not.
    ///
    /// # Examples
    ///
    /// ```
    /// use pinfold_core::family::Family;
    /// use pinfold_core::io::Reactor;
    ///
    /// let reactor = Reactor::new().unwrap();
    /// let mut family = Family::default();
    /// assert_eq!(family.reach(&reactor, Some(0), 0), Ok(0));
    /// assert!(family.reach(&reactor, Some(0), 1).is_err());
    /// assert!(family.reach(&reactor, None, 0).is_err());
    /// ```
    pub fn reach(
        &mut self,
        reactor: &Reactor,
        caller: Option<usize>,
        target: usize,
    ) -> Result<usize, &'static str> {
        self.forget_closed(reactor);
        let caller = caller.ok_or(NOT_YOURS)?;
        let mut pane = target;
        // Each step goes to a lower number, so this ends.
        while pane != caller {
            pane = self.parent(pane).ok_or(NOT_YOURS)?;
        }
        Ok(target)
    }

    /// Opens a pane for `caller`, the caller's own pane: a split of `of`, or with no way a new
    /// tab, running the user's shell in `cwd` if given. The caller then reaches it.
    ///
    /// # Errors
    ///
    /// Returns why not, and changes nothing: `of` is out of reach, `cwd` is not a directory, a
    /// cap is met, there is no room, or the shell will not start.
    pub fn spawn(
        &mut self,
        reactor: &mut Reactor,
        caller: usize,
        of: usize,
        way: Option<SplitWay>,
        cwd: Option<&str>,
    ) -> Result<usize, String> {
        let shell = (reactor.shell)();
        self.spawn_command(reactor, caller, of, way, cwd, shell)
    }

    /// Returns why `caller` may not start a pane that runs under a sandbox writing to `sandbox`,
    /// or under none: `caller`, or a pane above it, is sandboxed, and `sandbox` is not that
    /// sandbox's working directory. Both paths are resolved, as
    /// [`crate::sandbox::working_directory`] gives them. Not a directory inside it: on Linux
    /// the agent could swap one for a symlink between this check and bubblewrap's bind, but it
    /// cannot rename its own working directory or anything above it.
    ///
    /// # Errors
    ///
    /// Returns [`CONTAINED`].
    pub fn contain(
        &mut self,
        reactor: &Reactor,
        caller: usize,
        sandbox: Option<&Path>,
    ) -> Result<(), &'static str> {
        self.forget_closed(reactor);
        let mut pane = Some(caller);
        while let Some(at) = pane {
            let held = self.sandboxes.iter().find(|entry| entry.0 == at);
            if let Some((_, work)) = held
                && sandbox != Some(work.as_path())
            {
                return Err(CONTAINED);
            }
            pane = self.parent(at);
        }
        Ok(())
    }

    /// As [`Family::spawn`], but the pane runs `command`, which must come from the user, such as
    /// an agent profile, and never from the caller.
    ///
    /// # Errors
    ///
    /// As [`Family::spawn`], and [`CONTAINED`] for a sandboxed caller.
    pub fn spawn_command(
        &mut self,
        reactor: &mut Reactor,
        caller: usize,
        of: usize,
        way: Option<SplitWay>,
        cwd: Option<&str>,
        command: Command,
    ) -> Result<usize, String> {
        self.reach(reactor, Some(caller), of)?;
        self.contain(reactor, caller, None)?;
        self.open(reactor, caller, of, way, cwd.map(Path::new), command)
    }

    /// As [`Family::spawn_command`], but `command` runs under a sandbox that writes to `work`,
    /// where it starts. The pane, and every pane under it, is then held to `work`.
    ///
    /// # Errors
    ///
    /// As [`Family::spawn`], and [`CONTAINED`] if [`Family::contain`] refuses `work`.
    pub fn spawn_sandboxed(
        &mut self,
        reactor: &mut Reactor,
        caller: usize,
        of: usize,
        way: Option<SplitWay>,
        work: &Path,
        command: Command,
    ) -> Result<usize, String> {
        self.reach(reactor, Some(caller), of)?;
        self.contain(reactor, caller, Some(work))?;
        let pane = self.open(reactor, caller, of, way, Some(work), command)?;
        self.sandboxes.push((pane, work.to_path_buf()));
        Ok(pane)
    }

    fn open(
        &mut self,
        reactor: &mut Reactor,
        caller: usize,
        of: usize,
        way: Option<SplitWay>,
        cwd: Option<&Path>,
        mut command: Command,
    ) -> Result<usize, String> {
        if let Some(cwd) = cwd {
            if !cwd.is_absolute() || !cwd.is_dir() {
                return Err("cwd must be an existing directory's absolute path".to_owned());
            }
            command.current_dir(cwd);
        }
        let size = reactor.area.unwrap_or(FIRST_SIZE);
        // Focus and the active tab stay, so a caller cannot take the person's keystrokes.
        let pane = match way {
            Some(way) => reactor.split_with(of, command, way, false),
            None => reactor.open_tab_with(command, size.rows, size.cols, false),
        };
        let pane = pane.map_err(|refusal| refusal.to_string())?;
        self.parents.push((pane, caller));
        Ok(pane)
    }

    /// Closes `pane`, which must be one `caller` spawned, or one they spawned. A caller cannot
    /// close its own pane this way.
    ///
    /// # Errors
    ///
    /// Returns why not.
    pub fn close(
        &mut self,
        reactor: &mut Reactor,
        caller: Option<usize>,
        pane: usize,
    ) -> Result<(), &'static str> {
        if caller == Some(pane) {
            return Err("pane.close closes only panes the caller spawned");
        }
        self.reach(reactor, caller, pane)?;
        if !reactor.close_pane(pane) {
            return Err("no tab holds that pane");
        }
        self.forget_closed(reactor);
        Ok(())
    }

    fn parent(&self, child: usize) -> Option<usize> {
        let entry = self.parents.iter().find(|entry| entry.0 == child);
        entry.map(|entry| entry.1)
    }

    /// Drops each closed pane, handing its children to its parent, so a grandchild stays in
    /// reach when the pane between closes, and the list holds only open panes.
    fn forget_closed(&mut self, reactor: &Reactor) {
        let closed = |entry: &(usize, usize)| reactor.is_closed(entry.0);
        while let Some(index) = self.parents.iter().position(closed) {
            let (gone, parent) = self.parents.swap_remove(index);
            for entry in &mut self.parents {
                if entry.1 == gone {
                    entry.1 = parent;
                }
            }
        }
        self.sandboxes.retain(|entry| !reactor.is_closed(entry.0));
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::{Duration, Instant};

    use pinfold_mux::PaneId;
    use pinfold_proto::link::{MAX_PANES, MAX_TABS};

    use super::*;

    fn waiting() -> Command {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            r#"echo "[$(pwd)]"; read line; echo got-$line; read line"#,
        ]);
        command
    }

    /// A reactor whose panes wait for input, with the caller's pane as its one tab.
    fn reactor() -> (Reactor, usize) {
        let mut reactor = Reactor::new().unwrap();
        reactor.shell = waiting;
        let own = reactor.open_tab(waiting(), 24, 80).unwrap();
        (reactor, own)
    }

    fn shows(reactor: &mut Reactor, pane: usize, text: &str) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            if reactor.read(pane, None).unwrap_or_default().contains(text) {
                return true;
            }
            reactor.turn(Some(Duration::from_millis(5))).unwrap();
        }
        false
    }

    #[test]
    fn a_caller_reaches_its_children_and_theirs_and_nothing_else() {
        let (mut reactor, own) = reactor();
        let other = reactor.open_tab(waiting(), 24, 80).unwrap();
        let mut family = Family::default();
        // The person's tab is active, and the caller's tab is behind it.
        let focus = |reactor: &Reactor| {
            let tabs = reactor.session.tabs().iter().map(pinfold_mux::Tab::focused);
            (reactor.session.active(), tabs.take(2).collect::<Vec<_>>())
        };
        let before = focus(&reactor);
        let right = Some(SplitWay::Right);
        let child = family.spawn(&mut reactor, own, own, right, None).unwrap();
        let tab = family
            .spawn(&mut reactor, own, own, None, Some("/"))
            .unwrap();
        // The child's own processes are the child, which spawns a grandchild.
        let down = Some(SplitWay::Down);
        let grandchild = family
            .spawn(&mut reactor, child, child, down, None)
            .unwrap();
        assert_eq!(reactor.session.tabs().len(), 3);
        assert_eq!(focus(&reactor), before);
        assert!(shows(&mut reactor, tab, "[/]"));

        for pane in [own, child, tab, grandchild] {
            assert_eq!(family.reach(&reactor, Some(own), pane), Ok(pane));
        }
        assert!(reactor.send(grandchild, "hi\n", Instant::now()).is_ok());
        assert!(shows(&mut reactor, grandchild, "got-hi"));
        // Not the parent, a sibling, the user's other pane, or anything for no caller.
        for (caller, target) in [(child, own), (child, tab), (tab, child), (own, other)] {
            assert_eq!(family.reach(&reactor, Some(caller), target), Err(NOT_YOURS));
        }
        assert!(family.reach(&reactor, None, own).is_err());
        assert!(family.spawn(&mut reactor, child, own, None, None).is_err());
        assert!(family.close(&mut reactor, Some(child), own).is_err());
        assert!(family.close(&mut reactor, Some(own), own).is_err());
        assert!(family.close(&mut reactor, Some(own), other).is_err());

        // Closing the pane between keeps the grandchild in reach.
        assert_eq!(family.close(&mut reactor, Some(own), child), Ok(()));
        assert_eq!(
            family.reach(&reactor, Some(own), grandchild),
            Ok(grandchild)
        );
        assert_eq!(family.close(&mut reactor, Some(own), grandchild), Ok(()));
        assert!(reactor.is_closed(grandchild) && !reactor.is_closed(other));
        assert_eq!(family.parents, [(tab, own)]);
    }

    #[test]
    fn a_sandboxed_pane_and_those_under_it_start_only_sandboxes_in_its_own_directory() {
        let (mut reactor, own) = reactor();
        let mut family = Family::default();
        // Real directories, as a pane starts in its working directory.
        let work = Path::new("/usr");
        let boxed =
            |family: &mut Family, reactor: &mut Reactor, caller, work: Option<&Path>| match work {
                Some(work) => {
                    family.spawn_sandboxed(reactor, caller, caller, None, work, waiting())
                }
                None => family.spawn_command(reactor, caller, caller, None, None, waiting()),
            };
        // An unsandboxed caller is as before.
        let agent = boxed(&mut family, &mut reactor, own, Some(work)).unwrap();
        assert!(family.spawn(&mut reactor, own, own, None, None).is_ok());

        let refused = Err(CONTAINED.to_owned());
        assert_eq!(
            family.spawn(&mut reactor, agent, agent, None, None),
            refused
        );
        let others = ["/", "/usrx", "/usr/bin"].map(|other| Some(Path::new(other)));
        for other in [None].into_iter().chain(others) {
            assert_eq!(boxed(&mut family, &mut reactor, agent, other), refused);
        }
        // A sandbox in its own directory may start, and is held to it.
        let child = boxed(&mut family, &mut reactor, agent, Some(work)).unwrap();
        assert_eq!(
            family.spawn(&mut reactor, child, child, None, None),
            refused
        );
        assert!(boxed(&mut family, &mut reactor, child, Some(work)).is_ok());
        // Still held once the sandboxed pane above has closed.
        assert_eq!(family.close(&mut reactor, Some(own), agent), Ok(()));
        assert_eq!(
            family.spawn(&mut reactor, child, child, None, None),
            refused
        );
    }

    #[test]
    fn a_bad_cwd_or_a_cap_refuses_and_changes_nothing() {
        let (mut reactor, own) = reactor();
        let mut family = Family::default();
        for cwd in ["relative", "/no/such/place", "/etc/hosts"] {
            assert!(
                family
                    .spawn(&mut reactor, own, own, None, Some(cwd))
                    .is_err()
            );
        }
        assert_eq!(reactor.pane_count(), 1);
        // Focus and the active tab stay with the person.
        let right = Some(SplitWay::Right);
        family.spawn(&mut reactor, own, own, right, None).unwrap();
        for _ in 1..MAX_TABS {
            family.spawn(&mut reactor, own, own, None, None).unwrap();
        }
        let tabs = family.spawn(&mut reactor, own, own, None, None);
        assert_eq!(
            tabs,
            Err("This session has as many tabs as it can hold".to_owned())
        );
        let focused =
            |reactor: &Reactor| reactor.session.active_tab().map(pinfold_mux::Tab::focused);
        assert_eq!(
            (reactor.session.active(), focused(&reactor)),
            (0, Some(own as PaneId))
        );
        // Each other tab's pane splits once, which fills the session.
        let tabs = reactor.session.tabs().get(1..).unwrap_or_default();
        let firsts: Vec<_> = tabs.iter().map(|tab| tab.focused() as usize).collect();
        for pane in firsts {
            family.spawn(&mut reactor, own, pane, right, None).unwrap();
        }
        let panes = family.spawn(&mut reactor, own, own, right, None);
        assert_eq!(
            panes,
            Err("This session has as many panes as it can hold".to_owned())
        );
        assert_eq!(family.parents.len(), MAX_PANES - 1);
    }
}
