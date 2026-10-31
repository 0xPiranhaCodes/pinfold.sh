//! The `pinfold` binary.

use std::process::ExitCode;

fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let size = match args[..] {
        [] => None,
        ["--size", text] => parse_size(text),
        _ => None,
    };
    if !args.is_empty() && size.is_none() {
        eprintln!("usage: pinfold [--size <width>x<height>]");
        return ExitCode::FAILURE;
    }
    match pinfold_ui::run(size) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pinfold: {error}");
            ExitCode::FAILURE
        }
    }
}
