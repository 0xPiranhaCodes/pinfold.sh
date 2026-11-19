use std::io;

use super::*;
use crate::Tool as _;
use crate::checkpoint::take::{take, tests::settled};
use crate::diff::Viewer;
use crate::keys::Key;
use crate::screen::Screen;

const RUN: &str = "r100-0";

/// A working directory with an agent's edit, add and delete, a nested repository and a file the
/// person changed after the agent stopped, checkpointed at launch (0), done (1) and view (2).
fn scratch(name: &str) -> (PathBuf, String, PathBuf) {
    let dir = std::env::temp_dir().join(format!("pinfold-rewind-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let work = dir.join("work");
    fs::create_dir_all(work.join("src")).expect("work");
    for (path, text) in [("a.txt", "a\n"), ("c.txt", "c\n"), ("src/b.rs", "b\n")] {
        fs::write(work.join(path), text).expect("write");
    }
    let dir = dir.canonicalize().expect("dir");
    let (state, work) = (dir.join("state"), dir.join("work"));
    let at = |n: u64, reason: &str| settled(|| take(&state, RUN, n, reason, &work, 100 + n));
    let key = at(0, "launch").expect("launch").store;
    fs::write(work.join("a.txt"), "agent\n").expect("a");
    fs::remove_file(work.join("src/b.rs")).expect("b");
    fs::write(work.join("new.txt"), "new\n").expect("new");
    fs::create_dir_all(work.join("inner/.git")).expect("nested");
    fs::write(work.join("inner/code.rs"), "x\n").expect("nested");
    at(1, "done").expect("done");
    fs::write(work.join("c.txt"), "person\n").expect("c");
    at(2, "view").expect("view");
    (state, key, dir)
}

fn labels(diff: &Diff) -> Vec<String> {
    let names = diff.files.iter().map(|file| file.new.clone());
    names
        .map(|name| String::from_utf8(name).expect("utf-8"))
        .collect()
}

#[test]
fn a_checkpoint_diff_marks_files_changed_later_and_lists_nested_repositories() {
    let (state, key, dir) = scratch("load");
    let (diff, view) = load(&state, &key, ("r100-0/0", "r100-0/2"), 7).expect("load");
    assert_eq!(labels(&diff), ["a.txt", "c.txt", "new.txt", "src/b.rs"]);
    assert_eq!(view.later, [false, true, false, false]);
    assert_eq!(view.nested, ["inner"]);
    let refused = load(&state, &key, ("r100-0/0", "r101-0/2"), 7).err();
    assert_eq!(refused.as_deref(), Some("not two checkpoints of one run"));
    assert!(load(&state, "../../etc", ("r100-0/0", "r100-0/2"), 7).is_err());
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn u_names_one_file_and_cap_u_every_file_not_changed_later_never_an_empty_list() {
    let (state, key, dir) = scratch("params");
    let (diff, mut view) = load(&state, &key, ("r100-0/0", "r100-0/2"), 7).expect("load");
    let base = json!({"pane": 7, "checkpoint": "r100-0/0", "shown": "r100-0/2"});
    let mut one = base.clone();
    one["paths"] = json!(["c.txt"]);
    assert_eq!(view.params(&diff, Some(1)), Ok(one));
    let mut rest = base.clone();
    rest["paths"] = json!(["a.txt", "new.txt", "src/b.rs"]);
    assert_eq!(view.params(&diff, None), Ok(rest));
    // With nothing changed later, U sends no list, which means all.
    view.later.fill(false);
    assert_eq!(view.params(&diff, None), Ok(base));
    view.later.fill(true);
    let refused = view.params(&diff, None).unwrap_err();
    assert!(refused.starts_with("Every file changed later"), "{refused}");
    // A store whose later files could not be read refuses U; u still names one file.
    view.marked = false;
    let refused = view.params(&diff, None).unwrap_err();
    assert!(
        refused.starts_with("Files changed later cannot be told"),
        "{refused}"
    );
    assert!(view.params(&diff, Some(0)).is_ok());
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn quitting_deletes_the_view_checkpoint_unless_a_restore_used_it() {
    let (state, key, dir) = scratch("finish");
    let (_, mut view) = load(&state, &key, ("r100-0/0", "r100-0/2"), 7).expect("load");
    let exists = |view: &View| plan::exists(&view.store, RUN, 2).expect("exists");
    view.used = true;
    view.finish();
    assert!(exists(&view));
    view.used = false;
    view.finish();
    assert!(!exists(&view));
    assert!(!view.store.join("stat").join("r100-0-2").exists());
    assert!(plan::exists(&view.store, RUN, 0).expect("launch"));
    fs::remove_dir_all(dir).expect("clean");
}

/// A viewer on a checkpoint of `scratch`'s tree, at 80×24 after `keys`.
fn viewer(name: &str, keys: &[Key]) -> (Viewer, Screen, PathBuf) {
    let (state, key, dir) = scratch(name);
    let (diff, view) = load(&state, &key, ("r100-0/0", "r100-0/2"), 7).expect("load");
    let mut viewer = Viewer::new(b"rewind since r100-0/0".to_vec(), diff).rewind(view);
    for &key in keys {
        viewer.key(key);
    }
    let mut screen = Screen::new(24, 80);
    viewer.draw(&mut screen);
    (viewer, screen, dir)
}

#[test]
fn a_viewer_on_a_checkpoint() {
    let (_, screen, dir) = viewer("screen", &[]);
    insta::assert_snapshot!(screen.runs());
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_nested_repository_row_says_a_rewind_leaves_it() {
    let (mut viewer, screen, dir) = viewer("nested", &[Key::Char('n'); 4]);
    insta::assert_snapshot!(screen.runs());
    viewer.key(Key::Char('u'));
    let mut screen = Screen::new(24, 80);
    viewer.draw(&mut screen);
    assert!(
        screen
            .row(1)
            .starts_with("✕ error A nested repository is not covered")
    );
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn stage_revert_and_send_are_off_on_a_checkpoint() {
    for (key, said) in [
        ('s', "Stage and revert work on the work tree's diff alone."),
        ('r', "Stage and revert work on the work tree's diff alone."),
        ('a', "Send is off on a checkpoint."),
    ] {
        let (_, screen, dir) = viewer(&format!("off-{key}"), &[Key::Char(key)]);
        assert_eq!(screen.row(1).trim_end(), format!("✕ error {said}"), "{key}");
        fs::remove_dir_all(dir).expect("clean");
    }
}

#[test]
fn the_view_checkpoint_of_a_store_is_never_read_through_a_link() -> io::Result<()> {
    let (state, key, dir) = scratch("link");
    let store = state.join("checkpoints").join(&key);
    let moved = dir.join("moved");
    fs::rename(&store, &moved)?;
    std::os::unix::fs::symlink(&moved, &store)?;
    assert!(load(&state, &key, ("r100-0/0", "r100-0/2"), 7).is_err());
    fs::remove_dir_all(dir)
}
