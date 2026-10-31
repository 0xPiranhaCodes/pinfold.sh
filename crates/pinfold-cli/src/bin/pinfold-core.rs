//! The `pinfold-core` binary.

#[allow(clippy::print_stdout, reason = "binary main")]
fn main() {
    // shortcut: parks so mem-gate has a process to measure, replaced by the event loop in #18.
    if std::env::args().nth(1).as_deref() == Some("--idle") {
        loop {
            std::thread::park();
        }
    }
    println!(
        "pinfold-core {} (pre-alpha, not functional yet)",
        env!("CARGO_PKG_VERSION")
    );
}
