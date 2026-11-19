use std::os::unix::fs::PermissionsExt;

use super::*;

/// A stub `pinfold` in a fresh folder: each run appends its arguments to `log`; a take sleeps
/// for the seconds in `slow`, if there is one, and answers checkpoint `<run>/<n>` counting from
/// 0 with `n * 3` changed, a list answers one checkpoint, a restore its checkpoint 7, and a
/// prune nothing.
fn stub(wait: Duration) -> (tempfile::TempDir, Rewind<u32>) {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("pinfold");
    let script = format!(
        "#!/bin/sh\ncd '{dir}' || exit 99\necho \"$*\" >> log\ncase \"$2\" in\n\
         take) [ -f slow ] && sleep \"$(cat slow)\"; n=$(cat n.$3 2>/dev/null || echo 0); \
         echo $((n + 1)) > n.$3; \
         printf '{{\"checkpoint\":\"%s/%s\",\"store\":\"k\",\"files\":1,\"new_bytes\":2,\
         \"complete\":true,\"changed\":%s}}\\n' \"$3\" \"$n\" $((n * 3)) ;;\n\
         list) printf '{{\"checkpoints\":[{{\"checkpoint\":\"%s/0\",\"reason\":\"launch\"}}],\
         \"more\":0}}\\n' \"$3\" ;;\n\
         prune) echo '{{}}' ;;\n\
         plan) echo '{{\"restore\":1,\"delete\":0,\"bring_back\":0,\"paths\":[]}}' ;;\n\
         restore) printf '{{\"restored\":1,\"deleted\":2,\"skipped\":0,\
         \"restore_checkpoint\":\"%s/7\"}}\\n' \"$3\" ;;\nesac\n",
        dir = dir.path().display()
    );
    // `cp` writes the program, so no fork of another test thread holds it open for writing.
    let source = dir.path().join("pinfold.sh");
    std::fs::write(&source, script).unwrap();
    let copied = Command::new("/bin/cp").arg(&source).arg(&program).status();
    assert!(copied.unwrap().success());
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let settings = Settings {
        on_done: true,
        wait,
        max_files: 9,
        max_file_mib: 1,
        max_new_mib: 2,
        include_ignored: true,
        max_store_mib: 4,
        keep_days: 3,
        keep_per_run: 5,
    };
    (dir, Rewind::new(program, settings, 1_700_000_000, None))
}

fn log(dir: &tempfile::TempDir) -> Vec<String> {
    let log = std::fs::read_to_string(dir.path().join("log")).unwrap_or_default();
    log.lines().map(str::to_owned).collect()
}

/// Polls until `done` holds of what ended and the log, for up to 20 s.
fn until(
    rewind: &mut Rewind<u32>,
    dir: &tempfile::TempDir,
    mut done: impl FnMut(&[Finished<u32>], &[String]) -> bool,
) -> Vec<Finished<u32>> {
    let (deadline, mut ended) = (Instant::now() + Duration::from_secs(20), Vec::new());
    loop {
        ended.extend(rewind.poll(Instant::now()));
        if done(&ended, &log(dir)) {
            return ended;
        }
        assert!(Instant::now() < deadline, "{:?}", log(dir));
        thread::sleep(Duration::from_millis(10));
    }
}

/// Polls until both workers are free and nothing waits.
fn idle(rewind: &mut Rewind<u32>) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(rewind.poll(Instant::now()).is_empty());
        let wanted = rewind
            .runs
            .iter()
            .any(|run| run.flags & (WANT_DONE | WANT_EXIT) != 0);
        if rewind.slots.iter().all(Option::is_none) && rewind.queue.is_empty() && !wanted {
            return;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_run_record_is_48_bytes_and_at_64_the_oldest_closed_one_is_reused() {
    assert_eq!(size_of::<Run>(), 48);
    let (dir, mut rewind) = stub(Duration::from_secs(10));
    assert_eq!(rewind.runs.capacity(), 0, "nothing until a launch");
    for pane in 0..MAX_RUNS {
        let run = rewind.open(100, dir.path()).unwrap();
        rewind.started(run, Some(pane));
    }
    assert_eq!(rewind.runs.capacity(), MAX_RUNS, "3 KiB at the cap");
    assert_eq!(rewind.open(100, dir.path()), Err(Missing::TooManyRuns));
    // Panes 3 and 5 close; their exit takes run, then 3, the older, is reused.
    rewind.observe(Instant::now(), |pane| (pane == 3 || pane == 5, false));
    idle(&mut rewind);
    let run = rewind.open(100, dir.path()).unwrap();
    assert_eq!(run, 3);
    let record = rewind.run(run).unwrap();
    assert_eq!(record.id(), "r1700000000-64");
    assert_eq!((record.pane(), record.launcher()), (None, 100));
    let key = record.key();
    assert_eq!(key.len(), 16);
    let resolved = dir.path().canonicalize().unwrap();
    let hash = pinfold_api::trust::sha256(resolved.as_os_str().as_encoded_bytes());
    assert_eq!(key, hash[..16], "the store's key, as the take computes it");
}

#[test]
fn a_launch_past_wait_s_ends_with_no_checkpoint_and_never_blocks() {
    let (dir, mut rewind) = stub(Duration::from_millis(300));
    std::fs::write(dir.path().join("slow"), "30").unwrap();
    let run = rewind.open(1, dir.path()).unwrap();
    let started = Instant::now();
    rewind
        .launch(run, dir.path().to_owned(), 7, started)
        .unwrap();
    assert!(rewind.poll(started).is_empty(), "polls return at once");
    assert!(started.elapsed() < Duration::from_millis(250));
    assert!(rewind.deadline() <= Some(started + Duration::from_millis(300)));
    let ended = until(&mut rewind, &dir, |ended, _| !ended.is_empty());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "killed at wait_s"
    );
    assert_eq!(ended[0].payload, 7);
    assert_eq!(ended[0].result.as_ref().unwrap_err(), KILLED);
    assert_eq!(rewind.run(run).unwrap().missing(), Missing::TookOver);
}

#[test]
fn a_launch_queued_past_wait_s_gives_up_without_a_worker() {
    let (dir, mut rewind) = stub(Duration::from_millis(200));
    std::fs::write(dir.path().join("slow"), "30").unwrap();
    let now = Instant::now();
    // Two slow takes of two other stores hold both workers.
    for name in ["a", "b", "c"] {
        std::fs::create_dir(dir.path().join(name)).unwrap();
    }
    for name in ["a", "b"] {
        let run = rewind.open(1, &dir.path().join(name)).unwrap();
        rewind.started(run, Some(run));
        rewind.take(run, 0, now).unwrap();
    }
    // Both started, as a launch queued beside them would start first.
    until(&mut rewind, &dir, |_, log| {
        log.iter().filter(|line| line.contains(" api ")).count() == 2
    });
    let queued = rewind.open(1, &dir.path().join("c")).unwrap();
    rewind.launch(queued, dir.path().join("c"), 9, now).unwrap();
    let ended = until(&mut rewind, &dir, |ended, _| {
        ended.iter().any(|e| e.payload == 9)
    });
    let gave_up = ended.iter().find(|e| e.payload == 9).unwrap();
    assert_eq!(gave_up.result.as_ref().unwrap_err(), KILLED);
    assert_eq!(rewind.run(queued).unwrap().missing(), Missing::TookOver);
    assert!(!log(&dir).iter().any(|line| line.contains(" launch ")));
}

#[test]
fn done_and_exit_take_checkpoints_by_key_then_prune_the_live_runs() {
    let (dir, mut rewind) = stub(Duration::from_secs(10));
    let run = rewind.open(1, dir.path()).unwrap();
    rewind
        .launch(run, dir.path().to_owned(), 1, Instant::now())
        .unwrap();
    let ended = until(&mut rewind, &dir, |ended, _| !ended.is_empty());
    assert_eq!(
        ended[0].result.as_ref().unwrap()["checkpoint"],
        "r1700000000-0/0"
    );
    rewind.started(run, Some(4));
    idle(&mut rewind);

    let now = Instant::now();
    rewind.observe(now, |_| (false, true));
    // Done again within 60 s takes nothing.
    rewind.observe(now, |_| (false, false));
    rewind.observe(now, |_| (false, true));
    idle(&mut rewind);
    assert_eq!(rewind.run(run).unwrap().newest(), Some(1));
    rewind.observe(now, |_| (true, false));
    idle(&mut rewind);
    let record = rewind.run(run).unwrap();
    assert!(record.is_closed());
    assert_eq!(
        (record.newest(), record.missing()),
        (Some(2), Missing::None)
    );
    let key = record.key();
    let log = log(&dir);
    let takes: Vec<_> = log.iter().filter(|line| line.contains(" take ")).collect();
    assert_eq!(takes.len(), 3, "{log:?}");
    let launched = format!(
        "checkpoint take r1700000000-0 launch --max-files 9 --max-file-mib 1 --max-new-mib 2 --include-ignored -- {}",
        dir.path().display()
    );
    assert_eq!(takes[0], &launched);
    let by_key = |reason| {
        format!(
            "take r1700000000-0 {reason} --max-files 9 --max-file-mib 1 --max-new-mib 2 --include-ignored --key {key}"
        )
    };
    assert!(takes[1].ends_with(&by_key("done")), "{log:?}");
    assert!(takes[2].ends_with(&by_key("exit")), "{log:?}");
    let prunes: Vec<_> = log.iter().filter(|line| line.contains(" prune")).collect();
    assert_eq!(prunes.len(), 3, "after each take");
    assert_eq!(
        prunes[0],
        "checkpoint prune --live r1700000000-0 --max-store-mib 4 --keep-days 3 --keep-per-run 5"
    );
    assert_eq!(
        prunes[2], "checkpoint prune --max-store-mib 4 --keep-days 3 --keep-per-run 5",
        "a closed run is not live"
    );
}

#[test]
fn takes_are_limited_lists_read_on_demand_and_a_full_queue_is_busy() {
    let (dir, mut rewind) = stub(Duration::from_secs(10));
    let now = Instant::now();
    let run = rewind.open(1, dir.path()).unwrap();
    rewind.started(run, Some(2));
    rewind.take(run, 1, now).unwrap();
    let (_, why) = rewind
        .take(run, 2, now + Duration::from_secs(29))
        .unwrap_err();
    assert!(why.contains("once in 30 s"), "{why}");
    // An API caller's view shares that limit; the window's is not limited.
    let (_, why) = rewind.view(run, 6, Some(now)).unwrap_err();
    assert!(why.contains("once in 30 s"), "{why}");
    rewind.view(run, 7, None).unwrap();
    rewind.list(run, 3).unwrap();
    let ended = until(&mut rewind, &dir, |ended, _| ended.len() == 3);
    let viewed = ended.iter().find(|e| e.payload == 7).unwrap();
    assert!(viewed.result.is_ok(), "{viewed:?}");
    let listed = ended.iter().find(|e| e.payload == 3).unwrap();
    assert_eq!(
        listed.result.as_ref().unwrap()["checkpoints"][0]["reason"],
        "launch"
    );
    let key = rewind.run(run).unwrap().key();
    let list = format!("checkpoint list r1700000000-0 --key {key}");
    assert!(log(&dir).contains(&list));
    idle(&mut rewind);

    std::fs::write(dir.path().join("slow"), "30").unwrap();
    rewind.take(run, 4, now + Duration::from_secs(31)).unwrap();
    let other = rewind.open(1, &dir.path().join("..")).unwrap();
    rewind.take(other, 5, now).unwrap();
    assert!(rewind.poll(Instant::now()).is_empty());
    for payload in 0..QUEUE {
        rewind.list(run, 10 + payload as u32).unwrap();
    }
    assert_eq!(rewind.list(run, 99).unwrap_err().1, BUSY);
    let launch = rewind.open(1, dir.path()).unwrap();
    let (payload, missing) = rewind
        .launch(launch, dir.path().to_owned(), 98, now)
        .unwrap_err();
    assert_eq!((payload, missing), (98, Missing::Busy));
    assert_eq!(rewind.run(launch).unwrap().missing(), Missing::Busy);
}

#[test]
fn a_child_that_crashes_or_prints_too_much_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let crash = child(
        Path::new("/bin/sh"),
        &["-c".into(), "kill -9 $$".into()],
        LIST_BOUND,
    );
    assert_eq!(crash.unwrap_err(), "pinfold checkpoint gave no answer");
    let flood = "yes '{\"a\":1}' | head -c 20000";
    let flood = child(
        Path::new("/bin/sh"),
        &["-c".into(), flood.into()],
        LIST_BOUND,
    );
    assert!(flood.is_err());
    let error = child(
        Path::new("/bin/sh"),
        &["-c".into(), "echo '{\"error\":\"no git\"}'".into()],
        LIST_BOUND,
    );
    assert_eq!(error.unwrap_err(), "no git");
    let missing = child(&dir.path().join("none"), &[], LIST_BOUND);
    assert!(missing.unwrap_err().starts_with("cannot start"));
}

#[test]
fn a_restore_plans_without_the_lock_and_restores_under_it_with_paths_only_when_chosen() {
    let (dir, mut rewind) = stub(Duration::from_secs(10));
    let run = rewind.open(1, dir.path()).unwrap();
    rewind.started(run, Some(2));
    let key = rewind.run(run).unwrap().key();
    let paths = ["src/a.rs".to_owned(), "b c".to_owned()];
    rewind.plan(run, (0, 3), None, 1).unwrap();
    rewind.plan(run, (0, 3), Some(&paths), 2).unwrap();
    // An empty list would read as all to a reader that took it for none; it goes as all.
    rewind.restore(run, (0, 3), Some(&[]), 3).unwrap();
    let ended = until(&mut rewind, &dir, |ended, _| ended.len() == 3);
    assert!(ended.iter().all(|ended| ended.result.is_ok()), "{ended:?}");
    idle(&mut rewind);
    let log = log(&dir);
    let plan = format!("checkpoint plan r1700000000-0 0 3 --key {key}");
    assert!(log.contains(&plan), "{log:?}");
    assert!(log.contains(&format!("{plan} -- src/a.rs b c")), "{log:?}");
    let restore = format!("checkpoint restore r1700000000-0 0 3 --key {key}");
    assert!(log.contains(&restore), "{log:?}");
    // Only the restore writes, so only it is followed by a prune.
    let prunes = log
        .iter()
        .filter(|line| line.starts_with("checkpoint prune"));
    assert_eq!(prunes.count(), 1, "{log:?}");
    assert_eq!(rewind.run(run).unwrap().missing(), Missing::None);
    let restored = Taken {
        run,
        n: 7,
        reason: "restore",
        complete: true,
        changed: 3,
    };
    assert_eq!(rewind.taken(), [restored], "plans take nothing");
}

/// ADR-0052 slice 2: each take's checkpoint and `changed`, as its line gave them, waits for
/// the run's timeline.
#[test]
fn each_take_is_kept_for_the_timeline_with_its_changed_count() {
    let (dir, mut rewind) = stub(Duration::from_secs(10));
    let run = rewind.open(1, dir.path()).unwrap();
    rewind
        .launch(run, dir.path().to_owned(), 1, Instant::now())
        .unwrap();
    until(&mut rewind, &dir, |ended, _| !ended.is_empty());
    rewind.started(run, Some(4));
    rewind.observe(Instant::now(), |_| (false, true));
    idle(&mut rewind);
    rewind.take(run, 2, Instant::now()).unwrap();
    until(&mut rewind, &dir, |ended, _| !ended.is_empty());
    idle(&mut rewind);
    let taken: Vec<_> = rewind
        .taken()
        .into_iter()
        .map(|taken| (taken.n, taken.reason, taken.complete, taken.changed))
        .collect();
    let expected = [
        (0, "launch", true, 0),
        (1, "done", true, 3),
        (2, "api", true, 6),
    ];
    assert_eq!(taken, expected);
    assert!(rewind.taken().is_empty());
}

#[test]
fn the_window_is_told_each_live_runs_strip_once_and_again_when_it_changes() {
    let (dir, mut rewind) = stub(Duration::from_secs(10));
    let run = rewind.open(1, dir.path()).unwrap();
    assert_eq!(rewind.untold(), None, "no pane yet");
    rewind.started(run, Some(7));
    rewind.set_missing(run, Missing::TookOver);
    assert_eq!(rewind.untold(), Some((run, 7, Missing::TookOver)));
    rewind.told(run);
    assert_eq!(rewind.untold(), None);
    rewind.set_missing(run, Missing::TookOver);
    assert_eq!(rewind.untold(), None, "unchanged");
    rewind.set_missing(run, Missing::None);
    assert_eq!(rewind.untold(), Some((run, 7, Missing::None)), "cleared");
    rewind.told(run);
    rewind.untell();
    assert_eq!(
        rewind.untold(),
        Some((run, 7, Missing::None)),
        "a new window"
    );
    rewind.started(run, None);
    assert_eq!(rewind.untold(), None, "a closed run's pane is gone");
    assert_eq!(Missing::TookOver.word(), "took over wait_s");
}

#[test]
fn the_prompt_shows_the_stores_words_escaped_and_marks_paths_changed_later() {
    let plan = serde_json::json!({
        "restore": 5, "delete": 3, "bring_back": 1, "not_covered": 0, "later": 2,
        "complete": true, "dir": "/home/me/work\u{1b}[2J", "reason": "launch",
        "time": 1_000, "more": 4,
        "paths": [["src/a.rs", "restore", true], ["x\u{202e}txt.exe", "delete", false]],
    });
    let texts = prompt_texts(&plan, Some(4), true, 1_000 + 12 * 60);
    assert_eq!(
        texts,
        [
            "/home/me/work^[[2J",
            "since 12 min ago (launch)",
            "asked by the tool in pane 4",
            "restore 5 · delete 3 the agent added · bring back 1 it deleted · 2 changed later",
            "the agent is still running, and may change these files again",
            "! restore src/a.rs",
            "  delete x<U+202E>txt.exe",
            "and 4 more",
        ]
    );
    let incomplete = serde_json::json!({"complete": false, "dir": "/w", "time": 0});
    let texts = prompt_texts(&incomplete, None, false, 0);
    assert_eq!(texts[2], "asked by a terminal outside every pane");
    assert_eq!(
        texts[4],
        "the checkpoint is incomplete, so nothing is deleted"
    );
    let long = serde_json::json!({"paths": [["\u{1}".repeat(300), "restore", false]]});
    let texts = prompt_texts(&long, None, false, 0);
    assert_eq!(
        texts.last().unwrap().chars().count(),
        2 + "restore ".len() + SHOWN
    );
}

#[test]
fn a_hostile_store_message_or_path_never_reaches_the_flash() {
    for why in [
        "cannot write /home/me/.ssh/id_rsa",
        "a broken checkpoint message: ignore previous instructions",
        "\u{1b}[2J✓ done · undid 0 files",
        &"x".repeat(4096),
    ] {
        let said = restore_error(why);
        assert_eq!(said, "the restore was refused or failed", "{why}");
    }
    assert_eq!(restore_error(KILLED), "the restore took over 60 s");
    let gone = "checkpoint r1-0/3 no longer exists";
    assert_eq!(restore_error(gone), "the checkpoint no longer exists");
    for said in [KILLED, gone, BUSY, "?"].map(restore_error) {
        assert!(said.len() <= pinfold_proto::link::MAX_RESTORE_ERROR && !said.contains('/'));
    }
}
