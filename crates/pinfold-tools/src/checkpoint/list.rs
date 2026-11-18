//! `pinfold checkpoint list` (ADR-0050 section 7): a run's checkpoints, read from the store on
//! demand for `checkpoint.list`, so the core keeps none. Metadata only, never a path.

use std::io;
use std::path::Path;

use super::plan::{message, numbers, store_by_key};
use super::take::is_run;

/// The checkpoints of `run` in the store `key` under `state`, newest first, each
/// `{"checkpoint":"<run>/<n>","reason","time","files","new_bytes","complete"}` from its commit
/// message. One pruned while this reads is left out.
///
/// # Errors
///
/// Refuses a bad run or key and a store a take would refuse; otherwise a failed git.
///
/// # Examples
///
/// ```no_run
/// use pinfold_tools::checkpoint::list::list;
///
/// let listed = list("/path/to/state".as_ref(), "r1700000000-0", "0123456789abcdef")?;
/// println!("{} checkpoints", listed.len());
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn list(state: &Path, run: &str, key: &str) -> io::Result<Vec<serde_json::Value>> {
    if !is_run(run) {
        let bad = format!("not a run: {run}");
        return Err(io::Error::new(io::ErrorKind::InvalidInput, bad));
    }
    let store = store_by_key(state, key)?;
    let mut ns = numbers(&store, run)?;
    ns.sort_unstable_by(|a, b| b.cmp(a));
    let mut listed = Vec::with_capacity(ns.len());
    for n in ns {
        let Some(message) = message(&store, run, n)? else {
            continue;
        };
        listed.push(serde_json::json!({
            "checkpoint": format!("{run}/{n}"),
            "reason": message["reason"],
            "time": message["time"],
            "files": message["files"],
            "new_bytes": message["new_bytes"],
            "complete": message["complete"],
        }));
    }
    Ok(listed)
}

#[cfg(test)]
mod tests {
    use super::super::Caps;
    use super::super::take::{Dir, take_next};
    use super::*;

    #[test]
    fn a_run_lists_newest_first_with_no_paths_and_takes_number_from_the_store() {
        let dir = std::env::temp_dir().join(format!("pinfold-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("work")).expect("work");
        std::fs::write(dir.join("work/secret-name.txt"), "a\n").expect("file");
        let dir = dir.canonicalize().expect("dir");
        let (state, work, caps) = (dir.join("state"), dir.join("work"), Caps::default());

        let launch = take_next(&state, "r5-0", "launch", Dir::Path(&work), 5, &caps);
        let launch = launch.expect("launch");
        assert_eq!(launch.checkpoint, "r5-0/0");
        // By key, as the core takes every checkpoint after launch.
        let done = take_next(&state, "r5-0", "done", Dir::Key(&launch.store), 6, &caps);
        assert_eq!(done.expect("done").checkpoint, "r5-0/1");
        let other = take_next(&state, "r6-0", "launch", Dir::Path(&work), 7, &caps);
        assert_eq!(other.expect("other run").checkpoint, "r6-0/0");

        let listed = list(&state, "r5-0", &launch.store).expect("list");
        let ids: Vec<_> = listed.iter().map(|c| c["checkpoint"].clone()).collect();
        assert_eq!(ids, ["r5-0/1", "r5-0/0"]);
        assert_eq!(listed[1]["reason"], "launch");
        assert_eq!(
            (listed[1]["files"].as_u64(), listed[1]["complete"].as_bool()),
            (Some(1), Some(true))
        );
        assert!(
            !serde_json::Value::Array(listed)
                .to_string()
                .contains("secret-name")
        );

        assert!(list(&state, "r5-0", "../../etc").is_err());
        assert!(
            take_next(
                &state,
                "r5-0",
                "done",
                Dir::Key("ffffffffffffffff"),
                8,
                &caps
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_store_whose_dir_names_another_directory_is_refused() {
        let dir = std::env::temp_dir().join(format!("pinfold-list-moved-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("work")).expect("work");
        std::fs::create_dir_all(dir.join("other")).expect("other");
        let dir = dir.canonicalize().expect("dir");
        let (state, caps) = (dir.join("state"), Caps::default());
        let launch = take_next(
            &state,
            "r5-0",
            "launch",
            Dir::Path(&dir.join("work")),
            5,
            &caps,
        );
        let key = launch.expect("launch").store;
        let file = state.join("checkpoints").join(&key).join("dir");
        std::fs::remove_file(&file).expect("rm");
        std::fs::write(&file, dir.join("other").as_os_str().as_encoded_bytes()).expect("write");
        let refused = take_next(&state, "r5-0", "api", Dir::Key(&key), 6, &caps);
        assert_eq!(
            refused.expect_err("moved").kind(),
            io::ErrorKind::PermissionDenied
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_working_directory_swapped_for_a_link_after_launch_is_refused_by_key() {
        let dir = std::env::temp_dir().join(format!("pinfold-list-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("work/sub")).expect("work");
        std::fs::create_dir_all(dir.join("elsewhere")).expect("elsewhere");
        let dir = dir.canonicalize().expect("dir");
        let (state, caps) = (dir.join("state"), Caps::default());
        let (work, sub) = (dir.join("work"), dir.join("work/sub"));
        let launch = take_next(&state, "r5-0", "launch", Dir::Path(&sub), 5, &caps);
        let key = launch.expect("launch").store;
        // The agent moves the tree away and links its name, or a parent's, somewhere else.
        std::fs::rename(&work, dir.join("moved")).expect("move");
        std::os::unix::fs::symlink(dir.join("elsewhere"), &work).expect("link");
        std::fs::create_dir_all(dir.join("elsewhere/sub")).expect("sub");
        let refused = take_next(&state, "r5-0", "api", Dir::Key(&key), 6, &caps);
        let refused = refused.expect_err("swapped");
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            refused.to_string().contains("resolves elsewhere"),
            "{refused}"
        );
        let listed = list(&state, "r5-0", &key).expect("list");
        assert_eq!(listed.len(), 1, "nothing taken of the other tree");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
