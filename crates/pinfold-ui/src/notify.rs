//! Desktop notifications that an agent pane needs you or hit an error (ADR-0046, ADR-0051). The
//! text is PinFold's, "`<tab>` needs you" or "`<tab>` error", never what a program printed, and
//! they are rate-limited per pane and across all of them. macOS bounces the Dock and shows a
//! banner; Linux runs `notify-send` when there is one. With `[alerts] program_notifications` on,
//! a program's own notification comes this way too, as "`<tab>` says: `<text>`", under the same
//! limits (ADR-0051 §9).

#[cfg(any(test, not(target_os = "macos")))]
use std::ffi::OsStr;
#[cfg(any(test, not(target_os = "macos")))]
use std::io::{self, Write as _};
#[cfg(any(test, not(target_os = "macos")))]
use std::path::{Path, PathBuf};
#[cfg(any(test, not(target_os = "macos")))]
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::top_bar::STATES;

/// The least time between two notifications for one pane.
pub const PER_PANE: Duration = Duration::from_secs(60);
/// The least time between any two notifications.
pub const ANY: Duration = Duration::from_secs(10);

/// When notifications were last raised, in all and for each pane within the last minute.
#[derive(Default)]
struct Limit {
    last: Option<Instant>,
    panes: Vec<(u32, Instant)>,
}

impl Limit {
    /// Whether `pane` may notify at `now`, and records it if so. One a pane a minute and one
    /// in 10 s keep at most 6 entries.
    fn allow(&mut self, pane: u32, now: Instant) -> bool {
        let soon = |last: Instant, gap| now.saturating_duration_since(last) < gap;
        let held = self.panes.iter().position(|(at, _)| *at == pane);
        if self.last.is_some_and(|last| soon(last, ANY))
            || held.is_some_and(|held| soon(self.panes[held].1, PER_PANE))
        {
            return false;
        }
        self.last = Some(now);
        self.panes.retain(|(_, last)| soon(*last, PER_PANE));
        self.panes.push((pane, now));
        true
    }
}

/// Raises the notifications, through the platform where it has a way.
pub struct Notifier {
    limit: Limit,
    #[cfg(target_os = "macos")]
    system: Option<pinfold_sys::notify::Notifier>,
    #[cfg(not(target_os = "macos"))]
    system: Sender,
}

impl Notifier {
    /// Sets up the platform's notifications. `clicked` gets the pane whose notification was
    /// clicked.
    pub fn new(clicked: impl Fn(u32) + 'static) -> Self {
        // `notify-send` reports no clicks, which ADR-0051 leaves out on Linux in v1.
        #[cfg(not(target_os = "macos"))]
        drop(clicked);
        #[cfg(target_os = "macos")]
        let clicked = move |pane: usize| {
            if let Ok(pane) = u32::try_from(pane) {
                clicked(pane);
            }
        };
        Self {
            limit: Limit::default(),
            #[cfg(target_os = "macos")]
            system: pinfold_sys::notify::Notifier::new(clicked),
            // Tests never reach the desktop's `notify-send`; `Sender`'s own run a stub.
            #[cfg(not(target_os = "macos"))]
            system: Sender::new(if cfg!(test) {
                None
            } else {
                Sender::find(std::env::var_os("PATH").as_deref())
            }),
        }
    }

    /// Notifies that `pane`, in the tab at index `tab` labelled `label`, came to `state`, unless
    /// the limits hold it back. The caller has checked that the config asks for this state and
    /// that the person cannot see the pane. `label` is already safe to draw (`top_bar::label`).
    pub fn alert(
        &mut self,
        (pane, state): (u32, u8),
        (tab, label): (Option<usize>, Option<&str>),
        now: Instant,
    ) {
        if self.limit.allow(pane, now) {
            self.show(pane, &text(state, tab, label), now);
        }
    }

    /// Shows a program notification for `pane`, which the person cannot see, as
    /// "`<label>` says: `<said>`", unless the limits alerts share hold it back. `said` is the
    /// core's cleaned and cut text; `label` is already safe to draw.
    pub fn toast(
        &mut self,
        pane: u32,
        (tab, label): (Option<usize>, Option<&str>),
        said: &str,
        now: Instant,
    ) {
        if self.limit.allow(pane, now) {
            self.show(pane, &format!("{} {said}", saying(tab, label)), now);
        }
    }

    /// Raises `text` through the platform. macOS reports clicks by `pane`; Linux runs one
    /// `notify-send` at a time from `now`.
    fn show(&mut self, pane: u32, text: &str, now: Instant) {
        #[cfg(target_os = "macos")]
        {
            let _ = now;
            if let Some(system) = &self.system {
                system.notify(text, pane as usize);
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = pane;
            self.system.send(text, now);
        }
    }

    /// Returns how many panes notified within the last minute.
    #[cfg(test)]
    pub(crate) fn raised(&self) -> usize {
        self.limit.panes.len()
    }
}

/// How long a `notify-send` may run before the next notification due kills it.
#[cfg(any(test, not(target_os = "macos")))]
const WAIT: Duration = Duration::from_secs(10);

/// The only variables `notify-send` gets, which it needs to reach the session's bus.
#[cfg(any(test, not(target_os = "macos")))]
const KEPT: [&str; 4] = [
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
];

/// Runs `notify-send` for each notification, one at a time, never through a shell (ADR-0051).
#[cfg(any(test, not(target_os = "macos")))]
struct Sender {
    program: Option<PathBuf>,
    running: Option<(Child, Instant)>,
}

#[cfg(any(test, not(target_os = "macos")))]
impl Sender {
    /// The first executable `notify-send` in an absolute entry of `path`, a `PATH` value.
    fn find(path: Option<&OsStr>) -> Option<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        std::env::split_paths(path?)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join("notify-send"))
            .find(|program| {
                std::fs::metadata(program)
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            })
    }

    /// Sends through `program`; without one, says once that alerts stay on the tab.
    fn new(program: Option<PathBuf>) -> Self {
        if program.is_none() && !cfg!(test) {
            let _ = writeln!(
                io::stderr(),
                "pinfold: no notify-send on PATH, so alerts show only on the tab"
            );
        }
        Self {
            program,
            running: None,
        }
    }

    /// Shows `text`, unless a `notify-send` still runs: then `text` is dropped, and the one
    /// running is killed if it is past `WAIT`. Never waits on a child that has not exited.
    /// Returns whether it started one.
    fn send(&mut self, text: &str, now: Instant) -> bool {
        if let Some((child, started)) = &mut self.running {
            let past = now.saturating_duration_since(*started) >= WAIT;
            match child.try_wait() {
                Ok(None) if !past => return false,
                Ok(None) => {
                    stop(child);
                    self.running = None;
                    return false;
                }
                _ => self.running = None,
            }
        }
        let Some(program) = self.program.as_deref() else {
            return false;
        };
        match pinfold_sys::spawn_detached(command(program, text), Stdio::null()) {
            Ok(child) => {
                self.running = Some((child, now));
                true
            }
            Err(error) => {
                let _ = writeln!(
                    io::stderr(),
                    "pinfold: cannot run {}, so alerts show only on the tab: {error}",
                    program.display()
                );
                self.program = None;
                false
            }
        }
    }
}

#[cfg(any(test, not(target_os = "macos")))]
impl Drop for Sender {
    fn drop(&mut self) {
        if let Some((child, _)) = &mut self.running {
            stop(child);
        }
    }
}

/// `notify-send` with fixed arguments and `text`, PinFold's own, last after `--`, so a label
/// that starts with `-` is still the summary. Its environment is cleared but for `KEPT`.
#[cfg(any(test, not(target_os = "macos")))]
fn command(program: &Path, text: &str) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .envs(
            KEPT.iter()
                .filter_map(|key| Some((key, std::env::var_os(key)?))),
        )
        .args(["--app-name=PinFold", "--expire-time=10000", "--", text]);
    command
}

/// Kills `child` and reaps it, which is at once after `SIGKILL`.
#[cfg(any(test, not(target_os = "macos")))]
fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Who a toast is from, "`<label>` says:", by the tab's label or else its number (ADR-0051 §9).
pub(crate) fn saying(tab: Option<usize>, label: Option<&str>) -> String {
    match (label.filter(|label| !label.is_empty()), tab) {
        (Some(label), _) => format!("{label} says:"),
        (None, Some(tab)) => format!("Tab {} says:", tab + 1),
        (None, None) => "A program says:".to_owned(),
    }
}

/// A notification's text: the tab's label, or its number without one, and the state's word.
/// Nothing a program printed.
fn text(state: u8, tab: Option<usize>, label: Option<&str>) -> String {
    let word = STATES
        .get(usize::from(state))
        .map_or("needs you", |(_, word)| word);
    match (label.filter(|label| !label.is_empty()), tab) {
        (Some(label), _) => format!("{label} {word}"),
        (None, Some(tab)) => format!("Tab {} {word}", tab + 1),
        (None, None) => format!("An agent {word}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::top_bar::{ERROR, NEEDS_YOU};

    #[test]
    fn the_text_is_pinfolds_own_word_and_label() {
        assert_eq!(text(NEEDS_YOU, Some(0), Some("api")), "api needs you");
        assert_eq!(text(ERROR, Some(0), Some("api")), "api error");
        assert_eq!(text(ERROR, Some(2), Some("")), "Tab 3 error");
        assert_eq!(text(NEEDS_YOU, Some(0), None), "Tab 1 needs you");
        assert_eq!(text(ERROR, None, None), "An agent error");
        assert_eq!(
            text(200, None, Some("api")),
            "api needs you",
            "a state out of range"
        );
    }

    #[test]
    fn a_flood_of_alerts_notifies_once_a_pane_a_minute_and_once_in_ten_seconds() {
        let now = Instant::now();
        let mut limit = Limit::default();
        let mut raised = Vec::new();
        // Ten panes flap needs you and error every millisecond for two minutes.
        for step in 0_u32..120_000 {
            let at = now + Duration::from_millis(u64::from(step));
            if limit.allow(step % 10, at) {
                raised.push(step);
            }
            assert!(limit.panes.len() <= 6);
        }
        // One every 10 s over 120 s, and each pane at most twice.
        assert_eq!(raised.len(), 12);
        for pane in 0..10 {
            assert!(raised.iter().filter(|step| *step % 10 == pane).count() <= 2);
        }
        let mut alone = Limit::default();
        assert!(alone.allow(7, now));
        assert!(!alone.allow(7, now + ANY), "inside the pane's minute");
        assert!(alone.allow(8, now + ANY), "another pane, keyed apart");
        assert!(alone.allow(7, now + PER_PANE));
    }

    /// A fresh folder for a `notify-send` test.
    fn folder(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pinfold-notify-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stub `notify-send` in `dir` that appends each run's arguments, one `[arg]` a line, and
    /// whether it got `HOME`, to `dir/runs`, then runs `then`.
    fn stub(dir: &Path, then: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let program = dir.join("notify-send");
        let runs = dir.join("runs");
        let script = format!(
            "#!/bin/sh\nfor arg in \"$@\"; do printf '[%s]\\n' \"$arg\"; done >> '{runs}'\n\
             printf 'HOME=%s\\n' \"${{HOME-unset}}\" >> '{runs}'\n{then}\n",
            runs = runs.display()
        );
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    /// What `dir/runs` holds once `count` runs have written it.
    fn runs(dir: &Path, count: usize) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let runs = std::fs::read_to_string(dir.join("runs")).unwrap_or_default();
            if runs.matches("HOME=").count() >= count || Instant::now() > deadline {
                return runs;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn notify_send_gets_fixed_arguments_then_the_text_as_one_inert_argument() {
        let dir = folder("argv");
        let mut sender = Sender::new(Some(stub(&dir, "")));
        let pwned = dir.join("pwned");
        let hostile = format!("--icon=/x\n$(: > '{}')\u{7}\u{1b}[2J`id`;", pwned.display());
        assert!(sender.send(&text(NEEDS_YOU, Some(0), Some(&hostile)), Instant::now()));
        let expected = format!(
            "[--app-name=PinFold]\n[--expire-time=10000]\n[--]\n[{hostile} needs you]\nHOME=unset\n"
        );
        assert_eq!(
            runs(&dir, 1),
            expected,
            "argv exact, the environment cleared"
        );
        assert!(!pwned.exists(), "never through a shell");
        drop(sender);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn one_notify_send_runs_at_a_time_and_one_past_the_wait_is_killed() {
        let dir = folder("one");
        let now = Instant::now();
        let mut sender = Sender::new(Some(stub(&dir, "exec /bin/sleep 30")));
        assert!(sender.send("first", now));
        runs(&dir, 1);
        assert!(
            !sender.send("second", now + ANY / 2),
            "dropped while one runs"
        );
        assert!(
            !sender.send("third", now + WAIT),
            "dropped, and the hung one killed"
        );
        assert!(sender.running.is_none(), "reaped");
        assert!(sender.send("fourth", now + WAIT));
        let runs = runs(&dir, 2);
        assert!(runs.contains("[first]") && runs.contains("[fourth]"));
        assert!(
            !runs.contains("[second]") && !runs.contains("[third]"),
            "not queued"
        );
        drop(sender);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn notify_send_is_found_only_as_an_executable_on_an_absolute_path_entry() {
        use std::os::unix::fs::PermissionsExt;
        let dir = folder("find");
        let (plain, bin) = (dir.join("plain"), dir.join("bin"));
        std::fs::create_dir(&plain).unwrap();
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(plain.join("notify-send"), "").unwrap();
        std::fs::set_permissions(
            plain.join("notify-send"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let program = stub(&bin, "");
        let path = |entries: &[&Path]| std::env::join_paths(entries).unwrap();
        assert_eq!(Sender::find(None), None);
        assert_eq!(
            Sender::find(Some(&path(&[&dir.join("none"), &plain]))),
            None
        );
        assert_eq!(
            Sender::find(Some(&path(&[Path::new("bin")]))),
            None,
            "relative"
        );
        assert_eq!(Sender::find(Some(&path(&[&plain, &bin]))), Some(program));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn without_notify_send_an_alert_only_counts_toward_the_tab() {
        let mut sender = Sender::new(None);
        assert!(!sender.send("api needs you", Instant::now()));
        let missing = folder("missing").join("notify-send");
        let mut sender = Sender::new(Some(missing));
        assert!(
            !sender.send("api needs you", Instant::now()),
            "cannot start"
        );
        assert!(sender.program.is_none(), "given up after the one log line");
        let mut notifier = Notifier::new(|_| ());
        notifier.alert((7, NEEDS_YOU), (Some(0), Some("api")), Instant::now());
        assert_eq!(notifier.raised(), 1);
    }
}
