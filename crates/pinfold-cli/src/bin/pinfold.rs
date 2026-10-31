//! The `pinfold` binary.

#[allow(clippy::print_stdout, reason = "binary main")]
fn main() {
    println!(
        "pinfold {} (pre-alpha, not functional yet)",
        env!("CARGO_PKG_VERSION")
    );
}
