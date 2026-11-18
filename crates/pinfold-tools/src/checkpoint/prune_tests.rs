use super::*;
use crate::checkpoint::take::take;
use crate::checkpoint::take::tests::settled;
use std::io::Write;

const DAY: u64 = 86_400;

fn scratch(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("pinfold-prune-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("work")).expect("work");
    let dir = dir.canonicalize().expect("dir");
    (dir.join("state"), dir.join("work"), dir)
}

/// `len` bytes that zlib cannot shrink, different for each `seed`.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Takes `run`'s checkpoint `n` at `time` with one file of its own 300 KB.
fn take_at(state: &Path, work: &Path, run: &str, n: u64, time: u64) {
    fs::write(work.join("blob"), noise(time * 100 + n, 300_000)).expect("blob");
    settled(|| take(state, run, n, "api", work, time)).expect("take");
}

/// [`prune`], tried again while a store looks locked, as [`settled`] says.
fn prune_settled(state: &Path, live: &[&str], limits: &Limits, now: u64) -> Pruned {
    settled(|| match prune(state, live, limits, now)? {
        p if p.busy > 0 => Err(io::ErrorKind::WouldBlock.into()),
        p => Ok(p),
    })
    .expect("prune")
}

fn store(state: &Path) -> PathBuf {
    let mut stores = fs::read_dir(state.join("checkpoints"))
        .expect("stores")
        .flatten();
    let store = stores.find(|e| e.file_name().len() == 16).expect("a store");
    store.path()
}

fn left(state: &Path) -> Vec<String> {
    let mut left: Vec<String> = listed(&store(state))
        .expect("refs")
        .into_iter()
        .map(|c| format!("{}/{}", c.run, c.n))
        .collect();
    left.sort();
    left
}

fn blob_id(state: &Path, run: &str, n: u64) -> String {
    let refname = format!("refs/pinfold/{run}/{n}");
    let tree = git::store(&store(state), ["diff-tree", "-r", "--root", &refname], b"");
    let out = String::from_utf8_lossy(&tree.expect("git").stdout).into_owned();
    // After the commit's own id: `:000000 100644 <zeros> <blob> A\tblob`.
    let line = out.lines().find(|l| l.starts_with(':')).expect("a file");
    line.split_whitespace().nth(3).expect("blob id").to_owned()
}

fn present(state: &Path, id: &str) -> bool {
    let input = format!("{id}\n");
    let out = git::store(&store(state), ["cat-file", "--batch"], input.as_bytes()).expect("git");
    !out.stdout.starts_with(format!("{id} missing").as_bytes())
}

#[test]
fn a_run_keeps_its_launch_and_newest_past_keep_per_run() {
    let (state, work, dir) = scratch("per-run");
    for n in 0..5 {
        take_at(&state, &work, "r1-0", n, 1000 + n);
    }
    let limits = Limits {
        keep_per_run: 3,
        ..Limits::default()
    };
    let pruned = prune_settled(&state, &[], &limits, 1100);
    assert_eq!(pruned.deleted, 2);
    assert_eq!(left(&state), ["r1-0/0", "r1-0/3", "r1-0/4"]);
    assert!(
        !store(&state).join("stat/r1-0-1").exists(),
        "its stat list goes too"
    );
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn keep_days_spares_only_a_live_runs_launch() {
    let (state, work, dir) = scratch("days");
    take_at(&state, &work, "r1-0", 0, 10);
    take_at(&state, &work, "r1-0", 1, 20);
    take_at(&state, &work, "r2-0", 0, 30);
    take_at(&state, &work, "r2-0", 1, 40);
    take_at(&state, &work, "r2-0", 2, 15 * DAY);
    let pruned = prune_settled(&state, &["r2-0"], &Limits::default(), 15 * DAY);
    assert_eq!(pruned.deleted, 3);
    assert_eq!(left(&state), ["r2-0/0", "r2-0/2"]);
    assert!(!pruned.full);
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn over_the_cap_closed_runs_go_first_and_their_objects_are_freed_by_the_repack() {
    let (state, work, dir) = scratch("cap");
    take_at(&state, &work, "r1-0", 0, 100);
    take_at(&state, &work, "r1-0", 1, 110);
    for n in 0..3 {
        take_at(&state, &work, "r2-0", n, 200 + n);
    }
    let gone = blob_id(&state, "r1-0", 0);
    assert!(present(&state, &gone));
    // Five 300 KB checkpoints: dropping the closed run's two brings them under 1 MB.
    let limits = Limits {
        max_store_bytes: 1_000_000,
        ..Limits::default()
    };
    let pruned = prune_settled(&state, &["r2-0"], &limits, 300);
    assert_eq!(left(&state), ["r2-0/0", "r2-0/1", "r2-0/2"]);
    assert_eq!(
        (pruned.deleted, pruned.repacked, pruned.full),
        (2, 1, false)
    );
    assert!(pruned.bytes <= limits.max_store_bytes, "{}", pruned.bytes);
    assert!(
        !present(&state, &gone),
        "a pruned checkpoint's own blob is gone after the repack"
    );
    assert!(present(&state, &blob_id(&state, "r2-0", 1)));
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_cap_that_needs_a_live_launch_refuses_new_checkpoints_instead() {
    let (state, work, dir) = scratch("full");
    take_at(&state, &work, "r1-0", 0, 100);
    for n in 0..3 {
        take_at(&state, &work, "r2-0", n, 200 + n);
    }
    let limits = Limits {
        max_store_bytes: 1,
        ..Limits::default()
    };
    let pruned = prune_settled(&state, &["r2-0"], &limits, 300);
    assert_eq!(
        left(&state),
        ["r2-0/0", "r2-0/2"],
        "the live launch and newest stay"
    );
    assert!(pruned.full);
    let refused = settled(|| take(&state, "r2-0", 3, "api", &work, 301)).expect_err("full");
    assert!(
        refused.to_string().starts_with("over max_store_mib"),
        "{refused}"
    );
    // Once the cap is met again, takes go on.
    prune_settled(&state, &["r2-0"], &Limits::default(), 302);
    settled(|| take(&state, "r2-0", 3, "api", &work, 303)).expect("take");
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_repack_runs_only_past_32_packs_or_512_mib_since_the_last() {
    assert!(!past_threshold(32, 512 << 20));
    assert!(past_threshold(33, 0));
    assert!(past_threshold(1, (512 << 20) + 1));

    let (state, work, dir) = scratch("repack");
    take_at(&state, &work, "r1-0", 0, 100);
    let packs = || {
        Store::packs(&Store {
            path: store(&state),
            lock: None,
            checkpoints: Vec::new(),
            bytes: 0,
            freed: false,
        })
    };
    let before = packs().0;
    assert!(before <= MAX_PACKS);
    let pruned = prune_settled(&state, &[], &Limits::default(), 200);
    assert_eq!(
        (pruned.repacked, packs().0),
        (0, before),
        "under both, no repack"
    );
    // fast-import leaves fewer than 100 new objects loose, so each pack gets 101 of its own.
    for seed in 0.. {
        if packs().0 > MAX_PACKS {
            break;
        }
        let mut import = git::import(&store(&state)).expect("git");
        for i in 0..101 {
            let blob = format!("{seed} {i}\n");
            write!(import, "blob\ndata {}\n{blob}\n", blob.len()).expect("blob");
        }
        import.finish().expect("pack");
    }
    let pruned = prune_settled(&state, &[], &Limits::default(), 200);
    assert_eq!((pruned.repacked, packs().0), (1, 1));
    let recorded = fs::read_to_string(store(&state).join(REPACKED)).expect("recorded");
    assert_eq!(recorded.parse::<u64>().ok(), Some(packs().1));
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_locked_store_is_counted_but_untouched_and_orphan_lists_go() {
    let (state, work, dir) = scratch("locked");
    take_at(&state, &work, "r1-0", 0, 10);
    let orphan = store(&state).join("stat/r1-0-7");
    fs::write(&orphan, "").expect("orphan");
    let held = lock(&store(&state)).expect("lock");
    let pruned = prune(&state, &[], &Limits::default(), 100 * DAY).expect("prune");
    assert_eq!((pruned.deleted, pruned.busy), (0, 1));
    assert!(pruned.bytes > 300_000 && orphan.exists());
    drop(held);
    let pruned = prune_settled(&state, &[], &Limits::default(), 100 * DAY);
    assert_eq!(pruned.deleted, 1);
    assert!(!orphan.exists());
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn flags_parse_and_anything_else_is_refused() {
    let (live, limits) = parse(&[
        "--live",
        "r1-0",
        "--keep-days",
        "3",
        "--max-store-mib",
        "64",
    ])
    .expect("parse");
    assert_eq!(live, ["r1-0"]);
    assert_eq!((limits.keep_days, limits.max_store_bytes), (3, 64 << 20));
    assert_eq!(parse(&[]), Some((Vec::new(), Limits::default())));
    for bad in [
        &["--live", "x"][..],
        &["--keep-days"],
        &["--keep-days", "-1"],
        &["--all", "1"],
    ] {
        assert_eq!(parse(bad), None, "{bad:?}");
    }
}
