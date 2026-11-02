//! The pinfold and pinfold-core binaries.

use std::io::{self, Write};

/// Makes a panic print only its place. Stderr may be the session's log, and a panic's text can
/// quote pane contents, as a bad `str` slice quotes the string.
///
/// # Examples
///
/// ```
/// pinfold_cli::panics_name_no_text();
/// ```
pub fn panics_name_no_text() {
    std::panic::set_hook(Box::new(|info| {
        let place = info.location().map(ToString::to_string);
        let place = place.as_deref().unwrap_or("an unknown place");
        // Not `eprintln!`, which would panic again on a full disk.
        let _ = writeln!(io::stderr(), "pinfold: panicked at {place}");
    }));
}
