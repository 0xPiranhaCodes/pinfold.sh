//! Renderer trait, CPU renderer and glyph cache.

pub mod cpu;
pub mod font;
pub mod glyphs;
#[cfg(target_os = "macos")]
pub mod metal;
pub mod paint;
