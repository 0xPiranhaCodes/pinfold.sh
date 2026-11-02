//! Window process: winit, chrome as cells, input and IME.

mod flash;
mod ime;
mod input;
mod mouse;
mod paste;
mod reload;
mod search_bar;
mod selection;
mod shell;
mod top_bar;
mod vi;
mod window;

pub use window::{Error, run};

#[cfg(test)]
mod tests {
    use pinfold_core::vt::{AlacrittyEngine, VtEngine};
    use pinfold_proto::{DEFAULT_BACKGROUND, Frame};

    #[test]
    fn the_core_and_the_renderers_agree_on_the_default_background() {
        let mut frame = Frame::default();
        AlacrittyEngine::new(1, 1).frame(&mut frame);
        assert_eq!(frame.cells[0].background, DEFAULT_BACKGROUND);
    }
}
