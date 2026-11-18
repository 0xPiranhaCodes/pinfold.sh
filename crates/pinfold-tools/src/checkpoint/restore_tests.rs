use super::*;
use crate::checkpoint::take::{take, tests::settled};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::PathBuf;

const RUN: &str = "r100-0";

/// A working directory, a directory outside it that links point to, and the scratch root.
fn scratch(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("pinfold-restore-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("work/src")).expect("work");
    fs::create_dir_all(dir.join("outside")).expect("outside");
    fs::write(dir.join("work/a.txt"), "a\n").expect("a");
    fs::write(dir.join("work/src/b.rs"), "b\n").expect("b");
    fs::write(dir.join("work/run.sh"), "#!/bin/sh\n").expect("run");
    fs::set_permissions(dir.join("work/run.sh"), Permissions::from_mode(0o755)).expect("mode");
    let dir = dir.canonicalize().expect("dir");
    let (state, work, outside) = (dir.join("state"), dir.join("work"), dir.join("outside"));
    (state, work, outside, dir)
}

fn checkpoint(state: &Path, work: &Path, n: u64, reason: &str) {
    settled(|| take(state, RUN, n, reason, work, 100 + n)).expect("take");
}

fn restored(state: &Path, work: &Path, c: u64, shown: u64) -> Restored {
    settled(|| restore(state, RUN, c, shown, work, None, 200)).expect("restore")
}

/// Every entry under `dir`: a file's mode and text, a link's target, `dir` for a directory.
fn tree(dir: &Path) -> BTreeMap<String, String> {
    let mut entries = BTreeMap::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(next) = todo.pop() {
        for entry in fs::read_dir(&next).expect("list") {
            let path = entry.expect("entry").path();
            let name = path.strip_prefix(dir).expect("under").display().to_string();
            let meta = fs::symlink_metadata(&path).expect("meta");
            let value = if meta.is_symlink() {
                format!("-> {}", fs::read_link(&path).expect("link").display())
            } else if meta.is_dir() {
                todo.push(path);
                "dir".to_owned()
            } else {
                let text = fs::read_to_string(&path).expect("read");
                format!("{:o} {text}", meta.mode() & 0o777)
            };
            entries.insert(name, value);
        }
    }
    entries
}

#[test]
fn a_restore_takes_its_checkpoint_first_and_that_checkpoint_undoes_it() {
    let (state, work, _, dir) = scratch("round");
    let launch = tree(&work);
    checkpoint(&state, &work, 0, "launch");
    fs::write(work.join("a.txt"), "agent\n").expect("a");
    // Not just the file: a checkpoint, as a git tree, holds no empty directory.
    fs::remove_dir_all(work.join("src")).expect("src");
    fs::create_dir_all(work.join("new/deep")).expect("new");
    fs::write(work.join("new/deep/x.txt"), "x\n").expect("x");
    fs::set_permissions(work.join("run.sh"), Permissions::from_mode(0o644)).expect("mode");
    checkpoint(&state, &work, 1, "view");
    let agent = tree(&work);

    let done = restored(&state, &work, 0, 1);
    assert_eq!((done.restored, done.deleted, done.skipped), (3, 1, 0));
    assert_eq!(done.restore_checkpoint, "r100-0/2");
    let mask = pinfold_sys::umask();
    let mode = fs::metadata(work.join("run.sh")).expect("run").mode() & 0o777;
    assert_eq!(mode, 0o755 & !mask);
    if mask == 0o022 {
        assert_eq!(tree(&work), launch, "the new directories are gone too");
    }

    // The restore checkpoint, restored over a view of now, is the tree before the restore.
    checkpoint(&state, &work, 3, "view");
    let done = restored(&state, &work, 2, 3);
    assert_eq!((done.restored, done.deleted, done.skipped), (3, 1, 0));
    assert_eq!(done.restore_checkpoint, "r100-0/4");
    assert_eq!(tree(&work), agent);
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_file_and_a_directory_the_agent_swapped_swap_back() {
    let (state, work, _, dir) = scratch("swap");
    let launch = tree(&work);
    checkpoint(&state, &work, 0, "launch");
    fs::remove_file(work.join("a.txt")).expect("a");
    fs::create_dir(work.join("a.txt")).expect("dir");
    fs::write(work.join("a.txt/in"), "in\n").expect("in");
    fs::remove_dir_all(work.join("src")).expect("src");
    fs::write(work.join("src"), "file\n").expect("src");
    checkpoint(&state, &work, 1, "view");
    let done = restored(&state, &work, 0, 1);
    assert_eq!((done.restored, done.deleted, done.skipped), (2, 2, 0));
    assert_eq!(tree(&work), launch);
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_path_changed_since_shown_is_skipped_and_reported() {
    let (state, work, _, dir) = scratch("since");
    checkpoint(&state, &work, 0, "launch");
    fs::write(work.join("a.txt"), "agent\n").expect("a");
    fs::write(work.join("src/b.rs"), "agent\n").expect("b");
    checkpoint(&state, &work, 1, "view");
    fs::write(work.join("a.txt"), "person\n").expect("a");
    let done = restored(&state, &work, 0, 1);
    assert_eq!((done.restored, done.skipped), (1, 1));
    assert_eq!(done.skipped_paths, [("a.txt".to_owned(), "changed")]);
    assert_eq!(
        fs::read_to_string(work.join("a.txt")).expect("a"),
        "person\n"
    );
    assert_eq!(fs::read_to_string(work.join("src/b.rs")).expect("b"), "b\n");
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn an_incomplete_checkpoint_deletes_nothing() {
    let (state, work, _, dir) = scratch("incomplete");
    checkpoint(&state, &work, 0, "launch");
    let store = plan::store_of(&state, &work).expect("store");
    let caps = Caps {
        max_files: 1,
        ..Caps::default()
    };
    let lock = settled(|| lock(&store)).expect("lock");
    let taken = take_in(&state, &store, &work, RUN, 1, "api", 101, &caps).expect("take");
    drop(lock);
    assert!(!taken.complete);
    fs::write(work.join("a.txt"), "agent\n").expect("a");
    fs::write(work.join("new.txt"), "agent\n").expect("new");
    checkpoint(&state, &work, 2, "view");
    let done = restored(&state, &work, 1, 2);
    assert_eq!((done.restored, done.deleted), (1, 0));
    assert_eq!(fs::read_to_string(work.join("a.txt")).expect("a"), "a\n");
    for kept in ["new.txt", "src/b.rs", "run.sh"] {
        assert!(work.join(kept).exists(), "{kept}");
    }
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn paths_the_checkpoint_did_not_cover_or_not_chosen_are_never_touched() {
    let (state, work, outside, dir) = scratch("uncovered");
    fs::create_dir_all(work.join("inner/.git")).expect("nested");
    fs::write(work.join("inner/keep.txt"), "theirs\n").expect("keep");
    fs::write(work.join("linked.txt"), "linked\n").expect("linked");
    fs::hard_link(work.join("linked.txt"), outside.join("linked.txt")).expect("hard link");
    checkpoint(&state, &work, 0, "launch");
    // Now N covers both, which C skipped.
    fs::remove_dir(work.join("inner/.git")).expect("unnest");
    fs::remove_file(outside.join("linked.txt")).expect("unlink");
    fs::write(work.join("a.txt"), "agent\n").expect("a");
    fs::write(work.join("added.txt"), "agent\n").expect("added");
    checkpoint(&state, &work, 1, "view");

    let chosen = Some(vec![b"a.txt".to_vec()]);
    let done = settled(|| restore(&state, RUN, 0, 1, &work, chosen.clone(), 200)).expect("one");
    assert_eq!((done.restored, done.deleted), (1, 0));
    assert!(work.join("added.txt").exists());

    // a.txt, restored just now, has changed since shown.
    let done = restored(&state, &work, 0, 1);
    assert_eq!((done.restored, done.deleted, done.skipped), (0, 1, 1));
    assert_eq!(done.skipped_paths, [("a.txt".to_owned(), "changed")]);
    assert!(!work.join("added.txt").exists());
    assert_eq!(
        fs::read_to_string(work.join("inner/keep.txt")).expect("keep"),
        "theirs\n"
    );
    assert_eq!(
        fs::read_to_string(work.join("linked.txt")).expect("linked"),
        "linked\n"
    );
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn links_planted_after_the_view_are_skipped_and_never_followed() {
    let (state, work, outside, dir) = scratch("planted");
    checkpoint(&state, &work, 0, "launch");
    fs::write(work.join("a.txt"), "agent\n").expect("a");
    fs::write(work.join("src/b.rs"), "agent b\n").expect("b");
    fs::remove_file(work.join("run.sh")).expect("run");
    checkpoint(&state, &work, 1, "view");
    // The same text as N, behind a file link, a directory link and a link above a path N lacks.
    fs::write(outside.join("a.txt"), "agent\n").expect("a");
    fs::create_dir(outside.join("src")).expect("src");
    fs::write(outside.join("src/b.rs"), "agent b\n").expect("b");
    fs::remove_file(work.join("a.txt")).expect("a");
    symlink(outside.join("a.txt"), work.join("a.txt")).expect("file link");
    fs::rename(work.join("src"), dir.join("moved")).expect("move");
    symlink(outside.join("src"), work.join("src")).expect("dir link");
    let before = tree(&outside);

    let done = restored(&state, &work, 0, 1);
    assert_eq!((done.restored, done.deleted, done.skipped), (1, 0, 2));
    let skipped: Vec<_> = done
        .skipped_paths
        .iter()
        .map(|(p, why)| (p.as_str(), *why))
        .collect();
    assert_eq!(skipped, [("a.txt", "changed"), ("src/b.rs", "changed")]);
    assert_eq!(tree(&outside), before, "nothing outside was written");
    assert!(
        fs::symlink_metadata(work.join("src"))
            .expect("src")
            .is_symlink()
    );

    // A path N lacks under a directory link planted since: written only once the link goes.
    fs::remove_file(work.join("src")).expect("unlink");
    checkpoint(&state, &work, 3, "view");
    symlink(&outside, work.join("src")).expect("dir link");
    let done = settled(|| {
        restore(
            &state,
            RUN,
            0,
            3,
            &work,
            Some(vec![b"src/b.rs".to_vec()]),
            300,
        )
    })
    .expect("restore");
    assert_eq!(done.skipped_paths, [("src/b.rs".to_owned(), "changed")]);
    assert_eq!(tree(&outside), before);
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_hard_link_is_replaced_and_its_other_name_never_written() {
    let (state, work, outside, dir) = scratch("hard");
    checkpoint(&state, &work, 0, "launch");
    fs::write(work.join("a.txt"), "agent\n").expect("a");
    checkpoint(&state, &work, 1, "view");
    fs::write(outside.join("secret"), "agent\n").expect("secret");
    fs::remove_file(work.join("a.txt")).expect("a");
    fs::hard_link(outside.join("secret"), work.join("a.txt")).expect("hard link");
    let done = restored(&state, &work, 0, 1);
    assert_eq!((done.restored, done.skipped), (1, 0));
    assert_eq!(
        fs::read_to_string(outside.join("secret")).expect("secret"),
        "agent\n"
    );
    assert_eq!(fs::read_to_string(work.join("a.txt")).expect("a"), "a\n");
    assert_eq!(
        fs::metadata(outside.join("secret"))
            .expect("secret")
            .nlink(),
        1
    );
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_directory_link_the_view_showed_goes_and_the_directory_comes_back() {
    let (state, work, outside, dir) = scratch("shown");
    let launch = tree(&work);
    checkpoint(&state, &work, 0, "launch");
    fs::rename(work.join("src"), dir.join("moved")).expect("move");
    symlink(&outside, work.join("src")).expect("dir link");
    checkpoint(&state, &work, 1, "view");
    let done = restored(&state, &work, 0, 1);
    assert_eq!((done.restored, done.deleted, done.skipped), (1, 1, 0));
    assert_eq!(tree(&work), launch);
    assert!(
        tree(&outside).is_empty(),
        "nothing was written through the link"
    );
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_restore_refuses_over_a_thousand_paths_and_a_held_lock_before_any_change() {
    let (state, work, _, dir) = scratch("refused");
    checkpoint(&state, &work, 0, "launch");
    fs::write(work.join("a.txt"), "agent\n").expect("a");
    checkpoint(&state, &work, 1, "view");
    let store = plan::store_of(&state, &work).expect("store");

    let paths = (0..=plan::MAX_PATHS)
        .map(|i| format!("f{i}").into_bytes())
        .collect();
    let error = restore(&state, RUN, 0, 1, &work, Some(paths), 200).expect_err("bound");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

    let held = settled(|| lock(&store)).expect("lock");
    let error = restore(&state, RUN, 0, 1, &work, None, 200).expect_err("locked");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    drop(held);

    assert_eq!(
        numbers(&store, RUN).expect("refs").len(),
        2,
        "no restore checkpoint"
    );
    assert_eq!(
        fs::read_to_string(work.join("a.txt")).expect("a"),
        "agent\n"
    );
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn nothing_a_hostile_work_tree_configures_runs_in_a_plan_or_a_restore() {
    let (state, work, _, dir) = scratch("hostile");
    let marker = dir.join("marker");
    let touch = format!("touch {}", marker.display());
    let config = format!(
        "[core]\n\tfsmonitor = {touch}\n\thooksPath = hooks\n\
         [filter \"evil\"]\n\tclean = {touch}\n\tsmudge = {touch}\n\tprocess = {touch}\n"
    );
    fs::create_dir_all(work.join(".git/hooks")).expect("git");
    fs::write(work.join(".git/config"), config).expect("config");
    fs::write(
        work.join(".git/hooks/post-checkout"),
        format!("#!/bin/sh\n{touch}\n"),
    )
    .expect("hook");
    fs::write(work.join(".gitattributes"), "* filter=evil\n").expect("attributes");
    checkpoint(&state, &work, 0, "launch");
    fs::write(work.join("a.txt"), "agent\n").expect("a");
    checkpoint(&state, &work, 1, "view");
    let planned = plan::plan(&state, RUN, 0, 1, &work, None).expect("plan");
    assert_eq!(planned.restore, 1);
    assert_eq!(restored(&state, &work, 0, 1).restored, 1);
    assert!(!marker.exists(), "a command the work tree named ran");
    fs::remove_dir_all(dir).expect("clean");
}
