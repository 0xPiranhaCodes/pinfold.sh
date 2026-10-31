//! Shared parts of the font spike. See README.md.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::{ptr, slice};

/// Maps a font read-only, so that only the pages a rasterizer touches count as memory.
pub fn map(path: &str) -> &'static [u8] {
    let file = File::open(path).expect("open font");
    let length = file.metadata().expect("metadata").len() as usize;
    unsafe {
        let data = libc::mmap(ptr::null_mut(), length, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0);
        assert_ne!(data, libc::MAP_FAILED, "mmap");
        slice::from_raw_parts(data.cast(), length)
    }
}

#[cfg(target_os = "linux")]
pub fn footprint() -> f64 {
    let text = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
    let line = text.lines().find(|line| line.starts_with("Pss:")).unwrap_or("Pss: 0 kB");
    line.split_whitespace().nth(1).and_then(|kb| kb.parse::<f64>().ok()).unwrap_or(0.0) * 1024.0 / 1e6
}

#[cfg(target_os = "macos")]
pub fn footprint() -> f64 {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let pid = std::process::id() as i32;
    unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, (&raw mut info).cast()) };
    info.ri_phys_footprint as f64 / 1e6
}

/// Printable ASCII, then box drawing and Latin letters: about one screen of code.
pub fn glyphs() -> Vec<char> {
    let mut all: Vec<char> = (' '..='~').collect();
    all.extend('\u{2500}'..='\u{257f}');
    all.extend('\u{00c0}'..='\u{017f}');
    all
}

/// A test image of text as a grey PGM, one cell per character.
pub struct Sheet {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

impl Sheet {
    pub fn new(width: usize, height: usize) -> Self {
        Self { width, height, pixels: vec![0; width * height] }
    }

    pub fn blit(&mut self, left: i32, top: i32, width: usize, mask: &[u8]) {
        for (row, line) in mask.chunks(width.max(1)).enumerate() {
            for (column, &alpha) in line.iter().enumerate() {
                let (x, y) = (left + column as i32, top + row as i32);
                if x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height {
                    let at = y as usize * self.width + x as usize;
                    self.pixels[at] = self.pixels[at].max(alpha);
                }
            }
        }
    }

    pub fn save(&self, path: &str) {
        let mut data = format!("P5\n{} {}\n255\n", self.width, self.height).into_bytes();
        data.extend_from_slice(&self.pixels);
        std::fs::write(path, data).expect("write image");
    }
}

pub const SAMPLE: &str = "fn main() { let g = 0xBEEF; } il1I|O0 {}[]() => != --> ~@#$%";
