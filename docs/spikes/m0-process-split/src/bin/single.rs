//! The baseline: PTYs, VT state and the window in one process, with the same threads.

use std::env;

use spike_process_split::{draw, snapshot, spawn_api_thread, spawn_io_thread, spawn_pane};

fn main() {
    let args: Vec<String> = env::args().collect();
    let count: usize = args[1].parse().expect("pane count");
    spawn_api_thread(&format!("{}.api", args[2]));
    let panes: Vec<_> = (0..count).map(|_| spawn_pane()).collect();
    draw::run(count, move |proxy| {
        spawn_io_thread(panes.clone(), move || {
            let _ = proxy.send_event(());
        });
        Box::new(move |out| panes.iter().for_each(|pane| snapshot(pane, out)))
    });
}
