//! Core daemon library: PTY, VT engine, mux, sessions and status engine.

pub mod io;
pub mod link;
pub mod marks;
pub mod paste;
mod reflow;
pub mod scrollback;
pub mod search;
pub mod shell;
mod tabs;
pub mod vt;
