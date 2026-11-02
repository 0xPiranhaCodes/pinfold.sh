//! Window process: winit, chrome as cells, input and IME.

mod flash;
mod ime;
mod input;
mod mouse;
mod paste;
mod reload;
mod search_bar;
mod selection;
mod window;

pub use window::{Error, run};
