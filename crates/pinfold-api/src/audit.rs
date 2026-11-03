//! Tamper-evident records of API calls.
//!
//! The canonical hash input is the previous hash followed by the record fields in declaration
//! order. Integers are big-endian; optional integers have a one-byte presence tag; strings have
//! an eight-byte big-endian byte length followed by UTF-8 bytes; and outcomes have a one-byte
//! variant tag followed by their reason as a string. Thus no two field sequences are ambiguous.
//!
//! ```
//! use pinfold_api::{Log, Outcome, Record};
//!
//! let mut log = Log::new();
//! log.append(Record {
//!     seq: 0, at_ms: 42, caller: Some(1), method: "pane.read".into(),
//!     target: Some(2), scope: "pane.read:other".into(), outcome: Outcome::Ok("allowed".into()),
//! })?;
//! assert_eq!(log.for_pane(2).count(), 1);
//! log.verify()?;
//! # Ok::<(), pinfold_api::audit::Error>(())
//! ```

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const RING_BYTES: usize = 256 * 1024;
const FILE_BYTES: u64 = 5 * 1024 * 1024;
const HASH_BYTES: usize = 32;

/// The result of an audited call, with a short explanatory reason.
///
/// ```
/// use pinfold_api::Outcome;
/// let outcome = Outcome::Refused("protected pane".into());
/// assert!(matches!(outcome, Outcome::Refused(_)));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The call succeeded.
    Ok(String),
    /// Policy refused the call.
    Refused(String),
    /// The call failed while being performed.
    Error(String),
}

/// The fields recorded for one API call.
///
/// [`Log::append`] assigns `seq`, so whatever the caller puts there is replaced.
///
/// ```
/// use pinfold_api::{Outcome, Record};
/// let record = Record { seq: 7, at_ms: 100, caller: None, method: "hello".into(),
///     target: None, scope: "none".into(), outcome: Outcome::Ok("version matched".into()) };
/// assert_eq!(record.seq, 7);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Monotonically increasing call sequence.
    pub seq: u64,
    /// Unix time in milliseconds.
    pub at_ms: u64,
    /// Calling pane, or `None` when the caller is not identified as a pane.
    pub caller: Option<u64>,
    /// API method name.
    pub method: String,
    /// Target pane, when the call has one.
    pub target: Option<u64>,
    /// Scope used for the authorization decision.
    pub scope: String,
    /// Result and short reason.
    pub outcome: Outcome,
}

/// A record and its links in the hash chain.
///
/// ```
/// # use pinfold_api::{Log, Outcome, Record};
/// # let mut log = Log::new();
/// # log.append(Record { seq: 0, at_ms: 0, caller: None, method: "hello".into(),
/// # target: None, scope: "none".into(), outcome: Outcome::Ok("ok".into()) })?;
/// let entry = log.iter().next().ok_or("missing entry")?;
/// assert_ne!(entry.hash, [0; 32]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The audited call.
    pub record: Record,
    /// Hash of the preceding entry, or zeroes for the first entry.
    pub prev: [u8; HASH_BYTES],
    /// SHA-256 of `prev` and the canonical record bytes.
    pub hash: [u8; HASH_BYTES],
    encoded_len: usize,
}

/// Location of a broken link or invalid record hash.
///
/// ```
/// let broken = pinfold_api::Broken { seq: 3 };
/// assert_eq!(broken.seq, 3);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("audit chain is broken at sequence {seq}")]
pub struct Broken {
    /// Sequence whose hash or preceding link is invalid.
    pub seq: u64,
}

/// An audit-log or persisted-log error.
///
/// ```
/// let error = pinfold_api::audit::Error::Malformed { line: 2 };
/// assert!(error.to_string().contains("line 2"));
/// ```
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The system refused a file operation.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// One record alone cannot fit in the bounded ring.
    #[error("audit entry needs {bytes} bytes, over the 256 KiB limit")]
    EntryTooLarge {
        /// Canonical record bytes plus its two hashes.
        bytes: usize,
    },
    /// A persisted line is not in the audit format.
    #[error("malformed audit log line {line}")]
    Malformed {
        /// One-based line number.
        line: u64,
    },
    /// The chain is inconsistent.
    #[error(transparent)]
    Broken(#[from] Broken),
}

struct Disk {
    path: PathBuf,
    file: File,
    bytes: u64,
}

/// A 256 KiB in-memory audit ring, optionally mirrored to a rotating private file.
///
/// ```
/// let log = pinfold_api::Log::new();
/// assert_eq!(log.iter().count(), 0);
/// ```
pub struct Log {
    entries: VecDeque<Entry>,
    bytes: usize,
    tail: [u8; HASH_BYTES],
    next_seq: u64,
    disk: Option<Disk>,
}

impl Log {
    /// Creates an empty in-memory log.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            tail: [0; HASH_BYTES],
            next_seq: 0,
            disk: None,
        }
    }

    /// Creates a log mirrored to `path`, creating it with mode 0600 when absent. An
    /// existing file is verified first and the chain continues from its last entry, so a
    /// restarted core does not break it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the file cannot be opened, and [`Error::Malformed`] or
    /// [`Error::Broken`] when the existing file fails verification.
    pub fn with_file(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref().to_path_buf();
        let file = open_file(&path)?;
        let bytes = file.metadata()?.len();
        let mut log = Self::new();
        let state = chain_state(&path)?;
        if let (Some(hash), Some(seq)) = (state.hash, state.seq) {
            log.tail = hash;
            log.next_seq = seq.saturating_add(1);
        }
        log.disk = Some(Disk { path, file, bytes });
        Ok(log)
    }

    /// Appends a record and then mirrors it to disk when configured.
    ///
    /// A disk error is returned after the authoritative in-memory entry has been retained.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EntryTooLarge`] without appending, or [`Error::Io`] after appending.
    pub fn append(&mut self, mut record: Record) -> Result<(), Error> {
        record.seq = self.next_seq;
        let canonical = encode_record(&record);
        let encoded_len = 2 * HASH_BYTES + canonical.len();
        if encoded_len > RING_BYTES {
            return Err(Error::EntryTooLarge { bytes: encoded_len });
        }
        let prev = self.tail;
        let hash = digest(&prev, &canonical);
        let entry = Entry {
            record,
            prev,
            hash,
            encoded_len,
        };
        self.tail = hash;
        self.next_seq = self.next_seq.saturating_add(1);
        self.bytes += encoded_len;
        self.entries.push_back(entry);
        while self.bytes > RING_BYTES {
            if let Some(oldest) = self.entries.pop_front() {
                self.bytes -= oldest.encoded_len;
            }
        }
        if let Some(disk) = &mut self.disk {
            write_entry(
                disk,
                self.entries.back().ok_or(Error::Malformed { line: 0 })?,
            )?;
        }
        Ok(())
    }

    /// Iterates over retained entries from oldest to newest.
    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    /// Iterates over retained calls made by or targeting `pane`.
    pub fn for_pane(&self, pane: u64) -> impl Iterator<Item = &Entry> {
        self.iter().filter(move |entry: &&Entry| {
            entry.record.caller == Some(pane) || entry.record.target == Some(pane)
        })
    }

    /// Recomputes every retained hash and link.
    ///
    /// # Errors
    ///
    /// Returns [`Broken`] at the first inconsistent retained entry.
    pub fn verify(&self) -> Result<(), Broken> {
        let mut prior: Option<[u8; HASH_BYTES]> = None;
        let mut prior_seq: Option<u64> = None;
        for entry in &self.entries {
            if prior.is_some_and(|hash| hash != entry.prev)
                || prior_seq.is_some_and(|seq| seq.checked_add(1) != Some(entry.record.seq))
                || digest(&entry.prev, &encode_record(&entry.record)) != entry.hash
            {
                return Err(Broken {
                    seq: entry.record.seq,
                });
            }
            prior = Some(entry.hash);
            prior_seq = Some(entry.record.seq);
        }
        Ok(())
    }
}

impl Default for Log {
    fn default() -> Self {
        Self::new()
    }
}

/// Verifies a persisted audit chain and returns its entry count.
///
/// If `<path>.1` exists, it is verified first as the older part of the same chain.
///
/// # Errors
///
/// Returns [`Error::Malformed`], [`Error::Broken`], or [`Error::Io`].
pub fn verify_file(path: impl AsRef<Path>) -> Result<u64, Error> {
    Ok(chain_state(path.as_ref())?.count)
}

fn chain_state(path: &Path) -> Result<VerifyState, Error> {
    let old = rotated_path(path);
    let mut state = VerifyState::default();
    if old.exists() {
        verify_one(&old, &mut state)?;
    }
    verify_one(path, &mut state)?;
    Ok(state)
}

#[derive(Default)]
struct VerifyState {
    hash: Option<[u8; HASH_BYTES]>,
    seq: Option<u64>,
    count: u64,
}

fn verify_one(path: &Path, state: &mut VerifyState) -> Result<(), Error> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = String::new();
    let mut line_no = 0_u64;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        line_no = line_no.saturating_add(1);
        if !line.ends_with('\n') {
            return Err(Error::Malformed { line: line_no });
        }
        line.pop();
        let entry = parse_line(&line, line_no)?;
        if state.hash.is_some_and(|hash| hash != entry.prev)
            || state
                .seq
                .is_some_and(|seq| seq.checked_add(1) != Some(entry.record.seq))
            || digest(&entry.prev, &encode_record(&entry.record)) != entry.hash
        {
            return Err(Broken {
                seq: entry.record.seq,
            }
            .into());
        }
        state.hash = Some(entry.hash);
        state.seq = Some(entry.record.seq);
        state.count = state.count.saturating_add(1);
    }
    Ok(())
}

fn encode_record(record: &Record) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&record.seq.to_be_bytes());
    bytes.extend_from_slice(&record.at_ms.to_be_bytes());
    encode_option(&mut bytes, record.caller);
    encode_string(&mut bytes, &record.method);
    encode_option(&mut bytes, record.target);
    encode_string(&mut bytes, &record.scope);
    let (tag, reason) = match &record.outcome {
        Outcome::Ok(reason) => (0_u8, reason),
        Outcome::Refused(reason) => (1_u8, reason),
        Outcome::Error(reason) => (2_u8, reason),
    };
    bytes.push(tag);
    encode_string(&mut bytes, reason);
    bytes
}

fn encode_option(bytes: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(value) => {
            bytes.push(1);
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        None => bytes.push(0),
    }
}

fn encode_string(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
}

fn digest(prev: &[u8; HASH_BYTES], canonical: &[u8]) -> [u8; HASH_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(prev);
    hasher.update(canonical);
    hasher.finalize().into()
}

fn open_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

fn write_entry(disk: &mut Disk, entry: &Entry) -> Result<(), Error> {
    let line = format_line(entry);
    let line_bytes = line.len() as u64;
    if disk.bytes > 0 && disk.bytes.saturating_add(line_bytes) > FILE_BYTES {
        let old = rotated_path(&disk.path);
        match fs::remove_file(&old) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        fs::rename(&disk.path, old)?;
        disk.file = open_file(&disk.path)?;
        disk.bytes = 0;
    }
    disk.file.write_all(line.as_bytes())?;
    disk.file.flush()?;
    disk.bytes = disk.bytes.saturating_add(line_bytes);
    Ok(())
}

fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

fn format_line(entry: &Entry) -> String {
    let (outcome, reason) = match &entry.record.outcome {
        Outcome::Ok(reason) => ("ok", reason),
        Outcome::Refused(reason) => ("refused", reason),
        Outcome::Error(reason) => ("error", reason),
    };
    json!({
        "hash": hex(&entry.hash), "prev": hex(&entry.prev),
        "seq": entry.record.seq, "at_ms": entry.record.at_ms,
        "caller": entry.record.caller, "method": entry.record.method,
        "target": entry.record.target, "scope": entry.record.scope,
        "outcome": outcome, "reason": reason,
    })
    .to_string()
        + "\n"
}

fn parse_line(line: &str, line_no: u64) -> Result<Entry, Error> {
    let value: Value =
        serde_json::from_str(line).map_err(|_| Error::Malformed { line: line_no })?;
    let field = |name: &str| value.get(name).ok_or(Error::Malformed { line: line_no });
    let text = |name: &str| {
        field(name)?
            .as_str()
            .map(str::to_owned)
            .ok_or(Error::Malformed { line: line_no })
    };
    let number = |name: &str| {
        field(name)?
            .as_u64()
            .ok_or(Error::Malformed { line: line_no })
    };
    let optional = |name: &str| match field(name)? {
        Value::Null => Ok(None),
        value => value
            .as_u64()
            .map(Some)
            .ok_or(Error::Malformed { line: line_no }),
    };
    let hash = parse_hash(&text("hash")?, line_no)?;
    let prev = parse_hash(&text("prev")?, line_no)?;
    let reason = text("reason")?;
    let outcome = match text("outcome")?.as_str() {
        "ok" => Outcome::Ok(reason),
        "refused" => Outcome::Refused(reason),
        "error" => Outcome::Error(reason),
        _ => return Err(Error::Malformed { line: line_no }),
    };
    let record = Record {
        seq: number("seq")?,
        at_ms: number("at_ms")?,
        caller: optional("caller")?,
        method: text("method")?,
        target: optional("target")?,
        scope: text("scope")?,
        outcome,
    };
    let encoded_len = 2 * HASH_BYTES + encode_record(&record).len();
    Ok(Entry {
        record,
        prev,
        hash,
        encoded_len,
    })
}

fn parse_hash(value: &str, line: u64) -> Result<[u8; HASH_BYTES], Error> {
    if value.len() != 2 * HASH_BYTES {
        return Err(Error::Malformed { line });
    }
    let mut hash = [0_u8; HASH_BYTES];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        hash[index] = (digit(pair[0]).ok_or(Error::Malformed { line })? << 4)
            | digit(pair[1]).ok_or(Error::Malformed { line })?;
    }
    Ok(hash)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

fn digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn record(seq: u64) -> Record {
        Record {
            seq,
            at_ms: 1_700_000_000_000 + seq,
            caller: Some(seq % 3),
            method: "pane.send".into(),
            target: Some((seq + 1) % 3),
            scope: "children".into(),
            outcome: Outcome::Ok("sent 12 bytes".into()),
        }
    }

    fn path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pinfold-audit-{}-{}-{name}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn chain_and_ring_verify_after_eviction() -> Result<(), Error> {
        let mut log = Log::new();
        for seq in 0..10_000 {
            log.append(record(seq))?;
        }
        assert!(log.bytes <= RING_BYTES);
        assert!(log.iter().next().is_some_and(|entry| entry.record.seq > 0));
        log.verify()?;
        Ok(())
    }

    #[test]
    fn a_restarted_log_continues_the_file_chain() -> Result<(), Error> {
        let path = path("restart");
        let mut first = Log::with_file(&path)?;
        for seq in 0..3 {
            first.append(record(seq))?;
        }
        drop(first);
        let mut second = Log::with_file(&path)?;
        second.append(record(0))?;
        assert_eq!(second.iter().next().map(|entry| entry.record.seq), Some(3));
        assert_eq!(verify_file(&path)?, 4);
        fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn pane_filter_matches_caller_or_target() -> Result<(), Error> {
        let mut log = Log::new();
        for seq in 0..4 {
            log.append(record(seq))?;
        }
        let sequences: Vec<u64> = log.for_pane(0).map(|entry| entry.record.seq).collect();
        assert_eq!(sequences, [0, 2, 3]);
        Ok(())
    }

    #[test]
    fn file_is_private_and_newline_is_data() -> Result<(), Error> {
        let path = path("private");
        let mut log = Log::with_file(&path)?;
        let mut item = record(0);
        item.outcome = Outcome::Error("first\nforged\tline".into());
        log.append(item)?;
        drop(log);
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        assert_eq!(BufReader::new(File::open(&path)?).lines().count(), 1);
        assert_eq!(verify_file(&path)?, 1);
        fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn write_error_keeps_the_memory_record() -> Result<(), Error> {
        let path = path("read-only");
        fs::write(&path, [])?;
        let file = File::open(&path)?;
        let mut log = Log::new();
        log.disk = Some(Disk {
            path: path.clone(),
            file,
            bytes: 0,
        });
        assert!(matches!(log.append(record(0)), Err(Error::Io(_))));
        assert_eq!(log.iter().count(), 1);
        log.verify()?;
        fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn rotates_at_five_mebibytes() -> Result<(), Error> {
        let path = path("rotate");
        let mut log = Log::with_file(&path)?;
        let mut large = record(0);
        large.outcome = Outcome::Ok("x".repeat(FILE_BYTES as usize - 300));
        assert!(matches!(
            log.append(large),
            Err(Error::EntryTooLarge { .. })
        ));
        let mut written = 0;
        for seq in 0..40_000 {
            log.append(record(seq))?;
            written += 1;
            if rotated_path(&path).exists() {
                break;
            }
        }
        assert!(rotated_path(&path).exists());
        drop(log);
        assert_eq!(verify_file(&path)?, written);
        fs::remove_file(rotated_path(&path))?;
        fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn detects_changed_dropped_swapped_and_forged_lines() -> Result<(), Error> {
        for kind in 0..4 {
            let path = path("tamper");
            let mut log = Log::with_file(&path)?;
            for seq in 0..4 {
                log.append(record(seq))?;
            }
            drop(log);
            let original = fs::read_to_string(&path)?;
            let mut lines: Vec<&str> = original.lines().collect();
            let tampered = match kind {
                0 => {
                    let replacement = if original.as_bytes()[0] == b'0' {
                        "1"
                    } else {
                        "0"
                    };
                    original.replacen(&original[..1], replacement, 1)
                }
                1 => [lines[0], lines[2], lines[3]].join("\n") + "\n",
                2 => {
                    lines.swap(1, 2);
                    lines.join("\n") + "\n"
                }
                _ => format!("{original}{}\n", lines[0]),
            };
            fs::write(&path, tampered)?;
            assert!(verify_file(&path).is_err());
            fs::remove_file(path)?;
        }
        Ok(())
    }

    #[test]
    fn reports_typical_capacity() -> Result<(), Error> {
        let typical_bytes = 2 * HASH_BYTES + encode_record(&record(0)).len();
        let mut log = Log::new();
        let mut seq = 0;
        while log.bytes + typical_bytes <= RING_BYTES {
            log.append(record(seq))?;
            seq += 1;
        }
        eprintln!("typical_encoded_bytes={typical_bytes} ring_entries={seq}");
        assert_eq!(log.bytes, typical_bytes * seq as usize);
        Ok(())
    }
}
