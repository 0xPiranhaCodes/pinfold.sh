//! API socket server, authorization and audit log.

pub mod audit;
pub mod control;

pub use audit::{Broken, Entry, Error, Log, Outcome, Record, verify_file};
