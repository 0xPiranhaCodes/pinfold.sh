//! The window process: draws grids it receives from the core.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::{env, thread, time::Duration};

use spike_process_split::draw;

fn main() {
    let args: Vec<String> = env::args().collect();
    let count: usize = args[1].parse().expect("pane count");
    let socket = args[2].clone();
    let latest = Arc::new(Mutex::new(Vec::<u64>::new()));
    let shared = latest.clone();
    draw::run(count, move |proxy| {
        thread::spawn(move || {
            let mut stream = loop {
                match UnixStream::connect(&socket) {
                    Ok(stream) => break stream,
                    Err(_) => thread::sleep(Duration::from_millis(50)),
                }
            };
            let mut length = [0u8; 4];
            while stream.read_exact(&mut length).is_ok() {
                let mut bytes = vec![0u8; u32::from_le_bytes(length) as usize];
                if stream.read_exact(&mut bytes).is_err() {
                    return;
                }
                let cells = bytes.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes")));
                *shared.lock().expect("lock") = cells.collect();
                let _ = proxy.send_event(());
            }
        });
        Box::new(move |out| out.extend_from_slice(&latest.lock().expect("lock")))
    });
}
