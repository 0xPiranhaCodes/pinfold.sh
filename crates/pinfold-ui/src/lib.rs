//! Window process: winit, chrome as cells, input and IME.

mod ime;
mod input;
mod mouse;
mod paste;
mod window;

pub use window::{Error, run};
