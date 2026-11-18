use super::*;
use crate::git;
use std::fs;
use std::io::Cursor;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pinfold-walk-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("root")).expect("scratch");
    dir
}

fn write(path: PathBuf, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, text).expect("write");
}

fn id(bytes: &[u8]) -> [u8; 20] {
    let mut hash = Sha1::new();
    hash.update(format!("blob {}\0", bytes.len()));
    hash.update(bytes);
    hash.finalize().into()
}

fn records(mut stat: &[u8]) -> Vec<Record> {
    let mut line = Vec::new();
    std::iter::from_fn(|| Record::read(&mut stat, &mut line)).collect()
}

fn paths(stat: &[u8]) -> Vec<String> {
    let path = |r: Record| String::from_utf8_lossy(&r.path).into_owned();
    records(stat).into_iter().map(path).collect()
}

fn skipped(walked: &Walked) -> Vec<(String, Why)> {
    let skip = |(p, w): &(Vec<u8>, Why)| (String::from_utf8_lossy(p).into_owned(), *w);
    walked.skipped.iter().map(skip).collect()
}

/// A walk of `root` with two readers into memory: counts, stat list, and every blob written.
fn take(root: &Path, caps: &Caps, previous: Option<&[u8]>) -> (Walked, Vec<u8>, Vec<u8>) {
    let mut stat = Vec::new();
    let mut previous = previous.map(Cursor::new);
    let previous = previous.as_mut().map(|p| p as &mut dyn BufRead);
    let importers = vec![Vec::new(), Vec::new()];
    let (walked, importers) = walk(root, caps, previous, &mut stat, importers).expect("walk");
    (walked, stat, importers.concat())
}

/// An importer that runs `hook` before every write.
struct Spy<F: FnMut() -> io::Result<()>> {
    out: Vec<u8>,
    hook: F,
}

impl<F: FnMut() -> io::Result<()>> Write for Spy<F> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        (self.hook)()?;
        self.out.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A previous stat list that runs `hook` once the walk starts reading the record at `at`,
/// which it does just after `fstatat` of that path and before queueing it.
struct Tripwire {
    data: Cursor<Vec<u8>>,
    at: u64,
    hook: Option<Box<dyn FnOnce()>>,
}

impl Read for Tripwire {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.data.read(buf)
    }
}

impl BufRead for Tripwire {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.data.position() >= self.at
            && let Some(hook) = self.hook.take()
        {
            hook();
        }
        self.data.fill_buf()
    }
    fn consume(&mut self, n: usize) {
        self.data.consume(n);
    }
}

/// A stat list naming `paths` with a stat no file has, so none is reused, and where the
/// record of `paths[trip]` starts.
fn bogus(paths: &[&str], trip: usize) -> (Vec<u8>, u64) {
    let (mut list, mut at) = (Vec::new(), 0);
    for (i, path) in paths.iter().enumerate() {
        if i == trip {
            at = list.len() as u64;
        }
        let stat = Stat {
            kind: Kind::File,
            mode: 0o644,
            size: 1,
            links: 1,
            device: 0,
            inode: 0,
            modified_ns: 0,
            changed_ns: 0,
        };
        Record::new(0o100644, &stat, path.as_bytes())
            .write(&mut list)
            .expect("list");
    }
    (list, at)
}

fn walk_tripped(
    root: &Path,
    list: Vec<u8>,
    at: u64,
    hook: Box<dyn FnOnce()>,
) -> (Walked, Vec<u8>, Vec<u8>) {
    let mut previous = Tripwire {
        data: Cursor::new(list),
        at,
        hook: Some(hook),
    };
    let mut stat = Vec::new();
    let caps = Caps::default();
    let (walked, out) = walk(
        root,
        &caps,
        Some(&mut previous),
        &mut stat,
        vec![Vec::new()],
    )
    .expect("walk");
    (walked, stat, out.concat())
}

#[test]
fn files_and_links_are_covered_in_walk_order_and_no_link_is_followed() {
    let dir = scratch("cover");
    let root = dir.join("root");
    write(dir.join("secret"), "SECRET");
    write(root.join("b.txt"), "b\n");
    write(root.join("a/run"), "#!/bin/sh\n");
    fs::set_permissions(root.join("a/run"), fs::Permissions::from_mode(0o755)).expect("chmod");
    symlink("../secret", root.join("link")).expect("link");
    symlink(&dir, root.join("up")).expect("link");
    let (walked, stat, blobs) = take(&root, &Caps::default(), None);
    assert_eq!(paths(&stat), ["a/run", "b.txt", "link", "up"]);
    let records = records(&stat);
    let modes: Vec<_> = records.iter().map(|r| r.mode).collect();
    assert_eq!(modes, [0o100755, 0o100644, 0o120000, 0o120000]);
    assert_eq!(records[1].id, id(b"b\n"));
    assert_eq!(records[2].id, id(b"../secret"), "a link is its target text");
    assert!(
        !blobs.windows(6).any(|w| w == b"SECRET"),
        "no link was followed"
    );
    assert!(walked.complete && walked.files == 4 && walked.read == 4 && walked.uncovered == 0);
    let root_link = dir.join("root-link");
    symlink(&root, &root_link).expect("link");
    let refused = walk(
        &root_link,
        &Caps::default(),
        None,
        &mut Vec::new(),
        vec![Vec::new()],
    );
    assert!(
        refused.is_err(),
        "a working directory that is a link is refused"
    );
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn git_data_and_nested_repositories_are_skipped_and_listed() {
    let dir = scratch("git");
    let root = dir.join("root");
    write(root.join(".git/HEAD"), "ref: refs/heads/main\n");
    write(root.join("nested/.Git/config"), "[core]\n");
    write(root.join("nested/code.rs"), "x");
    write(
        root.join("vendor/lib/.git"),
        "gitdir: ../../.git/modules/lib\n",
    );
    write(root.join("vendor/lib/code.rs"), "x");
    write(root.join("vendor/own.rs"), "x");
    write(root.join(".g\u{200c}it/hooks/post-checkout"), "x");
    let (walked, stat, _) = take(&root, &Caps::default(), None);
    let mut expected = vec![
        (".git".to_owned(), Why::Git),
        ("nested".to_owned(), Why::Nested),
        ("vendor/lib".to_owned(), Why::Nested),
    ];
    if cfg!(target_os = "macos") {
        expected.insert(1, (".g\u{200c}it".to_owned(), Why::Git));
        assert_eq!(paths(&stat), ["vendor/own.rs"]);
    } else {
        assert_eq!(
            paths(&stat),
            [".g\u{200c}it/hooks/post-checkout", "vendor/own.rs"]
        );
    }
    assert_eq!(skipped(&walked), expected);
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn hfs_aliases_of_dotgit_are_gits() {
    for alias in [
        ".git",
        ".GIT",
        ".g\u{200c}it",
        "\u{feff}.Git\u{206a}",
        ".gi\u{200f}t",
    ] {
        assert!(is_hfs_dotgit(alias.as_bytes()), "{alias:?}");
    }
    for name in [".gitx", "git", ".gi", ".g\u{e9}it", ".git/"] {
        assert!(!is_hfs_dotgit(name.as_bytes()), "{name:?}");
    }
    assert!(!is_hfs_dotgit(b".git\xff"), "not UTF-8");
}

#[test]
fn hard_links_special_files_and_large_files_are_skipped() {
    let dir = scratch("special");
    let root = dir.join("root");
    write(dir.join("credential"), "token");
    fs::hard_link(dir.join("credential"), root.join("linked")).expect("hard link");
    write(root.join("big"), "0123456789!");
    write(root.join("small"), "0123456789");
    let made = std::process::Command::new("mkfifo")
        .arg(root.join("fifo"))
        .status();
    assert!(made.is_ok_and(|s| s.success()), "mkfifo");
    let caps = Caps {
        max_file_bytes: 10,
        ..Caps::default()
    };
    let (walked, stat, blobs) = take(&root, &caps, None);
    assert_eq!(paths(&stat), ["small"]);
    let expected = [
        ("big", Why::Large),
        ("fifo", Why::Special),
        ("linked", Why::HardLink),
    ];
    assert_eq!(skipped(&walked), expected.map(|(p, w)| (p.to_owned(), w)));
    assert!(!blobs.windows(5).any(|w| w == b"token"));
    assert!(walked.complete);
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn max_files_counts_files_and_links_not_directories() {
    let dir = scratch("files");
    let root = dir.join("root");
    write(root.join("d1/d2/d3/f1"), "1");
    write(root.join("d1/f2"), "2");
    symlink("f2", root.join("d1/l")).expect("link");
    let caps = |max_files| Caps {
        max_files,
        ..Caps::default()
    };
    let (walked, stat, _) = take(&root, &caps(3), None);
    assert!(walked.complete && walked.files == 3, "{walked:?}");
    let (walked, stat_cut, _) = take(&root, &caps(2), None);
    assert!(!walked.complete && walked.files == 2, "{walked:?}");
    assert_eq!(paths(&stat_cut), paths(&stat)[..2]);
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn max_new_bytes_counts_only_what_is_read() {
    let dir = scratch("new");
    let root = dir.join("root");
    write(root.join("a"), "0123456789");
    let caps = Caps {
        max_new_bytes: 15,
        ..Caps::default()
    };
    let (walked, first, _) = take(&root, &caps, None);
    assert!(walked.complete && walked.new_bytes == 10);
    write(root.join("b"), "0123456789");
    let (walked, _, _) = take(&root, &caps, None);
    assert!(!walked.complete && walked.files == 1, "{walked:?}");
    let (walked, _, _) = take(&root, &caps, Some(&first));
    assert!(
        walked.complete && walked.reused == 1 && walked.new_bytes == 10,
        "{walked:?}"
    );
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn gitignore_is_honoured_and_env_files_are_covered_where_the_walk_goes() {
    let dir = scratch("ignore");
    let root = dir.join("root");
    write(root.join(".gitignore"), "*.log\nbuild/\n.env*\n!keep.log\n");
    write(root.join("a.log"), "x");
    write(root.join("keep.log"), "x");
    write(root.join(".env.local"), "TOKEN=x");
    write(root.join("build/.env"), "TOKEN=y");
    write(root.join("build/out.o"), "x");
    write(root.join("src/.gitignore"), "/gen\n");
    write(root.join("src/gen/x"), "x");
    write(root.join("src/gen.rs"), "x");
    write(root.join("src/deep/gen"), "x");
    write(dir.join("everything"), "*\n");
    write(root.join("linked/file"), "x");
    symlink("../../everything", root.join("linked/.gitignore")).expect("link");
    let (walked, stat, _) = take(&root, &Caps::default(), None);
    let covered = [
        ".env.local",
        ".gitignore",
        "keep.log",
        "linked/.gitignore",
        "linked/file",
        "src/.gitignore",
        "src/deep/gen",
        "src/gen.rs",
    ];
    assert_eq!(
        paths(&stat),
        covered,
        "a .gitignore that is a link is not read"
    );
    let expected = [
        ("a.log", Why::Ignored),
        ("build", Why::Ignored),
        ("src/gen", Why::Ignored),
    ];
    assert_eq!(skipped(&walked), expected.map(|(p, w)| (p.to_owned(), w)));
    let caps = Caps {
        include_ignored: true,
        ..Caps::default()
    };
    let (walked, stat, _) = take(&root, &caps, None);
    assert!(
        walked.skipped.is_empty() && paths(&stat).len() == 12,
        "{:?}",
        paths(&stat)
    );
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_take_on_the_previous_list_reads_only_changes_and_matches_one_from_scratch() {
    let dir = scratch("merge");
    let root = dir.join("root");
    for i in 0..50 {
        write(
            root.join(format!("d{}/f{i:02}", i % 5)),
            &format!("file {i}\n"),
        );
    }
    let (_, first, _) = take(&root, &Caps::default(), None);
    write(root.join("d0/f00"), "changed, and longer\n");
    write(root.join("d9/new"), "new\n");
    fs::remove_file(root.join("d1/f01")).expect("rm");
    let (walked, merged, _) = take(&root, &Caps::default(), Some(&first));
    assert_eq!(
        (walked.reused, walked.read, walked.files),
        (48, 2, 50),
        "{walked:?}"
    );
    let (walked, scratch, _) = take(&root, &Caps::default(), None);
    assert_eq!(walked.read, 50);
    let ids = |list: &[u8]| -> Vec<_> {
        records(list)
            .into_iter()
            .map(|r| (r.path, r.mode, r.id))
            .collect()
    };
    assert_eq!(ids(&merged), ids(&scratch));
    let (walked, _, blobs) = take(&root, &Caps::default(), Some(&merged));
    assert!(
        walked.reused == 50 && blobs.is_empty(),
        "an unchanged take reads nothing"
    );
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_link_planted_for_a_directory_mid_walk_is_recorded_not_followed() {
    let dir = scratch("plant-dir");
    let root = dir.join("root");
    write(dir.join("elsewhere/inside"), "SECRET");
    write(root.join("a"), "a");
    write(root.join("m"), "m");
    write(root.join("z/inside"), "z");
    let (list, at) = bogus(&["a", "m"], 1);
    let (from, to) = (root.clone(), dir.clone());
    // Runs once the walk has listed the root and is at `m`, before it reaches `z`.
    let plant = Box::new(move || {
        fs::rename(from.join("z"), to.join("z-moved")).expect("move");
        symlink(to.join("elsewhere"), from.join("z")).expect("plant");
    });
    let (walked, stat, blobs) = walk_tripped(&root, list, at, plant);
    assert_eq!(paths(&stat), ["a", "m", "z"]);
    assert_eq!(records(&stat)[2].mode, 0o120000);
    assert!(!blobs.windows(6).any(|w| w == b"SECRET"));
    assert!(walked.complete);
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_parent_swapped_for_a_link_mid_walk_is_still_read_through_its_descriptor() {
    let dir = scratch("plant-parent");
    let root = dir.join("root");
    write(dir.join("elsewhere/m"), "SECRET");
    write(root.join("z/a"), "a");
    write(root.join("z/m"), "m");
    let (list, at) = bogus(&["z/a", "z/m"], 1);
    let (from, to) = (root.clone(), dir.clone());
    let plant = Box::new(move || {
        fs::rename(from.join("z"), to.join("z-moved")).expect("move");
        symlink(to.join("elsewhere"), from.join("z")).expect("plant");
    });
    let (_, stat, blobs) = walk_tripped(&root, list, at, plant);
    assert_eq!(paths(&stat), ["z/a", "z/m"]);
    assert_eq!(records(&stat)[1].id, id(b"m"));
    assert!(!blobs.windows(6).any(|w| w == b"SECRET"));
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_file_swapped_for_a_link_after_the_walk_saw_it_is_not_read() {
    let dir = scratch("plant-file");
    let root = dir.join("root");
    write(dir.join("secret"), "SECRET");
    write(root.join("a"), "a");
    write(root.join("b"), "b");
    let (list, at) = bogus(&["a", "b"], 1);
    let (from, to) = (root.clone(), dir.clone());
    // Runs after `fstatat` of `b`, before a reader opens it.
    let plant = Box::new(move || {
        fs::remove_file(from.join("b")).expect("rm");
        symlink(to.join("secret"), from.join("b")).expect("plant");
    });
    let (walked, stat, blobs) = walk_tripped(&root, list, at, plant);
    assert_eq!(paths(&stat), ["a"]);
    assert_eq!(skipped(&walked), [("b".to_owned(), Why::Changed)]);
    assert!(!blobs.windows(6).any(|w| w == b"SECRET"));
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_file_that_keeps_changing_while_read_is_left_uncovered() {
    let dir = scratch("changing");
    let root = dir.join("root");
    write(root.join("log"), "start\n");
    let log = root.join("log");
    let grow = move || {
        fs::OpenOptions::new()
            .append(true)
            .open(&log)?
            .write_all(b"more\n")
    };
    let mut stat = Vec::new();
    let spy = Spy {
        out: Vec::new(),
        hook: grow,
    };
    let (walked, _) = walk(&root, &Caps::default(), None, &mut stat, vec![spy]).expect("walk");
    assert!(stat.is_empty());
    assert_eq!(skipped(&walked), [("log".to_owned(), Why::Changed)]);
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_failed_importer_fails_the_walk() {
    let dir = scratch("failed");
    let root = dir.join("root");
    for i in 0..600 {
        write(root.join(format!("f{i:03}")), "x");
    }
    let broken = || Err(io::Error::other("git exited"));
    let spies = vec![
        Spy {
            out: Vec::new(),
            hook: broken,
        },
        Spy {
            out: Vec::new(),
            hook: broken,
        },
    ];
    assert!(walk(&root, &Caps::default(), None, &mut Vec::new(), spies).is_err());
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn deep_directories_and_many_open_ones_are_bounded() {
    let dir = scratch("deep");
    let root = dir.join("root");
    let mut deep = root.clone();
    for _ in 0..=MAX_DEPTH {
        deep.push("d");
    }
    write(deep.join("f"), "x");
    for i in 0..(MAX_OPEN_DIRS * 3) {
        write(root.join(format!("wide/{i:03}/f")), "x");
    }
    let mut stat = Vec::new();
    let (walked, _) =
        walk(&root, &Caps::default(), None, &mut stat, vec![Vec::new()]).expect("walk");
    let deepest = "d/".repeat(MAX_DEPTH) + "d";
    assert_eq!(skipped(&walked), [(deepest, Why::Deep)]);
    assert_eq!(walked.files as usize, MAX_OPEN_DIRS * 3);
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn records_round_trip_and_a_broken_list_ends_the_merge() {
    let stat = Stat {
        kind: Kind::File,
        mode: 0o644,
        size: 7,
        links: 1,
        device: 1,
        inode: 99,
        modified_ns: -5,
        changed_ns: 1_700_000_000_123_456_789,
    };
    let mut record = Record::new(0o100755, &stat, b"dir/a name\nwith a newline");
    record.id = id(b"x");
    let mut list = Vec::new();
    record.write(&mut list).expect("write");
    list.extend_from_slice(b"100644 nothex 1 2 3 4 broken\0");
    assert_eq!(records(&list), [record]);
}

#[test]
fn a_directory_too_large_to_list_is_skipped_and_the_rest_covered() {
    let dir = scratch("names");
    let root = dir.join("root");
    fs::create_dir_all(root.join("huge")).expect("mkdir");
    for i in 0..=MAX_NAMES {
        fs::File::create(root.join(format!("huge/{i:05}"))).expect("create");
    }
    write(root.join("small"), "x");
    let (walked, stat, _) = take(&root, &Caps::default(), None);
    assert_eq!(paths(&stat), ["small"]);
    assert_eq!(skipped(&walked), [("huge".to_owned(), Why::Large)]);
    assert!(walked.complete);
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn a_gitignore_that_runs_out_of_steps_is_dropped_for_the_rest_of_the_walk() {
    let dir = scratch("bomb");
    let root = dir.join("root");
    // Exponential in git's matcher; checked first, as the last line.
    write(
        root.join(".gitignore"),
        &format!("*.log\n{}x\n", "**/".repeat(300)),
    );
    let mut deep = root.clone();
    for _ in 0..40 {
        deep.push("a");
    }
    write(deep.join("y"), "y");
    write(root.join("z.log"), "z");
    let start = std::time::Instant::now();
    let (_, stat, _) = take(&root, &Caps::default(), None);
    assert!(start.elapsed().as_secs() < 5, "{:?}", start.elapsed());
    let paths = paths(&stat);
    assert!(paths.contains(&"z.log".to_owned()), "{paths:?}");
    fs::remove_dir_all(dir).expect("clean");
}

/// A take into `store` at `refs/pinfold/t/<n>`, on `parent`'s list and commit if given, with
/// two readers each feeding its own `git fast-import`, then one more for the commit.
fn take_into(store: &Path, root: &Path, n: &str, parent: Option<&str>) -> Walked {
    let stat_path = store.join(format!("stat/t-{n}"));
    let list = |n: &str| fs::read(store.join(format!("stat/t-{n}"))).expect("list");
    let previous = parent.map(list);
    let mut stat = Vec::new();
    let mut old = previous.as_deref();
    let old = old.as_mut().map(|p| p as &mut dyn BufRead);
    let importers = vec![
        git::import(store).expect("git"),
        git::import(store).expect("git"),
    ];
    let (walked, importers) =
        walk(root, &Caps::default(), old, &mut stat, importers).expect("walk");
    for import in importers {
        import.finish().expect("blobs");
    }
    fs::write(&stat_path, &stat).expect("stat");
    let mut import = git::import(store).expect("git");
    let base = parent.map(|p| {
        let tree = store_git(
            store,
            &[
                "for-each-ref",
                "--format=%(tree)",
                &format!("refs/pinfold/t/{p}"),
            ],
        );
        tree.trim().to_owned()
    });
    let mut parent_list = previous.as_deref();
    let parent = base
        .as_deref()
        .zip(parent_list.as_mut().map(|p| p as &mut dyn BufRead));
    let refname = format!("refs/pinfold/t/{n}");
    commit(&mut import, &refname, "{}", 1, &mut &stat[..], parent).expect("commit");
    import.finish().expect("commit");
    walked
}

fn store_git(store: &Path, args: &[&str]) -> String {
    let out = git::store(store, args, b"").expect("git");
    assert!(
        out.status.is_some_and(|s| s.success()),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_commit_on_the_previous_tree_lists_only_changes_and_has_the_tree_of_one_from_scratch() {
    let dir = scratch("commit");
    let (root, store) = (dir.join("root"), dir.join("store"));
    git::create_store(&store).expect("store");
    fs::create_dir(store.join("stat")).expect("stat");
    for i in 0..30 {
        write(
            root.join(format!("d{}/f{i:02}", i % 3)),
            &format!("file {i}\n"),
        );
    }
    write(root.join("odd \"name\"\nwith a newline"), "x");
    symlink("d0/f00", root.join("link")).expect("link");
    take_into(&store, &root, "0", None);
    write(root.join("d0/f00"), "changed\n");
    write(root.join("d9/new"), "new\n");
    fs::remove_file(root.join("d1/f01")).expect("rm");
    let walked = take_into(&store, &root, "1", Some("0"));
    assert_eq!((walked.read, walked.reused), (2, 30));
    take_into(&store, &root, "scratch", None);
    let tree = |n: &str| {
        store_git(
            &store,
            &[
                "for-each-ref",
                "--format=%(tree)",
                &format!("refs/pinfold/t/{n}"),
            ],
        )
    };
    assert_eq!(tree("1"), tree("scratch"));
    let diff = store_git(
        &store,
        &[
            "diff-tree",
            "-r",
            "--name-status",
            "refs/pinfold/t/0",
            "refs/pinfold/t/1",
        ],
    );
    assert_eq!(diff, "M\td0/f00\nD\td1/f01\nA\td9/new\n");
    let parent = store_git(
        &store,
        &["for-each-ref", "--format=%(parent)", "refs/pinfold/t/1"],
    );
    assert_eq!(parent.trim(), "", "no parent, so pruning 0 can free it");
    let listed = store_git(
        &store,
        &[
            "diff-tree",
            "-r",
            "--root",
            "--name-only",
            "-z",
            "refs/pinfold/t/scratch",
        ],
    );
    assert!(
        listed.contains("odd \"name\"\nwith a newline"),
        "{listed:?}"
    );
    let config = fs::read_to_string(store.join("config")).expect("config");
    assert!(config.contains("compression = 1") && config.contains("bare = true"));
    let mode = fs::metadata(&store).expect("store").permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    assert!(
        git::create_store(&store).is_err(),
        "never made over an existing one"
    );
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn an_import_dropped_before_its_end_leaves_no_commit() {
    let dir = scratch("dropped");
    let store = dir.join("store");
    git::create_store(&store).expect("store");
    let mut import = git::import(&store).expect("git");
    commit(
        &mut import,
        "refs/pinfold/t/0",
        "{}",
        1,
        &mut &b""[..],
        None,
    )
    .expect("commit");
    import.flush().expect("flush");
    drop(import);
    assert_eq!(store_git(&store, &["for-each-ref"]), "");
    let mut import = Vec::new();
    let bad = commit(
        &mut import,
        "refs/pinfold/t/0\nreset refs/heads/x",
        "{}",
        1,
        &mut &b""[..],
        None,
    );
    assert_eq!(bad.map_err(|e| e.kind()), Err(io::ErrorKind::InvalidInput));
    let broken = commit(
        &mut import,
        "refs/pinfold/t/0",
        "{}",
        1,
        &mut &b"100644 x\0"[..],
        None,
    );
    assert_eq!(
        broken.map_err(|e| e.kind()),
        Err(io::ErrorKind::InvalidData)
    );
    fs::remove_dir_all(dir).expect("clean");
}

#[test]
fn nothing_a_hostile_work_tree_configures_runs_in_a_take_or_a_diff() {
    let dir = scratch("hostile");
    let (root, store) = (dir.join("root"), dir.join("store"));
    git::create_store(&store).expect("store");
    fs::create_dir(store.join("stat")).expect("stat");
    let marker = dir.join("marker");
    let touch = format!("touch {}", marker.display());
    let config = format!(
        "[core]\n\tfsmonitor = {touch}\n\thooksPath = hooks\n\tpager = {touch}\n\
         [filter \"evil\"]\n\tclean = {touch}\n\tsmudge = {touch}\n\
         [diff \"evil\"]\n\ttextconv = {touch}\n\tcommand = {touch}\n"
    );
    write(root.join(".git/config"), &config);
    write(
        root.join(".git/hooks/post-commit"),
        &format!("#!/bin/sh\n{touch}\n"),
    );
    write(root.join(".gitattributes"), "* filter=evil diff=evil\n");
    write(root.join("nested/.git/config"), &config);
    write(root.join("nested/code.rs"), "x");
    write(root.join("a.txt"), "one\n");
    take_into(&store, &root, "0", None);
    write(root.join("a.txt"), "two\n");
    take_into(&store, &root, "1", Some("0"));
    let diff = store_git(&store, &["diff", "refs/pinfold/t/0", "refs/pinfold/t/1"]);
    assert!(diff.contains("-one\n+two\n"), "{diff}");
    let blob = store_git(
        &store,
        &["for-each-ref", "--format=%(objectname)", "refs/pinfold/t/1"],
    );
    let batch = git::store(&store, ["cat-file", "--batch"], blob.as_bytes()).expect("git");
    assert!(batch.status.is_some_and(|s| s.success()));
    assert!(!marker.exists(), "a command the work tree named ran");
    fs::remove_dir_all(dir).expect("clean");
}
