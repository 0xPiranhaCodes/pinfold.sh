//! Any `.gitignore` text and path, matched as the checkpoint walk matches them, never panic and
//! finish within the fuzzer's timeout: the matcher's step budget holds on hostile patterns.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pinfold_tools::fuzz_gitignore;

fuzz_target!(|data: &[u8]| {
    // The text, then a NUL, then the path; no NUL means an empty path.
    let mut parts = data.splitn(2, |&byte| byte == 0);
    let text = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    let [as_dir, as_file] = fuzz_gitignore(text, path);
    // A pattern that only matches directories can only add to what a directory matches. Each
    // call has its own step budget, and the directory's, with more patterns to try, can run out
    // where the file's did not: only a directory call that finished must have an answer.
    if let (Some(Some(true)), Some(as_dir)) = (as_file, as_dir) {
        assert!(as_dir.is_some());
    }
});
