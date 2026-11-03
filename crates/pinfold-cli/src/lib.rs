//! The pinfold and pinfold-core binaries.

use std::io::{self, Write};

pub mod audit;

/// Reads the config file once, as the window does. A bad file means the defaults, which the
/// log hears about.
///
/// ```no_run
/// let config = pinfold_cli::config();
/// assert!(config.font_size > 0.0);
/// ```
#[must_use]
pub fn config() -> pinfold_config::Config {
    let Some(path) = pinfold_config::path() else {
        return pinfold_config::Config::default();
    };
    pinfold_config::load(&path).unwrap_or_else(|error| {
        let _ = writeln!(
            io::stderr(),
            "pinfold-core: using the defaults, not {}: {error}",
            path.display()
        );
        pinfold_config::Config::default()
    })
}

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
