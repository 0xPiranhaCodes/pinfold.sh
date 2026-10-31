//! The headless core: owns PTYs and VT state, and streams the visible grids to a window.

use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex, mpsc};
use std::{env, thread, time::Duration};

use spike_process_split::{snapshot, spawn_api_thread, spawn_io_thread, spawn_pane};

fn send(panes: &[Arc<spike_process_split::Pane>], stream: &mut UnixStream, cells: &mut Vec<u64>) -> bool {
    cells.clear();
    panes.iter().for_each(|pane| snapshot(pane, cells));
    let bytes: Vec<u8> = cells.iter().flat_map(|cell| cell.to_le_bytes()).collect();
    stream.write_all(&(bytes.len() as u32).to_le_bytes()).is_ok() && stream.write_all(&bytes).is_ok()
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let count: usize = args[1].parse().expect("pane count");
    let socket = args[2].clone();
    spawn_api_thread(&format!("{socket}.api"));
    let panes: Vec<_> = (0..count).map(|_| spawn_pane()).collect();
    let (dirty_tx, dirty_rx) = mpsc::channel();
    spawn_io_thread(panes.clone(), move || {
        let _ = dirty_tx.send(());
    });
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).expect("bind");
    let stream = Arc::new(Mutex::new(None::<UnixStream>));
    let accepted = stream.clone();
    thread::spawn(move || {
        for client in listener.incoming().flatten() {
            *accepted.lock().expect("lock") = Some(client);
        }
    });
    // The mux thread.
    let mut cells = Vec::new();
    loop {
        let _ = dirty_rx.recv_timeout(Duration::from_millis(500));
        while dirty_rx.try_recv().is_ok() {}
        let mut slot = stream.lock().expect("lock");
        if let Some(client) = slot.as_mut() {
            if !send(&panes, client, &mut cells) {
                *slot = None;
            }
        }
    }
}
