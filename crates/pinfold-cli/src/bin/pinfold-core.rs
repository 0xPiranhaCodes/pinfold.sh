//! The `pinfold-core` binary.

#[allow(clippy::print_stdout, reason = "binary main")]
fn main() {
    println!(
        "pinfold-core {} (pre-alpha, not functional yet)",
        env!("CARGO_PKG_VERSION")
    );
}
