//! Protected panes, which refuse API writes even with a grant, and sends split at line ends
//! (threat model, Authorization).
//!
//! A pane is protected while any process of its foreground group is `ssh`, `mosh`, `sudo`, `su`
//! or `doas` by name, runs as root, cannot be read or matches a command-line pattern, while
//! the terminal reads a password (echo off), or while it is marked. A send to another pane goes
//! a line at a time. Each next line waits [`LINE_GAP`] and for the pane's own program to hold
//! the foreground again, and is checked just before it is written, so `ssh prod\nrm -rf ~\n`
//! cannot land its second line in ssh.

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use pinfold_sys::foreground::{self, Process};

use crate::io::Reactor;

/// The least time between the lines of a split send.
pub const LINE_GAP: Duration = Duration::from_millis(150);
/// How long a split send waits for the pane's own program to hold the foreground again.
pub const SHELL_WAIT: Duration = Duration::from_secs(10);
/// How often a waiting split send checks the pane.
const RECHECK: Duration = Duration::from_millis(50);

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
    /// A command-line pattern, a process that could not be read, or for a split send, a
    /// foreground that did not return to the pane's own program in time.
    Rule,
    /// Not a protection: a later line of a split send found the pane's input queue full.
    Busy,
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
            Self::Busy => "the pane's input is full",
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

/// Where the first line of `text` ends: after `\n` or `\r`, as a terminal maps either to a line
/// end, so `\r\n` is two.
fn line_end(text: &str) -> usize {
    text.find(['\n', '\r']).map_or(text.len(), |at| at + 1)
}

/// Who holds a terminal's foreground.
#[derive(Debug, PartialEq, Eq)]
enum Holder {
    /// The pane's own program, such as its shell.
    Own,
    /// A program the pane's own one started.
    Other,
    /// A group whose processes have all ended, before the shell takes the terminal back.
    Nobody,
}

/// The rest of a split send.
#[derive(Debug)]
struct Pending {
    pane: usize,
    rest: String,
    /// When to look at the pane next.
    check: Instant,
    /// The next line goes no sooner than this.
    after: Instant,
    /// Past this, a foreground that is not the pane's own program drops the rest.
    give_up: Instant,
}

/// The reactor's protection state.
#[derive(Debug)]
pub(crate) struct Protect {
    patterns: Vec<String>,
    marked: Vec<usize>,
    pending: Vec<Pending>,
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
    /// When a split send next looks at its pane.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.pending.iter().map(|pending| pending.check).min()
    }

    /// Whether a split send to `pane` still has lines to go.
    pub(crate) fn is_sending(&self, pane: usize) -> bool {
        self.pending.iter().any(|pending| pending.pane == pane)
    }

    fn refuse(&mut self, pane: usize, reason: Reason) {
        // Bounded, as nothing may take them yet.
        if self.refused.len() < MAX_REFUSED {
            self.refused.push((pane, reason));
        }
    }
}

impl Reactor {
    /// Why a pane refuses API writes now, or nothing if it takes them.
    #[must_use]
    pub fn protection(&self, pane: usize) -> Option<Reason> {
        match self.inspect(pane)? {
            // A group with no live process is not known to be safe either.
            Ok(Holder::Nobody) => Some(Reason::Rule),
            Ok(_) => None,
            Err(reason) => Some(reason),
        }
    }

    /// Nothing for an unknown pane, else who holds its foreground, or why it is protected.
    fn inspect(&self, pane: usize) -> Option<Result<Holder, Reason>> {
        if self.protect.marked.contains(&pane) {
            return Some(Err(Reason::Marked));
        }
        let pty = self.pty(pane)?;
        // A terminal or process that cannot be read is not known to be safe.
        let Ok(found) = foreground::foreground(pty.master().as_fd()) else {
            return Some(Err(Reason::Rule));
        };
        let patterns = &self.protect.patterns;
        if let Some(reason) = found.members.iter().find_map(|m| classify(m, patterns)) {
            return Some(Err(reason));
        }
        if found.echo_off {
            return Some(Err(Reason::EchoOff));
        }
        Some(Ok(if found.members.is_empty() {
            Holder::Nobody
        // The program leads its session, so its group's id is the session's.
        } else if found.group == pty.terminal().session {
            Holder::Own
        } else {
            Holder::Other
        }))
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

    /// Sends `text` to another pane a line at a time, splitting after each `\n` and `\r`.
    /// The first line goes now, as [`Reactor::send`] sends it. Each next one goes once
    /// [`LINE_GAP`] has passed and the pane's own program holds the foreground again, after a
    /// check just before it is written. A check that finds the pane protected, or a foreground
    /// still held by another program after [`SHELL_WAIT`], drops the rest, and
    /// [`Reactor::take_refused`] tells of it. Returns how many lines the text has.
    ///
    /// # Errors
    ///
    /// Refuses, writing nothing, text with control characters other than tab and line ends,
    /// which could end a line or signal the program, and otherwise returns [`Reactor::send`]'s
    /// refusal for the first line.
    pub fn send_lines(
        &mut self,
        pane: usize,
        text: &str,
        now: Instant,
    ) -> Result<usize, &'static str> {
        let control = |byte: u8| byte < 0x20 && !matches!(byte, b'\t' | b'\n' | b'\r');
        if text.bytes().any(control) {
            return Err("the text has control characters");
        }
        let (first, rest) = text.split_at(line_end(text));
        self.send(pane, first, now)?;
        if !rest.is_empty() {
            self.protect.pending.push(Pending {
                pane,
                rest: rest.to_owned(),
                check: now + RECHECK,
                after: now + LINE_GAP,
                give_up: now + LINE_GAP + SHELL_WAIT,
            });
        }
        let (mut lines, mut left) = (1, rest);
        while !left.is_empty() {
            (lines, left) = (lines + 1, &left[line_end(left)..]);
        }
        Ok(lines)
    }

    /// Returns the panes whose split sends a check stopped since the last call, and why.
    pub fn take_refused(&mut self) -> Vec<(usize, Reason)> {
        std::mem::take(&mut self.protect.refused)
    }

    /// Checks each split send that is due, and sends its next line if it may.
    pub(crate) fn send_due(&mut self, now: Instant) {
        let mut pending = std::mem::take(&mut self.protect.pending);
        pending.retain_mut(|send| {
            if send.check > now {
                return true;
            }
            let holder = match self.inspect(send.pane) {
                None => return false,
                Some(Err(reason)) => {
                    self.protect.refuse(send.pane, reason);
                    return false;
                }
                Some(Ok(holder)) => holder,
            };
            if holder != Holder::Own || now < send.after {
                if now >= send.give_up {
                    self.protect.refuse(send.pane, Reason::Rule);
                    return false;
                }
                send.check = now + RECHECK;
                return true;
            }
            let line: String = send.rest.drain(..line_end(&send.rest)).collect();
            (send.check, send.after) = (now + RECHECK, now + LINE_GAP);
            send.give_up = now + LINE_GAP + SHELL_WAIT;
            // A full queue drops the rest, which the caller was told would go, so say so.
            if !self.write(send.pane, line.as_bytes()) {
                self.protect.refuse(send.pane, Reason::Busy);
                return false;
            }
            !send.rest.is_empty()
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

    #[test]
    fn lines_end_at_either_line_end() {
        assert_eq!(line_end("a\rb"), 2);
        assert_eq!(line_end("a\r\nb"), 2);
        assert_eq!(line_end("a\nb"), 2);
        assert_eq!(line_end("ab"), 2);
    }

    /// Wall clock, not `TestClock`: the pane is a real process.
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
        let script = "read a; read b; read c; echo \"got $a $b $c\"; sleep 30";
        let pane = spawn(&mut reactor, script);
        let sent = reactor.send_lines(pane, "one\rtwo\nthree\n", Instant::now());
        assert_eq!(sent, Ok(3));
        let got = |reactor: &mut Reactor| {
            reactor
                .read(pane, None)
                .is_some_and(|text| text.contains("got one two three"))
        };
        assert!(turn_until(&mut reactor, got));
        assert!(reactor.take_refused().is_empty());
    }

    #[test]
    fn a_later_line_that_finds_the_input_full_is_told_of() {
        let mut reactor = Reactor::new().unwrap();
        // Raw, so the kernel queue fills without a line limit, and nothing reads it.
        let pane = spawn(&mut reactor, "stty raw -echo; echo ready; exec sleep 30");
        // A program still starting hides its arguments, which protects the pane, so wait
        // until `sleep` holds the foreground with them readable.
        let started = |reactor: &mut Reactor| {
            let found = foreground::foreground(reactor.pty(pane).unwrap().master().as_fd());
            let sleep = |m: &Process| m.name == "sleep" && m.argv.is_some();
            reactor.read(pane, None).unwrap().contains("ready")
                && found.is_ok_and(|found| found.members.iter().any(sleep))
        };
        assert!(turn_until(&mut reactor, started));
        let now = Instant::now();
        assert_eq!(reactor.send_lines(pane, "a\nb\n", now), Ok(2));
        // Halving chunks, as a byte at a time is a million writes, which on a loaded machine
        // outlast `sleep 30`, and a pane whose program has ended is refused as a rule (#389).
        let mut chunk = crate::io::MAX_INPUT;
        while chunk > 0 {
            if !reactor.write(pane, &vec![b'x'; chunk]) {
                chunk /= 2;
            }
        }
        // The refused byte left the queue exactly full, so the next line is checked now, with
        // no turn between. A turn writes more of the queue to the PTY once Linux has moved
        // what it holds on to the line discipline, and the line then finds room (#389).
        reactor.send_due(now + LINE_GAP);
        assert!(!reactor.protect.is_sending(pane));
        assert_eq!(reactor.take_refused(), vec![(pane, Reason::Busy)]);
        assert_eq!(Reason::Busy.refusal(), "the pane's input is full");
    }

    #[test]
    fn control_characters_refuse_a_split_send_whole() {
        let mut reactor = Reactor::new().unwrap();
        let pane = spawn(&mut reactor, "sleep 30");
        for text in ["echo hi\x04", "a\x1b[A\n", "\x03", "b\x00"] {
            let refused = reactor.send_lines(pane, text, Instant::now());
            assert_eq!(refused, Err("the text has control characters"), "{text:?}");
        }
        assert!(!reactor.protect.is_sending(pane));
        assert_eq!(reactor.send_lines(pane, "a\tb", Instant::now()), Ok(1));
    }

    /// An interactive shell with job control gives the foreground to each command it runs.
    fn interactive(dir: &std::path::Path) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-i").current_dir(dir).env_clear();
        command.envs([("PATH", dir), ("HOME", dir)]);
        command.env("PS1", "ready$ ");
        command
    }

    fn at_prompt(reactor: &mut Reactor, pane: usize) {
        // A person types at a prompt. Typed earlier, both lines would wait for the shell.
        let ready = |reactor: &mut Reactor| reactor.read(pane, None).unwrap().contains("ready$");
        assert!(turn_until(reactor, ready));
    }

    #[test]
    fn a_next_line_waits_for_the_shell_to_hold_the_foreground() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        std::os::unix::fs::symlink("/bin/sleep", dir.join("sleep")).unwrap();
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(interactive(dir), 24, 80).unwrap();
        at_prompt(&mut reactor, pane);

        // Wall clock: `Reactor::turn` sends the next line at `Instant::now()`, after a real sleep.
        let start = Instant::now();
        let sent = reactor.send_lines(pane, "sleep 1\necho after\n", start);
        assert_eq!(sent, Ok(2));
        let done = |reactor: &mut Reactor| !reactor.protect.is_sending(pane);
        assert!(turn_until(&mut reactor, done));
        assert!(
            start.elapsed() >= Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
        let ran = |reactor: &mut Reactor| reactor.read(pane, None).unwrap().contains("\nafter");
        assert!(turn_until(&mut reactor, ran));
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
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let (this, ssh) = (std::env::current_exe().unwrap(), dir.join("ssh"));
        let linked = std::fs::hard_link(&this, &ssh);
        linked
            .or_else(|_| std::fs::copy(&this, &ssh).map(drop))
            .unwrap();
        let mut command = interactive(dir);
        command.env(FAKE_SSH, "1");
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(command, 24, 80).unwrap();
        at_prompt(&mut reactor, pane);

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
    }
}
