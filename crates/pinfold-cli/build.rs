//! Links `pinfold` without the window's frameworks, which it never calls: on macOS loading
//! AppKit, Metal and QuartzCore cost a `pinfold checkpoint take` about 1.1 MB of its 4 MB
//! (ADR-0050). The linker drops only libraries no symbol of the binary comes from.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bin=pinfold=-Wl,-dead_strip_dylibs");
    }
}
