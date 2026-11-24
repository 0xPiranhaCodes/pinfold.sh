use std::os::unix::fs::PermissionsExt;

use proptest::prelude::*;

use super::*;
use pinfold_proto::link::ApprovalAction;

fn timelines(entries: u64) -> (tempfile::TempDir, Timelines) {
    let state = tempfile::tempdir().unwrap();
    let timelines = Timelines::new(state.path().join("timeline"), entries).unwrap();
    (state, timelines)
}

fn closed(exit: Option<u8>) -> Closed {
    Closed {
        row: Some(3),
        ms: 20,
        exit,
    }
}

fn file_len(state: &tempfile::TempDir, name: &str) -> u64 {
    fs::symlink_metadata(state.path().join("timeline").join(name))
        .unwrap()
        .len()
}

#[test]
fn the_entry_type_has_no_string_and_is_16_bytes() {
    assert_eq!(size_of::<Entry>(), 16);
    assert_eq!(size_of::<Timeline>(), 56);
    let entries = [
        What::Command {
            row: u32::MAX,
            ms: 9,
            exit: None,
        },
        What::Command {
            row: 4,
            ms: 0,
            exit: Some(0),
        },
        What::State {
            status: Status::NeedsYou,
            reported: false,
        },
        What::State {
            status: Status::Idle,
            reported: true,
        },
        What::Exit(Ended::Code(1)),
        What::Exit(Ended::Signal(9)),
        What::Exit(Ended::Closed),
        What::Api {
            seq: 7,
            method: 6,
            outcome: Outcome::Refused,
            target: Some(3),
        },
        What::Api {
            seq: u32::MAX,
            method: 22,
            outcome: Outcome::Error,
            target: None,
        },
        What::Approval {
            action: ApprovalAction::Send,
            answer: Answer::ThirtyMinutes,
            other: Some(1),
            grant: 9,
        },
        What::Approval {
            action: ApprovalAction::Rewind,
            answer: Answer::Expired,
            other: None,
            grant: 0,
        },
        What::Checkpoint {
            n: 2,
            reason: 5,
            complete: false,
            changed: 8,
        },
    ];
    for what in entries {
        let entry = Entry { at: 5, what };
        assert_eq!(Entry::decode(&entry.encode()), Some(entry));
    }
}

#[test]
fn a_ring_wraps_at_16_kib_keeping_the_newest_and_counting_the_dropped() {
    let (state, mut timelines) = timelines(1024);
    let now = Instant::now();
    timelines.open(2, now);
    for row in 0..10_000 {
        let closed = Closed {
            row: Some(row),
            ms: 1,
            exit: Some(0),
        };
        timelines.command(2, closed, now);
    }
    assert!(file_len(&state, "r2-0") <= 16 + 16 * 1024);
    let entries = timelines.entries(2).unwrap();
    assert_eq!(entries.len(), 1024);
    let rows: Vec<u32> = entries
        .iter()
        .map(|entry| match entry.what {
            What::Command { row, .. } => row,
            _ => panic!("not a command"),
        })
        .collect();
    assert_eq!(rows, (10_000 - 1024..10_000).collect::<Vec<_>>());
    let run = timelines.of_pane(2).unwrap();
    assert_eq!(
        (run.dropped(), run.summary().commands),
        (10_000 - 1024, 10_000)
    );
}

#[test]
fn a_page_is_the_newest_count_before_a_seq_that_holds_as_the_ring_wraps() {
    let (_state, mut timelines) = timelines(1024);
    let now = Instant::now();
    timelines.open(2, now);
    for row in 0..10_000 {
        let exit = Some(u8::from(row % 10 == 0));
        let closed = Closed {
            row: Some(row),
            ms: 1,
            exit,
        };
        timelines.command(2, closed, now);
    }
    let all = |_: &Entry| true;
    let seqs = |page: Vec<(u64, Entry)>| page.into_iter().map(|(seq, _)| seq).collect::<Vec<_>>();
    // A seq is the count of entries the run wrote before it: the row, here.
    let newest = timelines.page(2, None, 1000, all).unwrap();
    assert!(newest.iter().all(|(seq, entry)| matches!(entry.what,
        What::Command { row, .. } if u64::from(row) == *seq)));
    assert_eq!(seqs(newest), (9000..10_000).collect::<Vec<_>>());
    let older = timelines.page(2, Some(9000), 1000, all).unwrap();
    assert_eq!(seqs(older), (10_000 - 1024..9000).collect::<Vec<_>>());
    assert!(
        timelines
            .page(2, Some(10_000 - 1024), 1000, all)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        seqs(timelines.page(2, Some(u64::MAX), 2, all).unwrap()),
        [9998, 9999]
    );
    let failed = |entry: &Entry| matches!(entry.what, What::Command { exit: Some(1), .. });
    let page = timelines.page(2, Some(9990), 3, failed).unwrap();
    assert_eq!(seqs(page), [9960, 9970, 9980]);
    // Reading changes nothing: no entry, no counter.
    assert_eq!(timelines.of_pane(2).unwrap().summary().commands, 10_000);
    assert!(timelines.page(3, None, 1000, all).unwrap().is_empty());
}

#[test]
fn a_non_zero_exit_counts_once_in_failed_and_errors() {
    let (_state, mut timelines) = timelines(64);
    let now = Instant::now();
    timelines.open(0, now);
    timelines.command(0, closed(Some(0)), now);
    timelines.command(0, closed(Some(2)), now);
    timelines.command(0, closed(None), now);
    let summary = timelines.of_pane(0).unwrap().summary();
    assert_eq!(
        (summary.commands, summary.failed, summary.errors),
        (3, 1, 1)
    );
}

#[test]
fn closing_shrinks_the_ring_to_2_kib_and_keeps_the_exit() {
    let (state, mut timelines) = timelines(1024);
    let now = Instant::now();
    timelines.open(1, now);
    for _ in 0..500 {
        timelines.command(1, closed(Some(0)), now);
    }
    timelines.state(1, Status::NeedsYou, true, now);
    timelines.exit(1, None, now);
    assert_eq!(file_len(&state, "r1-0"), 16 + 2048);
    let entries = timelines.entries(1).unwrap();
    assert_eq!(entries.len(), 128);
    assert_eq!(entries[127].what, What::Exit(Ended::Closed));
    assert_eq!(
        entries[126].what,
        What::State {
            status: Status::NeedsYou,
            reported: true
        }
    );
    let run = timelines.newest(1).unwrap();
    assert!(!run.is_live());
    assert_eq!((run.dropped(), run.summary().needs_you), (502 - 128, 1));
    // A closed run takes nothing more, and a short one is kept whole.
    timelines.command(1, closed(Some(0)), now);
    assert_eq!(timelines.entries(1).unwrap().len(), 128);
    timelines.open(1, now);
    timelines.command(1, closed(Some(0)), now);
    timelines.exit(1, None, now);
    assert_eq!(file_len(&state, "r1-1"), 16 + 32);
}

#[test]
fn the_timeline_file_holds_only_entries_and_is_emptied_at_start() {
    let state = tempfile::tempdir().unwrap();
    let dir = state.path().join("timeline");
    let mut timelines = Timelines::new(dir.clone(), 64).unwrap();
    timelines.open(0, Instant::now());
    timelines.command(0, closed(Some(0)), Instant::now());
    let mode = |path: &Path| fs::symlink_metadata(path).unwrap().mode() & 0o777;
    assert_eq!((mode(&dir), mode(&dir.join("r0-0"))), (0o700, 0o600));
    let outside = state.path().join("outside");
    fs::write(&outside, "keep").unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
    drop(timelines);
    let timelines = Timelines::new(dir.clone(), 64).unwrap();
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    assert_eq!(fs::read_to_string(outside).unwrap(), "keep");
    assert!(timelines.entries(0).unwrap().is_empty());
}

#[test]
fn a_directory_open_to_others_or_a_link_is_refused() {
    let state = tempfile::tempdir().unwrap();
    let open = state.path().join("open");
    fs::create_dir(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Timelines::new(open.clone(), 64).is_err());
    let link = state.path().join("link");
    std::os::unix::fs::symlink(&open, &link).unwrap();
    assert!(Timelines::new(link, 64).is_err());
}

#[test]
fn a_symlinked_or_foreign_ring_file_is_refused() {
    let (state, mut timelines) = timelines(64);
    let target = state.path().join("target");
    fs::write(&target, "untouched").unwrap();
    std::os::unix::fs::symlink(&target, state.path().join("timeline/r4-0")).unwrap();
    timelines.open(4, Instant::now());
    assert!(timelines.of_pane(4).is_none());
    timelines.command(4, closed(Some(0)), Instant::now());
    assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");

    // A closed run's file swapped for a link is not read through it.
    timelines.open(5, Instant::now());
    timelines.exit(5, None, Instant::now());
    let path = state.path().join("timeline/r5-1");
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(timelines.entries(5).is_err());

    // Someone else's file, or one open to others, is not ours.
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    let uid = pinfold_sys::effective_uid();
    assert!(ours(&fs::metadata(&target).unwrap(), uid));
    assert!(!ours(&fs::metadata(&target).unwrap(), uid + 1));
    fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!ours(&fs::metadata(&target).unwrap(), uid));
}

#[test]
fn the_oldest_closed_run_goes_at_64_with_its_file() {
    let (state, mut timelines) = timelines(64);
    let now = Instant::now();
    for pane in 0..MAX_RUNS {
        timelines.open(pane, now);
        timelines.exit(pane, None, now);
    }
    timelines.open(100, now);
    assert!(timelines.of_pane(100).is_some());
    assert!(timelines.newest(0).is_none());
    assert!(fs::symlink_metadata(state.path().join("timeline/r0-0")).is_err());
    assert_eq!(timelines.runs.len(), MAX_RUNS);
}

proptest! {
    #[test]
    fn garbled_bytes_are_skipped_on_read_never_a_panic(
        garbage in proptest::collection::vec(any::<u8>(), 0..2100),
        live in any::<bool>(),
        before in proptest::option::of(0..200_u64),
        count in 1..200_usize,
    ) {
        let (state, mut timelines) = timelines(64);
        let now = Instant::now();
        timelines.open(0, now);
        for _ in 0..70 {
            timelines.command(0, closed(Some(1)), now);
        }
        if !live {
            timelines.exit(0, None, now);
        }
        // The file is rewritten under the core, shorter or longer than it thinks.
        let path = state.path().join("timeline/r0-0");
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(0).unwrap();
        file.write_all_at(&garbage, 0).unwrap();
        let page = timelines.page(0, before, count, |entry| entry.at % 2 == 0).unwrap();
        prop_assert!(page.len() <= count);
        prop_assert!(page.windows(2).all(|pair| pair[0].0 < pair[1].0));
        prop_assert!(page.iter().all(|(seq, _)| before.is_none_or(|before| *seq < before)));
        let entries = timelines.entries(0).unwrap();
        prop_assert!(entries.len() <= 128);
        for entry in entries {
            prop_assert_eq!(Entry::decode(&entry.encode()), Some(entry));
        }
    }
}

fn call(seq: u64, caller: u64, method: &str, target: Option<u64>, ok: bool) -> pinfold_api::Record {
    pinfold_api::Record {
        seq,
        at_ms: 0,
        caller: Some(caller),
        terminal: true,
        method: method.to_owned(),
        target,
        scope: "self".to_owned(),
        outcome: if ok {
            pinfold_api::Outcome::Ok("allowed".to_owned())
        } else {
            pinfold_api::Outcome::Refused("only the caller's own pane".to_owned())
        },
    }
}

/// Decision 3: reads that succeed never reach the ring, so a polling agent cannot flush it;
/// a refused read is recorded in the caller's run alone, so it cannot flush the target's, and
/// a call that acts in each run it touches.
#[test]
fn a_polling_agent_adds_no_entries_and_a_refused_call_adds_one() {
    let (_state, mut timelines) = timelines(64);
    let mut log = Log::new();
    let now = Instant::now();
    timelines.open(2, now);
    timelines.open(3, now);
    for seq in 0..1_000 {
        log.append(call(seq, 2, "pane.list", None, true)).unwrap();
        timelines.calls(&log, now);
    }
    log.append(call(0, 2, "pane.wait", Some(2), true)).unwrap();
    log.append(call(0, 2, "pane.read", Some(2), true)).unwrap();
    log.append(call(0, 2, "hello", None, true)).unwrap();
    timelines.calls(&log, now);
    assert!(timelines.entries(2).unwrap().is_empty());
    assert_eq!(timelines.of_pane(2).unwrap().summary().calls, 0);

    log.append(call(0, 2, "pane.read", Some(3), false)).unwrap();
    log.append(call(0, 2, "pane.send", Some(2), true)).unwrap();
    log.append(call(0, 2, "pane.close", Some(3), true)).unwrap();
    timelines.calls(&log, now);
    // Looking again records nothing twice.
    timelines.calls(&log, now);
    let entries: Vec<What> = timelines
        .entries(2)
        .unwrap()
        .iter()
        .map(|e| e.what)
        .collect();
    let api = |seq, method, outcome, target| What::Api {
        seq,
        method,
        outcome,
        target,
    };
    assert_eq!(
        entries,
        [
            api(1_003, 4, Outcome::Refused, Some(3)),
            api(1_004, 6, Outcome::Ok, Some(2)),
            api(1_005, 11, Outcome::Ok, Some(3)),
        ]
    );
    // An `api` entry's method index is also the audit log's (ADR-0053).
    assert_eq!(METHODS.map(|(name, _)| name), pinfold_api::audit::METHODS);
    let target: Vec<What> = timelines
        .entries(3)
        .unwrap()
        .iter()
        .map(|e| e.what)
        .collect();
    assert_eq!(target, [api(1_005, 11, Outcome::Ok, Some(3))]);
    assert_eq!(timelines.of_pane(2).unwrap().summary().calls, 3);
    assert_eq!(timelines.of_pane(3).unwrap().summary().calls, 1);
}

#[test]
fn approvals_are_recorded_in_the_runs_they_are_about_and_counted() {
    let (_state, mut timelines) = timelines(64);
    let now = Instant::now();
    timelines.open(1, now);
    timelines.open(2, now);
    timelines.approval(1, 2, ApprovalAction::Send, Answer::Once, 5, now);
    timelines.approval(1, 2, ApprovalAction::Read, Answer::Denied, 6, now);
    timelines.approval(0, 2, ApprovalAction::Rewind, Answer::Expired, 7, now);
    // A trust or owner prompt is about no pane, whatever its numbers.
    timelines.approval(0, 1, ApprovalAction::Trust, Answer::Once, 8, now);
    timelines.approval(0, 0, ApprovalAction::Owner, Answer::Denied, 9, now);
    let approval = |action, answer, other, grant| What::Approval {
        action,
        answer,
        other,
        grant,
    };
    let kinds = |pane| -> Vec<What> {
        timelines
            .entries(pane)
            .unwrap()
            .iter()
            .map(|e| e.what)
            .collect()
    };
    assert_eq!(
        kinds(1),
        [
            approval(ApprovalAction::Send, Answer::Once, Some(2), 5),
            approval(ApprovalAction::Read, Answer::Denied, Some(2), 6),
        ]
    );
    assert_eq!(
        kinds(2),
        [
            approval(ApprovalAction::Send, Answer::Once, Some(1), 5),
            approval(ApprovalAction::Read, Answer::Denied, Some(1), 6),
            approval(ApprovalAction::Rewind, Answer::Expired, None, 7),
        ]
    );
    let summary = |pane| {
        let summary = timelines.of_pane(pane).unwrap().summary();
        (summary.approvals, summary.denied)
    };
    assert_eq!((summary(1), summary(2)), ((2, 1), (3, 1)));
}

/// A checkpoint's `changed` becomes the summary's; a restore's counts what it undid and leaves
/// it; an exit take that lands after its run closed is still kept, in the closed file.
#[test]
fn checkpoints_carry_their_changed_count_into_live_and_closed_runs() {
    let (state, mut timelines) = timelines(64);
    let now = Instant::now();
    timelines.open(4, now);
    timelines.checkpoint(4, 0, "launch", true, 0, now);
    timelines.checkpoint(4, 1, "done", false, 8, now);
    timelines.checkpoint(4, 2, "restore", true, 3, now);
    timelines.checkpoint(4, 3, "unknown", true, 1, now);
    timelines.checkpoint(5, 0, "launch", true, 1, now);
    assert_eq!(timelines.of_pane(4).unwrap().summary().changed, 8);
    timelines.exit(4, None, now);
    timelines.checkpoint(4, 4, "exit", true, 9, now);
    let checkpoint = |n, reason, complete, changed| What::Checkpoint {
        n,
        reason,
        complete,
        changed,
    };
    let kinds: Vec<What> = timelines
        .entries(4)
        .unwrap()
        .iter()
        .map(|e| e.what)
        .collect();
    assert_eq!(
        kinds,
        [
            checkpoint(0, 0, true, 0),
            checkpoint(1, 1, false, 8),
            checkpoint(2, 5, true, 3),
            What::Exit(Ended::Closed),
            checkpoint(4, 2, true, 9),
        ]
    );
    assert_eq!(timelines.newest(4).unwrap().summary().changed, 9);
    assert_eq!(file_len(&state, "r4-0"), 16 + 5 * 16);
    assert!(timelines.newest(5).is_none());
}
