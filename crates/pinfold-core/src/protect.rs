//! Protected panes, which refuse API writes even with a grant, and sends split at newlines
//! (threat model, Authorization).
//!
//! A pane is protected while any process of its foreground group is `ssh`, `mosh`, `sudo`, `su`
//! or `doas` by name, runs as root, cannot be read or matches a command-line pattern, while
//! the terminal reads a password (echo off), or while it is marked. A send to another pane goes a line at a time, with the check made again
//! [`LINE_GAP`] after each line, so `ssh prod\nrm -rf ~\n` cannot land its second line in ssh.

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use pinfold_sys::foreground::{self, Process};

use crate::io::Reactor;

/// How long a split send waits after a line before it checks the pane again.
pub const LINE_GAP: Duration = Duration::from_millis(150);

/// Command-line patterns used until config names others: shells into other machines,
/// containers and databases.
pub const DEFAULT_PATTERNS: [&str; 4] = ["kubectl exec", "docker exec", "psql", "aws ssm"];
/// Most stopped sends held for [`Reactor::take_refused`]. Later ones are not told of.
const MAX_REFUSED: usize = 64;

/// Why a pane is protected. The word names the rule, never the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// `ssh`, `mosh` or `mosh-client`.
    Ssh,
    /// `sudo`, `su` or `doas`.
    Sudo,
    /// A process acting as root.
    Root,
    /// Echo off with input in lines: a password prompt.
    EchoOff,
    /// Marked by the person.
    Marked,
    /// A command-line pattern, or a process that could not be read.
    Rule,
}

impl Reason {
    /// The error a refused send gets.
    #[must_use]
    pub fn refusal(self) -> &'static str {
        match self {
            Self::Ssh => "the pane is protected: ssh",
            Self::Sudo => "the pane is protected: sudo",
            Self::Root => "the pane is protected: root",
            Self::EchoOff => "the pane is protected: echo-off",
            Self::Marked => "the pane is protected: marked",
            Self::Rule => "the pane is protected: rule",
        }
    }
}

/// Why `process` protects its pane, if it does. A command line that cannot be read protects it.
///
/// # Examples
///
/// ```
/// use pinfold_core::protect::{Reason, classify};
/// use pinfold_sys::foreground::Process;
///
/// let found = Process { name: "ssh".into(), argv: None, euid: 501 };
/// assert_eq!(classify(&found, &[]), Some(Reason::Ssh));
/// ```
#[must_use]
pub fn classify(process: &Process, patterns: &[String]) -> Option<Reason> {
    match process.name.as_str() {
        "ssh" | "mosh" | "mosh-client" => return Some(Reason::Ssh),
        "sudo" | "su" | "doas" => return Some(Reason::Sudo),
        _ => {}
    }
    if process.euid == 0 {
        return Some(Reason::Root);
    }
    let matched = process
        .argv
        .as_ref()
        .is_none_or(|argv| patterns.iter().any(|pattern| matches(pattern, argv)));
    matched.then_some(Reason::Rule)
}

/// Whether a pattern's first word is the program's file name and each next word is a later
/// argument, in order.
fn matches(pattern: &str, argv: &[String]) -> bool {
    let mut words = pattern.split_whitespace();
    let (Some(program), Some((first, rest))) = (words.next(), argv.split_first()) else {
        return false;
    };
    let mut rest = rest.iter();
    first.rsplit('/').next() == Some(program) && words.all(|word| rest.any(|arg| arg == word))
}

/// The reactor's protection state.
#[derive(Debug)]
pub(crate) struct Protect {
    patterns: Vec<String>,
    marked: Vec<usize>,
    /// The rest of each split send, and when its next line is due.
    pending: Vec<(usize, String, Instant)>,
    refused: Vec<(usize, Reason)>,
}

impl Default for Protect {
    fn default() -> Self {
        Self {
            patterns: DEFAULT_PATTERNS.map(str::to_owned).to_vec(),
            marked: Vec::new(),
            pending: Vec::new(),
            refused: Vec::new(),
        }
    }
}

impl Protect {
    /// When the next line of a split send is due.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.pending.iter().map(|(_, _, due)| *due).min()
    }

    /// Whether a split send to `pane` still has lines to go.
    pub(crate) fn is_sending(&self, pane: usize) -> bool {
        self.pending.iter().any(|(to, _, _)| *to == pane)
    }
}

impl Reactor {
    /// Why a pane refuses API writes now, or nothing if it takes them.
    #[must_use]
    pub fn protection(&self, pane: usize) -> Option<Reason> {
        if self.protect.marked.contains(&pane) {
            return Some(Reason::Marked);
        }
        // A terminal or process that cannot be read, or a group with no live process, is not
        // known to be safe.
        let Ok(found) = foreground::foreground(self.master(pane)?.as_fd()) else {
            return Some(Reason::Rule);
        };
        if found.members.is_empty() {
            return Some(Reason::Rule);
        }
        let patterns = &self.protect.patterns;
        let reason = found.members.iter().find_map(|m| classify(m, patterns));
        reason.or(found.echo_off.then_some(Reason::EchoOff))
    }

    /// Marks a pane protected, or clears the mark.
    pub fn set_marked(&mut self, pane: usize, marked: bool) {
        self.protect.marked.retain(|&number| number != pane);
        if marked {
            self.protect.marked.push(pane);
        }
    }

    /// Replaces the command-line patterns. A pattern's first word must be the file name of the
    /// program, and each next word a later argument, in order. [`DEFAULT_PATTERNS`] apply until
    /// this is called.
    pub fn set_patterns(&mut self, patterns: Vec<String>) {
        self.protect.patterns = patterns;
    }

    /// Sends `text` to another pane a line at a time: the first now, as [`Reactor::send`] does,
    /// and each next one [`LINE_GAP`] later if the pane is still unprotected. Returns how many
    /// lines the text has. A line the check stops is dropped with the rest, and
    /// [`Reactor::take_refused`] tells of it.
    ///
    /// # Errors
    ///
    /// Returns [`Reactor::send`]'s refusal for the first line.
    pub fn send_lines(
        &mut self,
        pane: usize,
        text: &str,
        now: Instant,
    ) -> Result<usize, &'static str> {
        let (first, rest) = text.split_at(text.find('\n').map_or(text.len(), |at| at + 1));
        self.send(pane, first, now)?;
        if !rest.is_empty() {
            self.protect
                .pending
                .push((pane, rest.to_owned(), now + LINE_GAP));
        }
        Ok(text.split_inclusive('\n').count())
    }

    /// Returns the panes whose split sends a check stopped since the last call, and why.
    pub fn take_refused(&mut self) -> Vec<(usize, Reason)> {
        std::mem::take(&mut self.protect.refused)
    }

    /// Sends the next line of each split send that is due.
    pub(crate) fn send_due(&mut self, now: Instant) {
        let mut pending = std::mem::take(&mut self.protect.pending);
        pending.retain_mut(|(pane, rest, due)| {
            if *due > now {
                return true;
            }
            if let Some(reason) = self.protection(*pane) {
                // Bounded, as nothing may take them yet.
                if self.protect.refused.len() < MAX_REFUSED {
                    self.protect.refused.push((*pane, reason));
                }
                return false;
            }
            let at = rest.find('\n').map_or(rest.len(), |at| at + 1);
            let line: String = rest.drain(..at).collect();
            *due = now + LINE_GAP;
            // A full queue drops the rest, as a refused send would.
            self.write(*pane, line.as_bytes()) && !rest.is_empty()
        });
        self.protect.pending.append(&mut pending);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn process(name: &str, argv: &[&str], euid: u32) -> Process {
        let argv = Some(argv.iter().map(|arg| (*arg).to_owned()).collect());
        Process {
            name: name.to_owned(),
            argv,
            euid,
        }
    }

    #[test]
    fn each_rule_has_its_word() {
        let patterns = vec!["kubectl exec".to_owned()];
        let check = |found: Process| classify(&found, &patterns);
        assert_eq!(
            check(process("ssh", &["ssh", "prod"], 501)),
            Some(Reason::Ssh)
        );
        assert_eq!(check(process("mosh-client", &[], 501)), Some(Reason::Ssh));
        assert_eq!(check(process("doas", &[], 0)), Some(Reason::Sudo));
        assert_eq!(check(process("bash", &["bash"], 0)), Some(Reason::Root));
        let kubectl = process("kubectl", &["kubectl", "exec", "-it", "db"], 501);
        assert_eq!(check(kubectl), Some(Reason::Rule));
        let unread = Process {
            argv: None,
            ..process("bash", &[], 501)
        };
        assert_eq!(check(unread), Some(Reason::Rule));
        assert_eq!(check(process("bash", &["bash", "-l"], 501)), None);
        assert_eq!(Reason::EchoOff.refusal(), "the pane is protected: echo-off");
    }

    #[test]
    fn patterns_match_words_not_text() {
        let argv = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let kubectl = |line: &str| matches("kubectl exec", &argv(line));
        assert!(kubectl("kubectl -n x exec pod"));
        assert!(kubectl("/usr/local/bin/kubectl exec pod"));
        assert!(!kubectl("kubectl get exec-logs"));
        assert!(!kubectl("/tmp/notkubectl exec"));
        assert!(!kubectl("kubectl"));
        assert!(matches(
            "aws ssm start-session",
            &argv("aws --x ssm start-session")
        ));
        assert!(!matches(
            "aws ssm start-session",
            &argv("aws start-session ssm")
        ));
        assert!(!matches("", &argv("anything")));
    }

    fn turn_until(reactor: &mut Reactor, done: impl Fn(&mut Reactor) -> bool) -> bool {
        let start = Instant::now();
        while !done(reactor) && start.elapsed() < Duration::from_secs(10) {
            reactor.turn(Some(Duration::from_millis(20))).unwrap();
        }
        done(reactor)
    }

    fn spawn(reactor: &mut Reactor, script: &str) -> usize {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        reactor.spawn(command, 24, 80).unwrap()
    }

    #[test]
    fn echo_off_patterns_and_marks_refuse_a_send() {
        let mut reactor = Reactor::new().unwrap();
        let reading = spawn(&mut reactor, "stty -echo; read secret; sleep 30");
        let protected = |reactor: &mut Reactor| reactor.protection(reading).is_some();
        assert!(turn_until(&mut reactor, protected));
        assert_eq!(reactor.protection(reading), Some(Reason::EchoOff));
        let refused = reactor.send(reading, "hunter2\n", Instant::now());
        assert_eq!(refused, Err("the pane is protected: echo-off"));

        let sleeping = spawn(&mut reactor, "exec sleep 31");
        reactor.set_patterns(vec!["sleep 31".to_owned()]);
        let matched = |reactor: &mut Reactor| reactor.protection(sleeping).is_some();
        assert!(turn_until(&mut reactor, matched));
        assert_eq!(reactor.protection(sleeping), Some(Reason::Rule));
        reactor.set_patterns(Vec::new());
        assert_eq!(reactor.protection(sleeping), None);
        reactor.set_marked(sleeping, true);
        let refused = reactor.send(sleeping, "x", Instant::now());
        assert_eq!(refused, Err("the pane is protected: marked"));
        reactor.set_marked(sleeping, false);
        assert_eq!(reactor.send(sleeping, "x", Instant::now()), Ok(()));
    }

    #[test]
    fn a_process_anywhere_in_the_group_protects_the_pane() {
        let mut reactor = Reactor::new().unwrap();
        // Without job control, the background job shares the shell's group.
        let pane = spawn(&mut reactor, "sleep 33 & wait");
        reactor.set_patterns(vec!["sleep 33".to_owned()]);
        let matched = |reactor: &mut Reactor| reactor.protection(pane).is_some();
        assert!(turn_until(&mut reactor, matched));
        assert_eq!(reactor.protection(pane), Some(Reason::Rule));
    }

    #[test]
    fn a_split_send_sends_every_line_to_a_shell() {
        let mut reactor = Reactor::new().unwrap();
        let pane = spawn(&mut reactor, "read a; read b; echo \"got $a $b\"; sleep 30");
        assert_eq!(
            reactor.send_lines(pane, "one\ntwo\n", Instant::now()),
            Ok(2)
        );
        let got = |reactor: &mut Reactor| {
            reactor
                .read(pane, None)
                .is_some_and(|text| text.contains("got one two"))
        };
        assert!(turn_until(&mut reactor, got));
        assert!(reactor.take_refused().is_empty());
    }

    const FAKE_SSH: &str = "PINFOLD_TEST_FAKE_SSH";

    /// Not a test of its own: the fake `ssh` the next test types `ssh prod` to run. This binary
    /// is linked as `ssh`, which takes `prod` as a filter, so the name must hold `prod`. It sits
    /// with input echoed, as ssh does, so a line that landed would show.
    #[test]
    fn fake_ssh_to_prod() {
        if std::env::var_os(FAKE_SSH).is_some() {
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    /// The issue's test. A copy of bash or a script would be named bash on macOS, so `ssh` is
    /// this test binary. `PATH` holds only it, so the second line could not run `rm` even if
    /// it landed.
    #[test]
    fn a_second_line_never_lands_in_ssh() {
        let dir = std::env::temp_dir().join(format!("pinfold-protect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (this, ssh) = (std::env::current_exe().unwrap(), dir.join("ssh"));
        let linked = std::fs::hard_link(&this, &ssh);
        linked
            .or_else(|_| std::fs::copy(&this, &ssh).map(drop))
            .unwrap();
        let mut command = Command::new("/bin/sh");
        command.arg("-i").current_dir(&dir).env_clear();
        command.envs([("PATH", &dir), ("HOME", &dir)]);
        command.envs([(FAKE_SSH, "1"), ("PS1", "ready$ ")]);
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(command, 24, 80).unwrap();
        // A person types at a prompt. Typed earlier, both lines would wait for the shell.
        let ready = |reactor: &mut Reactor| reactor.read(pane, None).unwrap().contains("ready$");
        assert!(turn_until(&mut reactor, ready));

        let sent = reactor.send_lines(pane, "ssh prod\nrm -rf ~\n", Instant::now());
        assert_eq!(sent, Ok(2));
        let stopped = |reactor: &mut Reactor| !reactor.protect.is_sending(pane);
        assert!(turn_until(&mut reactor, stopped));
        assert_eq!(reactor.take_refused(), vec![(pane, Reason::Ssh)]);
        let refused = reactor.send(pane, "rm -rf ~\n", Instant::now());
        assert_eq!(refused, Err("the pane is protected: ssh"));
        reactor.turn(Some(Duration::from_millis(100))).unwrap();
        let text = reactor.read(pane, None).unwrap();
        assert!(
            text.contains("ssh prod") && !text.contains("rm -rf"),
            "{text}"
        );
        drop(reactor);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
