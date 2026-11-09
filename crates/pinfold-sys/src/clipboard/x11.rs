//! Reads the X11 clipboard within a limit. x11-clipboard's `load` collects a whole INCR
//! transfer, and reserves the size the owner announces, before its caller sees a byte (#310).

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use x11_clipboard::{Context, RustConnection};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, GetPropertyReply, Property};

use super::{Unread, utf8};

/// Returns the clipboard's text, or why there is none. The server sends at most `limit` bytes
/// and a word, so no more reaches this process.
///
/// Each read has its own connection and window. ICCCM cannot end an INCR transfer, so an owner
/// whose transfer was refused keeps writing whenever the property is deleted, which would mix
/// its chunks into the next read's.
pub(super) fn text(limit: usize, timeout: Duration) -> Result<String, Unread> {
    let deadline = Instant::now() + timeout;
    let context = &Context::new(None).map_err(|_| Unread::Empty)?;
    let (connection, window, atoms) = (&context.connection, context.window, &context.atoms);
    let asked = connection
        .delete_property(window, atoms.property)
        .and_then(|_| {
            connection.convert_selection(
                window,
                atoms.clipboard,
                atoms.utf8_string,
                atoms.property,
                x11rb::CURRENT_TIME,
            )
        })
        .and_then(|_| connection.flush());
    asked.map_err(|_| Unread::Empty)?;
    // Some once the owner has started an INCR transfer.
    let mut chunks: Option<Vec<u8>> = None;
    while Instant::now() < deadline {
        let Some(event) = connection.poll_for_event().map_err(|_| Unread::Empty)? else {
            wait(connection, deadline);
            continue;
        };
        match event {
            Event::SelectionNotify(event) if event.requestor == window => {
                if event.property == x11rb::NONE {
                    return Err(Unread::Empty);
                }
                let reply = get(context, false, limit)?;
                if reply.type_ != atoms.incr {
                    let mut text = Vec::new();
                    add(&mut text, &reply, limit)?;
                    let _ = connection.delete_property(window, atoms.property);
                    let _ = connection.flush();
                    return utf8(text);
                }
                // The size an owner announces is only a lower bound, so the chunks are counted,
                // into room that never has to grow.
                chunks = Some(Vec::with_capacity(limit));
                // Deleting the property asks the owner for the first chunk.
                let next = connection.delete_property(window, atoms.property);
                next.map_err(|_| Unread::Empty)?;
                connection.flush().map_err(|_| Unread::Empty)?;
            }
            Event::PropertyNotify(event)
                if event.window == window
                    && event.atom == atoms.property
                    && event.state == Property::NEW_VALUE =>
            {
                let Some(text) = chunks.as_mut() else {
                    continue;
                };
                // Deleting it as it is read asks for the next chunk; an empty one ends.
                let reply = get(context, true, limit - text.len())?;
                if reply.value.is_empty() {
                    return utf8(std::mem::take(text));
                }
                add(text, &reply, limit)?;
            }
            _ => {}
        }
    }
    Err(Unread::Empty)
}

/// Waits until the server has sent something or `deadline` passes. x11-clipboard sleeps 50 ms
/// instead, which makes an INCR transfer of 4,000-byte chunks crawl.
fn wait(connection: &RustConnection, deadline: Instant) {
    let left = deadline
        .saturating_duration_since(Instant::now())
        .as_millis();
    let mut socket = libc::pollfd {
        fd: connection.stream().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid `pollfd`, with its count.
    unsafe { libc::poll(&raw mut socket, 1, i32::try_from(left).unwrap_or(i32::MAX)) };
}

/// Reads the property, asking for at most `limit` bytes and one more word.
fn get(context: &Context, delete: bool, limit: usize) -> Result<GetPropertyReply, Unread> {
    let words = u32::try_from(limit / 4 + 1).unwrap_or(u32::MAX);
    let property = context.atoms.property;
    let asked =
        context
            .connection
            .get_property(delete, context.window, property, AtomEnum::ANY, 0, words);
    asked
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .ok_or(Unread::Empty)
}

/// Adds a property's value to `text`, unless the two pass `limit`, which `text` never does.
fn add(text: &mut Vec<u8>, reply: &GetPropertyReply, limit: usize) -> Result<(), Unread> {
    if reply.bytes_after > 0 || reply.value.len() > limit - text.len() {
        return Err(Unread::TooLarge);
    }
    text.extend_from_slice(&reply.value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(value: &[u8], bytes_after: u32) -> GetPropertyReply {
        GetPropertyReply {
            format: 8,
            sequence: 0,
            length: 0,
            type_: 0,
            bytes_after,
            value_len: value.len() as u32,
            value: value.to_vec(),
        }
    }

    #[test]
    fn chunks_add_up_to_the_limit_and_no_further() {
        let mut text = Vec::new();
        assert_eq!(add(&mut text, &reply(b"ab", 0), 4), Ok(()));
        assert_eq!(add(&mut text, &reply(b"cd", 0), 4), Ok(()));
        assert_eq!(add(&mut text, &reply(b"e", 0), 4), Err(Unread::TooLarge));
        assert_eq!(text, b"abcd");
        // What the server held back past the asked length is over the limit too.
        assert_eq!(
            add(&mut Vec::new(), &reply(b"ab", 1), 4),
            Err(Unread::TooLarge)
        );
    }

    /// Needs an X server. CI's Linux jobs have none, so `tests/window/clipboard.sh` runs it
    /// under Xvfb, with `xclip` as the owner.
    #[test]
    fn a_large_x11_clipboard_is_refused_and_a_small_one_read() {
        if std::env::var_os("DISPLAY").is_none() {
            eprintln!("skipped: no DISPLAY");
            return;
        }
        let owner = |text: &[u8]| {
            use std::io::Write;
            let mut xclip = std::process::Command::new("xclip")
                .args(["-selection", "clipboard", "-i"])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            xclip.stdin.take().unwrap().write_all(text).unwrap();
            assert!(xclip.wait().unwrap().success());
        };
        let second = Duration::from_secs(3);
        owner("é text".as_bytes());
        assert_eq!(text(1024, second), Ok("é text".to_owned()));
        assert_eq!(text(4, second), Err(Unread::TooLarge));
        // xclip sends this much by INCR, in chunks.
        let large = "x".repeat(4 * 1024 * 1024);
        owner(large.as_bytes());
        assert_eq!(text(1024 * 1024, second), Err(Unread::TooLarge));
        owner(large.as_bytes());
        let start = Instant::now();
        assert!(text(large.len(), second) == Ok(large));
        eprintln!("read 4 MiB by INCR in {:?}", start.elapsed());
    }
}
