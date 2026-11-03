//! API socket server, authorization and audit log.

pub mod audit;
pub mod control;
pub mod redact;
pub mod spawn;
pub mod trust;

pub use audit::{Broken, Entry, Error, Log, Outcome, Record, verify_file};
pub use redact::{Kind, redact};
