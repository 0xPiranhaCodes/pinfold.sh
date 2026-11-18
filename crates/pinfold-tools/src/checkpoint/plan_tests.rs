use super::*;
use crate::checkpoint::take::{take, tests::settled};

const RUN: &str = "r100-0";

fn scratch(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("pinfold-plan-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("work/src")).expect("work");
    fs::write(dir.join("work/a.txt"), "a\n").expect("a");
    fs::write(dir.join("work/src/b.rs"), "b\n").expect("b");
    let dir = dir.canonicalize().expect("dir");
    (dir.join("state"), dir.join("work"), dir)
}

fn checkpoint(state: &Path, work: &Path, n: u64, reason: &str) {
    settled(|| take(state, RUN, n, reason, work, 100 + n)).expect("take");
}

fn listed(planned: &Planned) -> Vec<(&str, &str, bool)> {
    let paths = planned.paths.iter();
    paths
        .map(|(path, what, later)| (path.as_str(), *what, *later))
        .collect()
}

#[test]
fn a_plan_counts_what_a_restore_would_do_and_marks_paths_changed_later() {
    let (state, work, dir) = scratch("counts");
    checkpoint(&state, &work, 0, "launch");
    fs::write(work.join("a.txt"), "agent\n").expect("a");
    fs::remove_file(work.join("src/b.rs")).expect("b");
    fs::write(work.join("new.txt"), "new\n").expect("new");
    checkpoint(&state, &work, 1, "done");
    fs::write(work.join("a.txt"), "person\n").expect("a");
    checkpoint(&state, &work, 2, "view");

    let planned = plan(&state, RUN, 0, 2, &work, None).expect("plan");
    let counts = (
        planned.restore,
        planned.delete,
        planned.bring_back,
        planned.not_covered,
    );
    assert_eq!(counts, (1, 1, 1, 0));
    assert_eq!(
        (planned.later, planned.complete, planned.total()),
        (1, true, 3)
    );
    assert_eq!(
        listed(&planned),
        [
            ("a.txt", "restore", true),
            ("new.txt", "delete", false),
            ("src/b.rs", "bring back", false),
        ]
    );

    let one = plan(&state, RUN, 0, 2, &work, Some(vec![b"src/b.rs".to_vec()])).expect("plan");
    assert_eq!(listed(&one), [("src/b.rs", "bring back", false)]);
    let none = plan(&state, RUN, 0, 2, &work, Some(vec![b"gone".to_vec()])).expect("plan");
    assert_eq!(none.total(), 0);
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_plan_of_a_checkpoint_a_prune_removed_fails_rather_than_counting_part() {
    let (state, work, dir) = scratch("gone");
    checkpoint(&state, &work, 0, "launch");
    fs::write(work.join("a.txt"), "agent\n").expect("a");
    checkpoint(&state, &work, 1, "view");
    let store = store_of(&state, &work).expect("store");
    git::store(&store, ["update-ref", "-d", "refs/pinfold/r100-0/1"], b"").expect("prune");
    let error = plan(&state, RUN, 0, 1, &work, None).expect_err("gone");
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert_eq!(error.to_string(), "checkpoint r100-0/1 no longer exists");
    git::store(&store, ["update-ref", "-d", "refs/pinfold/r100-0/0"], b"").expect("prune");
    let error = plan(&state, RUN, 0, 1, &work, None).expect_err("gone");
    assert_eq!(error.to_string(), "checkpoint r100-0/0 no longer exists");
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_path_the_checkpoint_did_not_cover_is_never_planned_for_deletion() {
    let (state, work, dir) = scratch("uncovered");
    fs::create_dir_all(work.join("inner/.git")).expect("nested");
    fs::write(work.join("inner/keep.txt"), "theirs\n").expect("keep");
    checkpoint(&state, &work, 0, "launch");
    fs::remove_dir(work.join("inner/.git")).expect("unnest");
    fs::write(work.join("added.txt"), "agent\n").expect("added");
    checkpoint(&state, &work, 1, "view");
    let planned = plan(&state, RUN, 0, 1, &work, None).expect("plan");
    assert_eq!((planned.delete, planned.not_covered), (1, 1));
    assert_eq!(
        listed(&planned),
        [
            ("added.txt", "delete", false),
            ("inner/keep.txt", "not covered", false)
        ]
    );
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_plan_takes_at_most_a_thousand_paths_and_a_run_id() {
    let (state, work, dir) = scratch("bound");
    checkpoint(&state, &work, 0, "launch");
    let paths = (0..=MAX_PATHS)
        .map(|i| format!("f{i}").into_bytes())
        .collect();
    let error = plan(&state, RUN, 0, 0, &work, Some(paths)).expect_err("bound");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    let error = plan(&state, "../x", 0, 0, &work, None).expect_err("run");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_path_above_a_chosen_one_is_chosen_with_it() {
    let chosen = Selection::new(Some(vec![b"src/b.rs".to_vec(), b"x".to_vec()])).expect("ok");
    assert!(chosen.has(b"src") && chosen.has(b"src/b.rs") && chosen.has(b"x"));
    assert!(!chosen.has(b"sr") && !chosen.has(b"src/b") && !chosen.has(b"src-b"));
    assert!(Selection::new(None).expect("all").has(b"anything"));
}

#[test]
fn a_line_with_long_escaped_paths_still_fits_what_the_core_reads() {
    let path = "\u{1}".repeat(SHOWN_BYTES);
    let paths: Vec<_> = (0..SHOWN)
        .map(|_| serde_json::json!([path, "restore", false]))
        .collect();
    let mut line = serde_json::json!({ "restore": 10, "more": 0, "paths": paths });
    assert!(line.to_string().len() > LINE);
    fit(&mut line, "paths", "more");
    assert!(line.to_string().len() <= LINE);
    let kept = line["paths"].as_array().map_or(0, Vec::len) as u64;
    assert_eq!(
        line["more"].as_u64().map(|more| more + kept),
        Some(SHOWN as u64)
    );
    assert_eq!(display("é".repeat(200).as_bytes()).len(), SHOWN_BYTES);
}
