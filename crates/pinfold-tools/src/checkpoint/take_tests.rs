use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

fn scratch(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("pinfold-take-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("work/src")).expect("work");
    fs::write(dir.join("work/a.txt"), "a\n").expect("a");
    fs::write(dir.join("work/src/b.rs"), "b\n").expect("b");
    let dir = dir.canonicalize().expect("dir");
    (dir.join("state"), dir.join("work"), dir)
}

/// `call`, tried again for up to a second while the lock looks held: in this test process
/// another test's `fork` can hold a copy of a lock just dropped until its child execs, though
/// the descriptor is close-on-exec. A take is its own process, so it never sees this.
pub(crate) fn settled<T>(call: impl Fn() -> io::Result<T>) -> io::Result<T> {
    for _ in 0..50 {
        match call() {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            other => return other,
        }
    }
    call()
}

fn store_of(state: &Path, work: &Path) -> PathBuf {
    state
        .join("checkpoints")
        .join(resolve(work).expect("key").1)
}

fn message(store: &Path, refname: &str) -> serde_json::Value {
    let out = git::store(
        store,
        ["for-each-ref", "--format=%(contents)", refname],
        b"",
    )
    .expect("git");
    serde_json::from_slice(out.stdout.trim_ascii()).expect("json")
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn a_take_makes_a_private_store_and_records_its_checkpoint() {
    let (state, work, dir) = scratch("store");
    let taken = take(&state, "r100-0", 0, "launch", &work, 100).expect("take");
    let store = store_of(&state, &work);
    assert_eq!(taken.checkpoint, "r100-0/0");
    assert_eq!(store.file_name().map(|n| n.len()), Some(16));
    assert_eq!((taken.files, taken.new_bytes, taken.complete), (2, 4, true));
    assert_eq!((mode(&store), mode(&store.join(LOCK))), (0o700, 0o600));
    assert_eq!(mode(&store.join("stat")), 0o700);
    let config = fs::read_to_string(store.join("config")).expect("config");
    assert!(config.contains("compression = 1"), "{config}");

    let launch = message(&store, "refs/pinfold/r100-0/0");
    assert_eq!(launch["run"], "r100-0");
    assert_eq!(launch["reason"], "launch");
    assert_eq!(
        (launch["n"].as_u64(), launch["time"].as_u64()),
        (Some(0), Some(100))
    );
    assert_eq!(launch["files"], 2);
    assert_eq!(
        (launch["complete"].as_bool(), launch["more"].as_bool()),
        (Some(true), Some(false))
    );

    fs::create_dir(work.join("inner")).expect("inner");
    fs::create_dir(work.join("inner/.git")).expect("nested");
    fs::write(work.join("a.txt"), "changed\n").expect("a");
    let done = settled(|| take(&state, "r100-0", 1, "done", &work, 160)).expect("take");
    assert_eq!((done.changed, done.files, done.new_bytes), (1, 2, 8));
    let done = message(&store, "refs/pinfold/r100-0/1");
    assert_eq!(done["skipped"], serde_json::json!([["inner", "nested"]]));
    let parents = git::store(
        &store,
        ["for-each-ref", "--format=%(parent)", "refs/pinfold/"],
        b"",
    );
    assert_eq!(
        parents.expect("git").stdout.trim_ascii(),
        b"",
        "no checkpoint names a parent"
    );

    let again = settled(|| take(&state, "r100-0", 1, "done", &work, 170)).expect_err("exists");
    assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
    for (run, reason) in [("r100-0", "later"), ("100-0", "done"), ("r-0", "done")] {
        let bad = take(&state, run, 2, reason, &work, 170).expect_err("bad");
        assert_eq!(bad.kind(), io::ErrorKind::InvalidInput, "{run} {reason}");
    }
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_relative_or_linked_working_directory_is_refused() {
    let (state, work, dir) = scratch("work");
    let relative = take(&state, "r1-0", 0, "launch", Path::new("work"), 1).expect_err("relative");
    assert!(relative.to_string().contains("not absolute"), "{relative}");
    symlink(&work, dir.join("link")).expect("link");
    let linked = take(&state, "r1-0", 0, "launch", &dir.join("link"), 1).expect_err("link");
    assert!(linked.to_string().contains("symbolic link"), "{linked}");
    assert!(
        !state.join("checkpoints").exists(),
        "refused before any store is made"
    );
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_store_that_is_a_link_a_file_or_open_to_others_is_refused_and_left_alone() {
    let (state, work, dir) = scratch("refused");
    let store = store_of(&state, &work);
    fs::create_dir_all(dir.join("elsewhere")).expect("elsewhere");
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(state.join("checkpoints"))
        .expect("dir");
    let cases: [(&str, &dyn Fn()); 3] = [
        ("is a symbolic link", &|| {
            symlink(dir.join("elsewhere"), &store).expect("link")
        }),
        ("is not a directory", &|| {
            fs::write(&store, "").expect("file")
        }),
        ("is open to group or others", &|| {
            fs::create_dir(&store).expect("dir");
            fs::set_permissions(&store, fs::Permissions::from_mode(0o750)).expect("mode");
        }),
    ];
    for (why, plant) in cases {
        plant();
        let refused = take(&state, "r1-0", 0, "launch", &work, 1).expect_err(why);
        assert!(refused.to_string().contains(why), "{refused}");
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        let _ = fs::remove_file(&store).or_else(|_| fs::remove_dir(&store));
    }
    assert_eq!(
        fs::read_dir(dir.join("elsewhere")).expect("dir").count(),
        0,
        "never repaired"
    );

    fs::set_permissions(state.join("checkpoints"), fs::Permissions::from_mode(0o755))
        .expect("mode");
    let open = take(&state, "r1-0", 0, "launch", &work, 1).expect_err("checkpoints open");
    assert!(
        open.to_string().contains("open to group or others"),
        "{open}"
    );
    if pinfold_sys::effective_uid() != 0 {
        let theirs = private(Path::new("/")).expect_err("root's");
        assert!(theirs.to_string().contains("another user's"), "{theirs}");
    }
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_second_take_on_a_locked_store_refuses_at_once_and_a_left_lock_file_does_not() {
    let (state, work, dir) = scratch("lock");
    take(&state, "r1-0", 0, "launch", &work, 1).expect("take");
    let store = store_of(&state, &work);
    let held = settled(|| lock(&store)).expect("lock");
    let started = std::time::Instant::now();
    let busy = take(&state, "r1-0", 1, "api", &work, 2).expect_err("locked");
    assert_eq!(busy.kind(), io::ErrorKind::WouldBlock);
    assert!(busy.to_string().contains("another checkpoint"), "{busy}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "refused, not waited"
    );
    assert!(
        !store.join("stat/r1-0-1").exists(),
        "nothing written while locked"
    );
    drop(held);
    // The file stays after every take, and only a live holder's flock refuses.
    assert!(store.join(LOCK).is_file());
    settled(|| take(&state, "r1-0", 1, "api", &work, 2)).expect("unlocked");
    fs::remove_file(store.join(LOCK)).expect("rm");
    settled(|| take(&state, "r1-0", 2, "api", &work, 3)).expect("made again");
    fs::remove_file(store.join(LOCK)).expect("rm");
    symlink(dir.join("work/a.txt"), store.join(LOCK)).expect("link");
    take(&state, "r1-0", 3, "api", &work, 4).expect_err("a linked lock is not opened");
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_full_store_refuses_every_take_with_the_reason() {
    let (state, work, dir) = scratch("full");
    take(&state, "r1-0", 0, "launch", &work, 1).expect("take");
    fs::write(
        state.join("checkpoints/full"),
        "over max_store_mib: 9 MiB of 8 MiB",
    )
    .expect("full");
    let full = settled(|| take(&state, "r1-0", 1, "api", &work, 2)).expect_err("full");
    assert!(full.to_string().starts_with("over max_store_mib"), "{full}");
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn take_reads_its_caps_and_where_the_directory_is() {
    let words = |text: &'static str| text.split(' ').map(str::as_bytes).collect::<Vec<_>>();
    let (caps, at) = parse(&words(
        "--max-files 9 --include-ignored --max-file-mib 1 -- /w",
    ))
    .expect("dir");
    assert_eq!(at, At::Dir(6));
    let want = Caps {
        max_files: 9,
        max_file_bytes: 1 << 20,
        include_ignored: true,
        ..Caps::default()
    };
    assert_eq!(caps, want);
    let (caps, at) = parse(&words("--max-new-mib 2 --key 0123456789abcdef")).expect("key");
    assert_eq!(
        (caps.max_new_bytes, at),
        (2 << 20, At::Key("0123456789abcdef"))
    );
    for bad in [
        "--max-files x -- /w",
        "--max-new-mib 99999999999999 -- /w",
        "-- /w /x",
        "--key",
        "--other 1 -- /w",
        "",
    ] {
        assert_eq!(parse(&words(bad)), None, "{bad}");
    }
}
