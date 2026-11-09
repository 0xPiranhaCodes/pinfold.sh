//! A sandboxed agent, run with the real `sandbox-exec` or bubblewrap, cannot read hidden
//! directories or another pane's terminal or type into its own, writes only to its working
//! directory but not its git hooks, reaches no Unix socket but the API's, nor on Linux with
//! Landlock ABI 6 an abstract one, and keeps the caller identity the API gives its pane. On Linux
//! it commits into its shadow git directory, whose hooks and config the user's fetch never runs.

use std::fs;
use std::io::{ErrorKind, Read};
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use pinfold_core::sandbox;
use pinfold_sys::pty::Pty;

/// The `PATH` launchd gives an app.
const LAUNCHD_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

#[test]
fn a_sandboxed_agent_cannot_read_hidden_paths_and_keeps_its_identity() {
    // Not under the temporary directory, which the agent may write to.
    let home = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("pinfold-sandbox-{}", std::process::id()));
    let ssh = home.join(".ssh");
    fs::create_dir_all(&ssh).unwrap();
    fs::write(ssh.join("id_test"), "SECRET-KEY\n").unwrap();
    let state = home.join("state");
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("state"), "SECRET-STATE\n").unwrap();
    fs::write(home.join("open"), "VISIBLE\n").unwrap();
    let work = home.join("work");
    fs::create_dir_all(&work).unwrap();
    let init = Command::new("git").args(["init", "-q"]).arg(&work).status();
    assert!(init.unwrap().success());
    // A submodule, whose hooks git runs too.
    let module = work.join(".git/modules/sub");
    fs::create_dir_all(module.join("hooks")).unwrap();
    for file in ["config", "HEAD"] {
        fs::write(module.join(file), "").unwrap();
    }
    fs::create_dir_all(work.join("sub")).unwrap();
    fs::write(work.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
    fs::write(
        work.join(".gitmodules"),
        "[submodule \"sub\"]\n\tpath = sub\n",
    )
    .unwrap();
    let config_before = fs::read(work.join(".git/config")).unwrap();
    for dir in [&home, &work] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    // A socket's path must fit `sun_path`, which a long checkout path under the target overruns.
    // Made here, so the guard removes only what this test created.
    let api_dir = Path::new("/tmp").join(format!("pf{}", std::process::id()));
    fs::create_dir(&api_dir).unwrap();
    let api_dir = Removed(api_dir);
    fs::set_permissions(&api_dir.0, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = api_dir.0.join("api.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    // Other programs' sockets, such as ssh-agent's, where they usually live.
    let name = format!("pinfold-planted-{}.sock", std::process::id());
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let planted: Vec<PathBuf> = [Some(std::env::temp_dir()), runtime]
        .into_iter()
        .flatten()
        .map(|dir| dir.join(&name))
        .collect();
    let _planted: Vec<UnixListener> = planted
        .iter()
        .map(|path| UnixListener::bind(path).unwrap())
        .collect();
    // An abstract socket, as X11 and session buses use, which only Landlock can scope.
    #[cfg(target_os = "linux")]
    let _abstract = {
        use std::os::linux::net::SocketAddrExt;
        let address = std::os::unix::net::SocketAddr::from_abstract_name(&name).unwrap();
        UnixListener::bind_addr(&address).unwrap()
    };

    // Another pane, whose terminal the agent must not open.
    let mut peer = Command::new("/bin/sh");
    peer.args(["-c", "tty; sleep 30"]);
    let peer = Pty::spawn(peer, 24, 80).unwrap();
    let peer_tty = read_until(&peer, "\n");
    let peer_tty = peer_tty.trim();
    assert!(peer_tty.starts_with("/dev/"), "{peer_tty}");
    let tiocsti = if cfg!(target_os = "macos") {
        0x8001_7472_u32
    } else {
        0x5412
    };

    let path = |path: &PathBuf| path.to_str().unwrap().to_owned();
    let (ssh_dir, state_dir, home_dir) = (path(&ssh), path(&state), path(&home));
    let planted_paths: Vec<String> = planted.iter().map(path).collect();
    let planted_paths = planted_paths.join("' '");
    let api = path(&socket);
    // The client stays connected until the core has asked who it is.
    let script = format!(
        "cat '{ssh_dir}/id_test'; cat '{state_dir}/state'; cat '{home_dir}/open'; \
         echo uid=$(id -u); echo ssh=${{SSH_AUTH_SOCK:-unset}}; \
         (echo x >'{home_dir}/.zshrc') 2>/dev/null && echo rc=written || echo rc=refused; \
         echo x >'{home_dir}/work/out' && echo work=written; \
         (echo x >'{home_dir}/work/.git/hooks/pre-commit') 2>/dev/null \
           && echo hook=written || echo hook=refused; \
         (echo x >'{home_dir}/work/.git/modules/sub/hooks/post-checkout') 2>/dev/null \
           && echo module=written || echo module=refused; \
         (echo 'gitdir: /tmp' >'{home_dir}/work/sub/.git') 2>/dev/null \
           && echo gitfile=written || echo gitfile=refused; \
         (echo x >>'{home_dir}/work/.git/config') 2>/dev/null \
           && echo config=written || echo config=refused; \
         (mv '{home_dir}/work/.git' '{home_dir}/work/moved') 2>/dev/null \
           && echo git=moved || echo git=kept; \
         (echo x >'{home_dir}/work/.git/scratch') 2>/dev/null \
           && echo scratch=written || echo scratch=refused; \
         (echo /tmp >'{home_dir}/work/.git/commondir') 2>/dev/null \
           && echo commondir=written || echo commondir=refused; \
         git -C '{home_dir}/work' -c user.name=a -c user.email=a@example.com \
           commit -q --allow-empty -m x >/dev/null 2>&1 \
           && echo commit=done || echo commit=failed; \
         if [ -n \"$GIT_DIR\" ]; then \
           evil='{home_dir}/work/evil.sh'; \
           printf '#!/bin/sh\\necho ran >>{home_dir}/work/ran\\n' >\"$evil\"; chmod +x \"$evil\"; \
           mkdir -p \"$GIT_DIR/hooks\" \"$GIT_DIR/info\" '{home_dir}/work/hooks'; \
           for hook in pre-commit post-checkout post-merge reference-transaction post-update \
             update pre-receive post-receive pre-auto-gc; do \
             cp \"$evil\" \"$GIT_DIR/hooks/$hook\"; cp \"$evil\" '{home_dir}/work/hooks/'$hook; \
           done; \
           echo '* filter=evil' >\"$GIT_DIR/info/attributes\"; \
           git config core.hooksPath '{home_dir}/work/hooks' \
           && git config core.fsmonitor \"$evil\" \
           && git config filter.evil.clean \"$evil\" \
           && git config filter.evil.smudge \"$evil\" \
           && git config filter.evil.process \"$evil\" \
           && git config uploadpack.packObjectsHook \"$evil\" && echo planted=yes; \
         fi; \
         (exec 3<'{peer_tty}') 2>/dev/null && echo peer=open || echo peer=closed; \
         perl -e 'open(my $t, \"+<\", \"/dev/tty\") or die; my $c = \"x\"; \
         print ioctl($t, {tiocsti}, $c) ? \"sti=allowed\\n\" : \"sti=denied\\n\"'; \
         perl -MSocket -e 'for (@ARGV) {{ socket(my $s, PF_UNIX, SOCK_STREAM, 0) or die; \
         print connect($s, pack_sockaddr_un($_)) ? \"planted=open\\n\" : \"planted=closed\\n\" }}' \
         '{planted_paths}'; \
         perl -MSocket -e 'socket(my $s, PF_UNIX, SOCK_STREAM, 0) or die; \
         print connect($s, pack_sockaddr_un(\"\\0$ARGV[0]\")) \
           ? \"abstract=open\\n\" : \"abstract=closed\\n\"' '{name}'; \
         perl -MSocket -e 'socket(my $s, PF_UNIX, SOCK_STREAM, 0) or die; \
         connect($s, pack_sockaddr_un($ARGV[0])) or die; sleep 2' '{api}'; \
         echo end=reached",
    );
    let program = ["/bin/sh".to_owned(), "-c".to_owned(), script];
    let hidden = [ssh, state];
    let work = sandbox::working_directory(&work, &hidden, LAUNCHD_PATH.as_ref()).unwrap();
    // Where PinFold keeps it, in its data directory.
    let shadow = home.join("data/agents/test/shadow");
    let command = match sandbox::command(&program, &hidden, &[], &work, &socket, Some(&shadow)) {
        Err(error) if cfg!(target_os = "linux") && error.kind() == ErrorKind::NotFound => {
            // CI must run the real bwrap (#106), so only a local machine may skip.
            assert!(std::env::var_os("CI").is_none(), "{error}");
            eprintln!("skipped: no bwrap");
            return;
        }
        command => command.unwrap(),
    };
    let pty = Pty::spawn(command, 24, 80).unwrap();

    let start = Instant::now();
    let caller = loop {
        match listener.accept() {
            Ok((caller, _)) => break caller,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                let late = start.elapsed() > Duration::from_secs(10);
                assert!(!late, "no caller: {}", read_until(&pty, "end=reached"));
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    };
    let identity = pinfold_sys::peer_terminal(caller.as_fd()).unwrap();
    assert_eq!(identity, Some(pty.terminal()));

    let output = read_until(&pty, "end=reached");
    let (rc, out) = (
        home.join(".zshrc").exists(),
        work.path().join("out").exists(),
    );
    let hook = work.path().join(".git/hooks/pre-commit").exists();
    let commondir = work.path().join(".git/commondir").exists();
    let config = fs::read(work.path().join(".git/config")).ok();
    // What the user does next, outside the sandbox: fetch the agent's commits and look.
    let git = |args: &[&str]| {
        let mut git = Command::new("git");
        git.arg("-C").arg(work.path()).args(args);
        git.output().unwrap()
    };
    let _ = fs::remove_file(work.path().join("ran"));
    let shadow = shadow.to_str().unwrap();
    let fetch = git(&[
        "fetch",
        "-q",
        "--no-tags",
        shadow,
        "+refs/heads/*:refs/remotes/agent-test/*",
    ]);
    let status = git(&["status", "--porcelain"]);
    let fetched = git(&["log", "-1", "--format=%s", "--remotes=agent-test"]);
    let own = git(&["rev-parse", "-q", "--verify", "HEAD"]);
    let ran = work.path().join("ran").exists();
    fs::remove_dir_all(&home).unwrap();
    for path in &planted {
        fs::remove_file(path).unwrap();
    }
    assert!(output.contains("end=reached"), "{output}");
    assert!(output.contains("hook=refused") && !hook, "{output}");
    assert!(
        output.contains("config=refused") && config == Some(config_before),
        "{output}"
    );
    assert!(output.contains("git=kept"), "{output}");
    assert!(
        output.contains("commondir=refused") && !commondir,
        "{output}"
    );
    assert!(output.contains("module=refused"), "{output}");
    assert!(output.contains("gitfile=refused"), "{output}");
    // Linux keeps all of `.git` read-only, so the agent commits into its shadow.
    if cfg!(target_os = "linux") {
        assert!(output.contains("scratch=refused"), "{output}");
        assert!(output.contains("commit=done"), "{output}");
        assert!(output.contains("planted=yes"), "{output}");
        assert!(
            fetch.status.success() && status.status.success(),
            "{fetch:?} {status:?}"
        );
        assert_eq!(String::from_utf8_lossy(&fetched.stdout), "x\n");
        // The user's own branch is unborn still, and nothing the agent planted ran.
        assert!(!own.status.success(), "{own:?}");
        assert!(!ran, "{output}");
    } else {
        assert!(output.contains("scratch=written"), "{output}");
        assert!(output.contains("commit=done"), "{output}");
    }
    assert!(output.contains("ssh=unset"), "{output}");
    let opened = output.matches("planted=open").count();
    let closed = output.matches("planted=closed").count();
    assert_eq!((opened, closed), (0, planted.len()), "{output}");
    if cfg!(target_os = "linux") {
        let scoped = pinfold_sys::landlock::abi().unwrap() >= pinfold_sys::landlock::SCOPED;
        eprintln!("Landlock scopes abstract sockets: {scoped}");
        let want = if scoped {
            "abstract=closed"
        } else {
            "abstract=open"
        };
        assert!(output.contains(want), "{output}");
    }
    assert!(output.contains("rc=refused") && !rc, "{output}");
    assert!(output.contains("work=written") && out, "{output}");
    assert!(!output.contains("SECRET-KEY"), "{output}");
    assert!(output.contains("id_test"), "{output}");
    assert!(!output.contains("SECRET-STATE"), "{output}");
    assert!(output.contains("peer=closed"), "{output}");
    assert!(output.contains("sti=denied"), "{output}");
    assert!(output.contains("VISIBLE"), "{output}");
    let uid = format!("uid={}", pinfold_sys::effective_uid());
    assert!(output.contains(&uid), "{output}");
}

/// A sandboxed agent cannot open a new terminal: its session would be no pane's, and the API
/// would take it for a person's terminal outside every pane.
#[test]
fn a_sandboxed_agent_cannot_open_a_new_terminal() {
    // Under HOME, where a working directory must be, wherever the target directory is.
    let work = PathBuf::from(std::env::var_os("HOME").unwrap())
        .join(format!("pinfold-terminal-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let _removed = Removed(work.clone());
    fs::set_permissions(&work, fs::Permissions::from_mode(0o700)).unwrap();
    let work = sandbox::working_directory(&work, &[], LAUNCHD_PATH.as_ref()).unwrap();
    let api = work.path().join("api.sock");
    // `script` opens a terminal and starts a session on it, as an agent would.
    let probe = "echo new=$(tty)";
    let script = if cfg!(target_os = "macos") {
        format!("script -q /dev/null /bin/sh -c '{probe}'; echo end=reached")
    } else {
        format!("script -qec '{probe}' /dev/null; echo end=reached")
    };
    // Outside the sandbox the probe opens one, so a missing `script` cannot pass.
    let mut plain = Command::new("/bin/sh");
    plain.args(["-c", &script]);
    let output = read_until(&Pty::spawn(plain, 24, 80).unwrap(), "end=reached");
    assert!(output.contains("new=/dev/"), "{output}");
    // On Linux the agent also tries a user namespace of its own, with a new devpts mounted there.
    let nested = "unshare -Urm sh -c 'mount -t devpts -o newinstance,ptmxmode=0666 devpts \
                  /dev/pts && script -qec \"echo new=\\$(tty)\" /dev/null'; ";
    let script = match cfg!(target_os = "linux") {
        true => format!("{nested}{script}"),
        false => script,
    };
    let program = ["/bin/sh".to_owned(), "-c".to_owned(), script];
    let command = match sandbox::command(&program, &[], &[], &work, &api, None) {
        Err(error) if cfg!(target_os = "linux") && error.kind() == ErrorKind::NotFound => {
            assert!(std::env::var_os("CI").is_none(), "{error}");
            eprintln!("skipped: no bwrap");
            return;
        }
        command => command.unwrap(),
    };
    let pty = Pty::spawn(command, 24, 80).unwrap();
    let output = read_until(&pty, "end=reached");
    assert!(output.contains("end=reached"), "{output}");
    assert!(!output.contains("new=/dev/"), "{output}");
}

/// No credential store of ADR-0041 can be read, and a missing one breaks nothing.
#[test]
fn a_sandboxed_agent_cannot_read_credential_stores() {
    let home = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("pinfold-stores-{}", std::process::id()));
    let _removed = Removed(home.clone());
    let work = home.join("work");
    fs::create_dir_all(&work).unwrap();
    let stores = pinfold_config::credential_stores(&home, &home.join(".config"));
    // The last store is left missing.
    let (_, present) = stores.split_last().unwrap();
    let mut script = String::new();
    for (index, store) in present.iter().enumerate() {
        let file = if store.ends_with(".netrc") {
            fs::create_dir_all(&home).unwrap();
            store.clone()
        } else {
            fs::create_dir_all(store).unwrap();
            store.join("credentials")
        };
        fs::write(&file, format!("SECRET-STORE-{index}\n")).unwrap();
        script += &format!("cat '{}' 2>/dev/null; echo tried={index}; ", file.display());
    }
    script += "echo end=reached";
    for dir in [&home, &work] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let work = sandbox::working_directory(&work, &stores, LAUNCHD_PATH.as_ref()).unwrap();
    let api = work.path().join("api.sock");
    let program = ["/bin/sh".to_owned(), "-c".to_owned(), script];
    let Some(command) = sandboxed(&program, &stores, &[], &work, &api).unwrap() else {
        return;
    };
    let output = read_until(&Pty::spawn(command, 24, 80).unwrap(), "end=reached");
    assert!(output.contains("end=reached"), "{output}");
    assert_eq!(output.matches("tried=").count(), present.len(), "{output}");
    assert!(!output.contains("SECRET-STORE"), "{output}");
}

/// Variables whose names look like secrets do not reach the agent, but those its profile keeps
/// and every other one do.
#[test]
fn a_sandboxed_agent_gets_no_secret_variables_but_those_it_keeps() {
    // The variables are read at each launch, so the check runs in a child with its own.
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "--ignored", "secrets_child", "--nocapture"])
        .env("PINFOLD_SECRETS_CHILD", "1")
        .env("GITHUB_TOKEN", "SECRET-TOKEN")
        .env("AWS_PROFILE", "SECRET-AWS")
        .env("my_password", "SECRET-LOWER")
        .env("OPENAI_API_KEY", "SECRET-KEY")
        .env("ANTHROPIC_API_KEY", "KEPT-KEY")
        .env("PINFOLD_PLAIN", "VISIBLE")
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
#[ignore = "run by a_sandboxed_agent_gets_no_secret_variables_but_those_it_keeps"]
fn secrets_child() {
    if std::env::var_os("PINFOLD_SECRETS_CHILD").is_none() {
        return;
    }
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("pinfold-secrets-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let _removed = Removed(work.clone());
    fs::set_permissions(&work, fs::Permissions::from_mode(0o700)).unwrap();
    let work = sandbox::working_directory(&work, &[], LAUNCHD_PATH.as_ref()).unwrap();
    let api = work.path().join("api.sock");
    let program = ["/bin/sh", "-c", "env; echo end=reached"].map(str::to_owned);
    let keep = ["ANTHROPIC_API_KEY".to_owned()];
    let Some(command) = sandboxed(&program, &[], &keep, &work, &api).unwrap() else {
        return;
    };
    let output = read_until(&Pty::spawn(command, 24, 80).unwrap(), "end=reached");
    assert!(output.contains("end=reached"), "{output}");
    assert!(output.contains("ANTHROPIC_API_KEY=KEPT-KEY"), "{output}");
    assert!(output.contains("PINFOLD_PLAIN=VISIBLE"), "{output}");
    assert!(!output.contains("SECRET-"), "{output}");
}

/// The sandboxed command, or nothing on a machine without bwrap, which CI never is (#106).
fn sandboxed(
    program: &[String],
    hidden: &[PathBuf],
    keep: &[String],
    work: &sandbox::WorkingDirectory,
    api: &Path,
) -> std::io::Result<Option<Command>> {
    match sandbox::command(program, hidden, keep, work, api, None) {
        Err(error) if cfg!(target_os = "linux") && error.kind() == ErrorKind::NotFound => {
            assert!(std::env::var_os("CI").is_none(), "{error}");
            eprintln!("skipped: no bwrap");
            Ok(None)
        }
        command => command.map(Some),
    }
}

#[test]
fn a_bwrap_planted_on_path_is_ignored() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("pinfold-planted-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let planted = dir.join("bwrap");
    fs::write(&planted, "#!/bin/sh\nexec \"$@\"\n").unwrap();
    fs::set_permissions(&planted, fs::Permissions::from_mode(0o755)).unwrap();
    // The variable is read at each lookup, so the check runs in a child with its own PATH.
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "--ignored", "planted_child", "--nocapture"])
        .env("PATH", &dir)
        .env("PINFOLD_PLANTED", &planted)
        .status()
        .unwrap();
    fs::remove_dir_all(&dir).unwrap();
    assert!(status.success());
}

#[test]
#[ignore = "run by a_bwrap_planted_on_path_is_ignored"]
fn planted_child() {
    let Some(planted) = std::env::var_os("PINFOLD_PLANTED") else {
        return;
    };
    let work = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("pinfold-child-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    fs::set_permissions(&work, fs::Permissions::from_mode(0o700)).unwrap();
    let work = sandbox::working_directory(&work, &[], LAUNCHD_PATH.as_ref()).unwrap();
    let api = work.path().join("api.sock");
    let result = sandbox::command(&["/bin/true".to_owned()], &[], &[], &work, &api, None);
    fs::remove_dir(work.path()).unwrap();
    match result {
        Ok(command) => assert_ne!(command.get_program(), planted),
        Err(error) => assert_eq!(error.kind(), ErrorKind::NotFound),
    }
}

/// A directory removed when the test ends, passed or not.
struct Removed(PathBuf);

impl Drop for Removed {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Reads until `wanted` appears or ten seconds pass.
fn read_until(pty: &Pty, wanted: &str) -> String {
    let (mut text, mut buffer) = (String::new(), [0u8; 4096]);
    let start = Instant::now();
    while !text.contains(wanted) && start.elapsed() < Duration::from_secs(10) {
        match pty.master().read(&mut buffer) {
            Ok(count) => text.push_str(&String::from_utf8_lossy(&buffer[..count])),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            // Linux reports EIO once the child has closed its end.
            Err(_) => break,
        }
    }
    text
}
