//! A repository whose configuration names a command at every place git runs one, looked at
//! through `pinfold_tools::git::run` (threat T5).

#![allow(clippy::unwrap_used, reason = "test helpers")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use pinfold_tools::git;

/// Plain git, for building the repository and for the control run.
fn plain(root: &Path, repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap()
}

fn markers(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn nothing_a_hostile_repository_configures_runs() {
    let root = std::env::temp_dir().join(format!("ph{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let (repo, marks, hooks) = (root.join("repo"), root.join("marks"), root.join("hooks"));
    for dir in [&repo, &marks, &hooks] {
        fs::create_dir_all(dir).unwrap();
    }
    let git = |args: &[&str]| assert!(plain(&root, &repo, args).status.success(), "{args:?}");
    git(&["init", "-q"]);
    let attributes = "*.txt filter=evil diff=evil\neq.txt filter=ev=il\n";
    fs::write(repo.join(".gitattributes"), attributes).unwrap();
    for file in ["a.txt", "b.txt", "eq.txt"] {
        fs::write(repo.join(file), "one\n").unwrap();
    }
    git(&["add", "."]);
    git(&["commit", "-qm", "one"]);
    // A submodule with its own filter, which `ignore = none` asks git to look inside.
    let sub = repo.join("sub");
    fs::create_dir(&sub).unwrap();
    let in_sub = |args: &[&str]| assert!(plain(&root, &sub, args).status.success(), "{args:?}");
    in_sub(&["init", "-q"]);
    fs::write(sub.join(".gitattributes"), "*.txt filter=sub\n").unwrap();
    fs::write(sub.join("s.txt"), "one\n").unwrap();
    in_sub(&["add", "."]);
    in_sub(&["commit", "-qm", "one"]);
    let modules = "[submodule \"sub\"]\n\tpath = sub\n\turl = ./sub\n\tignore = none\n";
    fs::write(repo.join(".gitmodules"), modules).unwrap();
    git(&["add", ".gitmodules", "sub"]);
    git(&["commit", "-qm", "sub"]);
    // The same size, so only the filter can tell.
    fs::write(sub.join("s.txt"), "two\n").unwrap();
    fs::write(repo.join("b.txt"), "staged\n").unwrap();
    git(&["add", "b.txt"]);
    fs::write(repo.join("a.txt"), "one\nchanged\n").unwrap();
    fs::write(repo.join("eq.txt"), "one\nchanged\n").unwrap();

    let mark = |name: &str| format!("touch {}", marks.join(name).display());
    let hook = hooks.join("pre-commit");
    fs::write(&hook, format!("#!/bin/sh\n{}\n", mark("hook"))).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    fs::copy(&hook, repo.join(".git/hooks/pre-commit")).unwrap();
    let hostile = [
        ("core.fsmonitor", format!("{}; false", mark("fsmonitor"))),
        ("core.pager", mark("pager")),
        ("pager.diff", mark("pager.diff")),
        ("pager.status", mark("pager.status")),
        ("diff.external", mark("external")),
        ("diff.evil.command", mark("diff-driver")),
        ("diff.evil.textconv", mark("textconv")),
        ("filter.evil.clean", format!("{}; cat", mark("clean"))),
        ("filter.evil.smudge", format!("{}; cat", mark("smudge"))),
        ("filter.evil.process", format!("{}; false", mark("process"))),
        ("filter.evil.required", "true".into()),
        // `-c filter.ev=il.clean=` would set `filter.ev` instead.
        ("filter.ev=il.clean", format!("{}; cat", mark("clean-eq"))),
        ("core.hooksPath", hooks.display().to_string()),
        ("core.sshCommand", format!("{}; false", mark("ssh"))),
        ("core.askPass", mark("askpass")),
        (
            "credential.helper",
            format!("!{}; false", mark("credential")),
        ),
        ("protocol.ext.allow", "always".into()),
        ("protocol.file.allow", "always".into()),
        ("protocol.ssh.allow", "always".into()),
        (
            "remote.ext.url",
            format!("ext::sh -c {}", mark("ext").replace(' ', "% ")),
        ),
        ("remote.file.url", format!("file://{}", repo.display())),
        (
            "remote.file.uploadpack",
            format!("{}; false", mark("uploadpack")),
        ),
        ("remote.ssh.url", "ssh://example.invalid/x".into()),
    ];
    for (key, value) in hostile {
        git(&["config", key, &value]);
    }
    in_sub(&[
        "config",
        "filter.sub.process",
        &format!("{}; false", mark("sub")),
    ]);

    // Control: plain git runs the repository's commands.
    plain(&root, &repo, &["diff", "--", "a.txt"]);
    plain(&root, &repo, &["diff", "--", "eq.txt"]);
    plain(&root, &repo, &["fetch", "ext"]);
    plain(&root, &repo, &["hook", "run", "pre-commit"]);
    // Past the filter that fails on purpose, to the submodule.
    let past = "filter.evil.required=false";
    plain(
        &root,
        &repo,
        &[
            "-c",
            "core.fsmonitor=false",
            "-c",
            "filter.evil.process=",
            "-c",
            past,
            "status",
        ],
    );
    let fired = markers(&marks);
    for expected in ["clean-eq", "ext", "fsmonitor", "hook", "process", "sub"] {
        assert!(
            fired.iter().any(|name| name == expected),
            "{expected}: {fired:?}"
        );
    }
    for name in fired {
        fs::remove_file(marks.join(name)).unwrap();
    }

    let run = |args: &[&str]| git::run(&repo, args).unwrap();
    // The diff viewer's stage and revert: the patch goes in on standard input, no file.
    let patch = run(&["diff", "--", "a.txt"]).stdout;
    let outputs = [
        run(&["diff"]),
        run(&["diff", "--staged"]),
        run(&["status", "--porcelain"]),
        git::apply(&repo, &patch, false).unwrap(),
        git::apply(&repo, &patch, true).unwrap(),
        // Each transport is refused, so none of the remote's commands can run.
        run(&["fetch", "ext"]),
        run(&["fetch", "file"]),
        run(&["fetch", "ssh"]),
        run(&["hook", "run", "--ignore-missing", "pre-commit"]),
    ];
    assert_eq!(markers(&marks), Vec::<String>::new());

    let [diff, staged, status, cached, reverse, ext, file, ssh, _] = outputs;
    let has =
        |output: &git::Output, text: &str| String::from_utf8_lossy(&output.stdout).contains(text);
    assert!(has(&diff, "+changed") && has(&staged, "+staged") && has(&status, "a.txt"));
    for output in [&cached, &reverse] {
        let errors = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.is_some_and(|status| status.success()),
            "{errors}"
        );
    }
    for fetched in [&ext, &file, &ssh] {
        assert!(fetched.status.is_some_and(|status| !status.success()));
    }
    fs::remove_dir_all(root).unwrap();
}
