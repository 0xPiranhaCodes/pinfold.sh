//! A sandboxed agent, run with the real `sandbox-exec` or bubblewrap, cannot read hidden
//! directories or another pane's terminal, writes only to its working directory but not its git
//! hooks, reaches no Unix socket but the API's, and keeps the caller identity the API gives its
//! pane.

use std::fs;
use std::io::{ErrorKind, Read};
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use pinfold_core::sandbox;
use pinfold_sys::pty::Pty;

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
    let socket = home.join("api.sock");
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
         git -C '{home_dir}/work' -c user.name=a -c user.email=a@example.com \
           commit -q --allow-empty -m x >/dev/null 2>&1 \
           && echo commit=done || echo commit=failed; \
         (exec 3<'{peer_tty}') 2>/dev/null && echo peer=open || echo peer=closed; \
         perl -e 'open(my $t, \"+<\", \"/dev/tty\") or die; my $c = \"x\"; \
         print ioctl($t, {tiocsti}, $c) ? \"sti=allowed\\n\" : \"sti=denied\\n\"'; \
         perl -MSocket -e 'for (@ARGV) {{ socket(my $s, PF_UNIX, SOCK_STREAM, 0) or die; \
         print connect($s, pack_sockaddr_un($_)) ? \"planted=open\\n\" : \"planted=closed\\n\" }}' \
         '{planted_paths}'; \
         perl -MSocket -e 'socket(my $s, PF_UNIX, SOCK_STREAM, 0) or die; \
         connect($s, pack_sockaddr_un($ARGV[0])) or die; sleep 2' '{api}'; \
         echo done",
    );
    let program = ["/bin/sh".to_owned(), "-c".to_owned(), script];
    let hidden = [ssh, state];
    let work = sandbox::working_directory(&work, &hidden).unwrap();
    let command = match sandbox::command(&program, &hidden, &work, &socket) {
        Err(error) if cfg!(target_os = "linux") && error.kind() == ErrorKind::NotFound => {
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
                assert!(!late, "no caller: {}", read_until(&pty, "done"));
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    };
    let identity = pinfold_sys::peer_terminal(caller.as_fd()).unwrap();
    assert_eq!(identity, Some(pty.terminal()));

    let output = read_until(&pty, "done");
    let (rc, out) = (
        home.join(".zshrc").exists(),
        work.path().join("out").exists(),
    );
    let hook = work.path().join(".git/hooks/pre-commit").exists();
    let config = fs::read(work.path().join(".git/config")).ok();
    fs::remove_dir_all(&home).unwrap();
    for path in &planted {
        fs::remove_file(path).unwrap();
    }
    assert!(output.contains("hook=refused") && !hook, "{output}");
    assert!(
        output.contains("config=refused") && config == Some(config_before),
        "{output}"
    );
    assert!(output.contains("git=kept"), "{output}");
    assert!(output.contains("module=refused"), "{output}");
    assert!(output.contains("gitfile=refused"), "{output}");
    // Linux keeps all of `.git` read-only, so commits happen outside the sandbox.
    if cfg!(target_os = "linux") {
        assert!(output.contains("scratch=refused"), "{output}");
        assert!(output.contains("commit=failed"), "{output}");
    } else {
        assert!(output.contains("scratch=written"), "{output}");
        assert!(output.contains("commit=done"), "{output}");
    }
    assert!(output.contains("ssh=unset"), "{output}");
    let opened = output.matches("planted=open").count();
    let closed = output.matches("planted=closed").count();
    assert_eq!((opened, closed), (0, planted.len()), "{output}");
    assert!(output.contains("rc=refused") && !rc, "{output}");
    assert!(output.contains("work=written") && out, "{output}");
    assert!(!output.contains("SECRET-KEY"), "{output}");
    assert!(output.contains("id_test"), "{output}");
    assert!(!output.contains("SECRET-STATE"), "{output}");
    assert!(output.contains("peer=closed"), "{output}");
    // Linux has no TIOCSTI filter yet; the sandbox wraps the whole pane instead.
    if cfg!(target_os = "macos") {
        assert!(output.contains("sti=denied"), "{output}");
    }
    assert!(output.contains("VISIBLE"), "{output}");
    let uid = format!("uid={}", pinfold_sys::effective_uid());
    assert!(output.contains(&uid), "{output}");
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
    let work = sandbox::working_directory(&work, &[]).unwrap();
    let api = work.path().join("api.sock");
    let result = sandbox::command(&["/bin/true".to_owned()], &[], &work, &api);
    fs::remove_dir(work.path()).unwrap();
    match result {
        Ok(command) => assert_ne!(command.get_program(), planted),
        Err(error) => assert_eq!(error.kind(), ErrorKind::NotFound),
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
