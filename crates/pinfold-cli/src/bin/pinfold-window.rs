//! The `pinfold-window` binary. Only the core starts this, with the link on `LINK_FD`
//! (ADR-0028). It takes no arguments, and `--measure <width>x<height>` is the one exception: a
//! measuring core passes that, as a hardened process cannot be measured from outside on Linux.

use std::process::ExitCode;

use pinfold_sys::socket::{self, LINK_FD};

fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

fn main() -> ExitCode {
    pinfold_cli::panics_name_no_text();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let size = match args[..] {
        [] => None,
        ["--measure", text] if parse_size(text).is_some() => parse_size(text),
        _ => {
            eprintln!("pinfold-window takes no arguments: run pinfold to open a window");
            return ExitCode::FAILURE;
        }
    };
    if size.is_none()
        && let Err(error) = pinfold_sys::harden()
    {
        eprintln!("pinfold-window: cannot harden: {error}");
        return ExitCode::FAILURE;
    }
    let Ok(link) = socket::take_link() else {
        eprintln!("pinfold-window: no link to a core on descriptor {LINK_FD}: run pinfold instead");
        return ExitCode::FAILURE;
    };
    match pinfold_ui::run(link, size) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pinfold-window: {error}");
            ExitCode::FAILURE
        }
    }
}
