//! Secret redaction for text crossing a trust boundary.

use std::borrow::Cow;

/// A detected secret's stable marker category.
///
/// ```
/// use pinfold_api::Kind;
///
/// assert_eq!(Kind::PrivateKey.as_str(), "private-key");
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    /// A PEM-encoded private-key block.
    PrivateKey,
    /// An AWS access-key identifier.
    AwsKey,
    /// A GitHub personal or OAuth token.
    GithubToken,
    /// A Slack `xox` token.
    SlackToken,
    /// An OpenAI `sk-` token.
    OpenAiToken,
    /// An Anthropic `sk-ant-` token.
    AnthropicToken,
    /// A Google `AIza` API key.
    GoogleApiKey,
    /// A JSON Web Token.
    Jwt,
    /// A value assigned to a secret-like key.
    Password,
    /// A long, high-entropy token without a known prefix.
    HighEntropy,
}

impl Kind {
    /// Returns the stable lowercase name used in redaction markers.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PrivateKey => "private-key",
            Self::AwsKey => "aws-key",
            Self::GithubToken => "github-token",
            Self::SlackToken => "slack-token",
            Self::OpenAiToken => "openai-token",
            Self::AnthropicToken => "anthropic-token",
            Self::GoogleApiKey => "google-api-key",
            Self::Jwt => "jwt",
            Self::Password => "password",
            Self::HighEntropy => "high-entropy",
        }
    }
}

/// Replaces likely secrets while borrowing input that needs no changes.
///
/// The scanner is linear in the input size and treats input as bytes until it
/// has verified ASCII match boundaries, so arbitrary UTF-8 cannot cause a
/// boundary panic.
///
/// ```
/// use std::borrow::Cow;
/// use pinfold_api::redact;
///
/// assert_eq!(redact("password=hunter2"), "password=«redacted:password»");
/// assert!(matches!(redact("build finished"), Cow::Borrowed(_)));
/// ```
pub fn redact(text: &str) -> Cow<'_, str> {
    let bytes: &[u8] = text.as_bytes();
    let mut output: Option<String> = None;
    let mut copied_until: usize = 0;
    let mut index: usize = 0;

    while index < bytes.len() {
        if let Some((start, end, kind)) = match_at(bytes, index) {
            let redacted: &mut String =
                output.get_or_insert_with(|| String::with_capacity(text.len()));
            redacted.push_str(&text[copied_until..start]);
            redacted.push_str("«redacted:");
            redacted.push_str(kind.as_str());
            redacted.push('»');
            copied_until = end;
            index = end;
        } else {
            index += 1;
        }
    }

    match output {
        Some(mut redacted) => {
            redacted.push_str(&text[copied_until..]);
            Cow::Owned(redacted)
        }
        None => Cow::Borrowed(text),
    }
}

/// Redacts the last `lines` lines of `text`, using the lines above them only as context, so a
/// read cut inside a private-key block still hides the rest of the block. Callers pass up to
/// [`KEY_CONTEXT`] lines more than they return. A private-key END line with no BEGIN before it
/// in `text` means the block began before it, so everything up to that line is redacted too.
///
/// ```
/// use pinfold_api::redact_last_lines;
///
/// let text = "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\nBBBB\n-----END OPENSSH PRIVATE KEY-----\n$ ls";
/// assert_eq!(redact_last_lines(text, 3), "«redacted:private-key»\n$ ls");
/// assert_eq!(redact_last_lines(text, 1), "$ ls");
/// ```
pub fn redact_last_lines(text: &str, lines: usize) -> Cow<'_, str> {
    let cut: usize = match lines.checked_sub(1) {
        None => text.len(),
        Some(skipped) => text
            .rmatch_indices('\n')
            .nth(skipped)
            .map_or(0, |(at, _)| at + 1),
    };
    let (above, tail) = text.split_at(cut);
    let above: &[u8] = above.as_bytes();
    // Walk the key blocks above as `redact` would, to find one still open at the cut.
    let mut index: usize = 0;
    let end_marker: Vec<u8> = loop {
        if index >= above.len() {
            return match orphan_end(tail.as_bytes()) {
                Some(end) => Cow::Owned(after_key(&tail[end..])),
                None => redact(tail),
            };
        }
        match private_key_end_marker(above, index) {
            Some(marker) => match find_bytes(above, index, &marker) {
                Some(at) => index = at + marker.len(),
                None => break marker,
            },
            None => index += 1,
        }
    };
    Cow::Owned(after_key(&tail[key_end(tail.as_bytes(), 0, &end_marker)..]))
}

/// A private-key marker followed by `rest` redacted.
fn after_key(rest: &str) -> String {
    let mut redacted: String = String::with_capacity(rest.len() + 24);
    redacted.push_str("«redacted:");
    redacted.push_str(Kind::PrivateKey.as_str());
    redacted.push('»');
    redacted.push_str(&redact(rest));
    redacted
}

/// Where a private-key END line ends when it comes before any BEGIN, as when a read lost the
/// BEGIN to its byte cap: the text from the start through it is taken as the key, failing closed.
fn orphan_end(bytes: &[u8]) -> Option<usize> {
    const END: &[u8] = b"-----END ";
    let mut index: usize = 0;
    while index < bytes.len() {
        if private_key_end_marker(bytes, index).is_some() {
            return None;
        }
        if bytes[index..].starts_with(END) {
            let label_start: usize = index + END.len();
            let label_end: usize = (label_start..bytes.len().min(label_start + 64))
                .find(|&at| matches!(bytes[at], b'-' | b'\n'))
                .unwrap_or(label_start);
            if contains_ignore_case(&bytes[label_start..label_end], b"PRIVATE KEY") {
                return Some(key_end(bytes, index, &bytes[index..label_end]));
            }
        }
        index += 1;
    }
    None
}

/// Lines above a cut read for [`redact_last_lines`]. The longest common key is RSA 8192 in
/// PKCS#8, 100 lines from BEGIN to END, so this reaches its BEGIN from any cut inside it.
pub const KEY_CONTEXT: usize = 128;

fn match_at(bytes: &[u8], index: usize) -> Option<(usize, usize, Kind)> {
    private_key_at(bytes, index)
        .map(|end| (index, end, Kind::PrivateKey))
        .or_else(|| uri_password_at(bytes, index).map(|(start, end)| (start, end, Kind::Password)))
        .or_else(|| pair_value_at(bytes, index).map(|(start, end)| (start, end, Kind::Password)))
        .or_else(|| prefixed_token_at(bytes, index))
        .or_else(|| jwt_at(bytes, index).map(|end| (index, end, Kind::Jwt)))
        .or_else(|| entropy_at(bytes, index).map(|end| (index, end, Kind::HighEntropy)))
}

/// Matches at the first byte of a URI password, so the scheme and user before it are
/// scanned like any other text and a token used as the user is still found.
fn uri_password_at(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    let colon: usize = index.checked_sub(1)?;
    if bytes[colon] != b':' {
        return None;
    }
    let user_start: usize = bytes[..colon]
        .iter()
        .rposition(|byte| ends_authority(*byte) || *byte == b':')
        .map_or(0, |position| position + 1);
    if user_start == colon || !bytes[..user_start].ends_with(b"://") {
        return None;
    }
    let scheme_end: usize = user_start - 3;
    if scheme_end == 0 || !is_uri_scheme(bytes[scheme_end - 1]) {
        return None;
    }
    let authority_end: usize = bytes[index..]
        .iter()
        .position(|byte| ends_authority(*byte))
        .map_or(bytes.len(), |offset| index + offset);
    let at: usize = bytes[index..authority_end]
        .iter()
        .rposition(|byte| *byte == b'@')
        .map(|offset| index + offset)?;
    (at > index && at + 1 < authority_end).then_some((index, at))
}

fn ends_authority(byte: u8) -> bool {
    byte.is_ascii_whitespace() || matches!(byte, b'/' | b'?' | b'#')
}

fn is_uri_scheme(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')
}

fn private_key_at(bytes: &[u8], index: usize) -> Option<usize> {
    let end_marker: Vec<u8> = private_key_end_marker(bytes, index)?;
    // Without its END line, the rest of the text is the key.
    Some(key_end(bytes, index, &end_marker))
}

/// The END line that closes a private-key BEGIN line at `index`.
fn private_key_end_marker(bytes: &[u8], index: usize) -> Option<Vec<u8>> {
    const BEGIN: &[u8] = b"-----BEGIN ";
    if !bytes.get(index..)?.starts_with(BEGIN) {
        return None;
    }
    // Any label with PRIVATE KEY in it: RSA, EC, OPENSSH, ENCRYPTED, PGP ... BLOCK and so on.
    let label_start: usize = index + BEGIN.len();
    let label_end: usize = (label_start..bytes.len().min(label_start + 64))
        .find(|&at| matches!(bytes[at], b'-' | b'\n'))?;
    let label: &[u8] = &bytes[label_start..label_end];
    if !contains_ignore_case(label, b"PRIVATE KEY") {
        return None;
    }
    let mut end_marker: Vec<u8> = b"-----END ".to_vec();
    end_marker.extend_from_slice(label);
    Some(end_marker)
}

/// Where a key block searched from `start` ends: after its END line, or at the end of the text.
fn key_end(bytes: &[u8], start: usize, end_marker: &[u8]) -> usize {
    find_bytes(bytes, start, end_marker).map_or(bytes.len(), |at| {
        let after: usize = at + end_marker.len();
        after
            + bytes[after..]
                .iter()
                .take_while(|&&byte| byte == b'-')
                .count()
    })
}

fn contains_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn find_bytes(haystack: &[u8], start: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || start > haystack.len() {
        return None;
    }
    haystack[start..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| start + offset)
}

fn pair_value_at(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    // Matched anywhere in the key, so `DB_PASSWORD=`, `AWS_SECRET_ACCESS_KEY=`, `--token=`
    // and JSON `"api_key": ` all count.
    const WORDS: &[&[u8]] = &[
        b"password",
        b"passwd",
        b"secret",
        b"token",
        b"api_key",
        b"apikey",
        b"private_key",
        b"credential",
        b"access_key",
    ];
    if index > 0 && is_key(bytes[index - 1]) {
        return None;
    }
    let mut cursor: usize = index;
    while bytes.get(cursor).is_some_and(|byte| is_key(*byte)) {
        cursor += 1;
    }
    let key: &[u8] = &bytes[index..cursor];
    if !WORDS.iter().any(|word| contains_ignore_case(key, word)) {
        return None;
    }
    if matches!(bytes.get(cursor), Some(b'\'' | b'"')) {
        cursor += 1;
    }
    while matches!(bytes.get(cursor), Some(b' ' | b'\t')) {
        cursor += 1;
    }
    if !matches!(bytes.get(cursor), Some(b'=' | b':')) {
        return None;
    }
    cursor += 1;
    while matches!(bytes.get(cursor), Some(b' ' | b'\t')) {
        cursor += 1;
    }
    let quote: Option<u8> = match bytes.get(cursor) {
        Some(b'\'' | b'"') => {
            let found: u8 = bytes[cursor];
            cursor += 1;
            Some(found)
        }
        _ => None,
    };
    let value_start: usize = cursor;
    // A key block as the value is left whole for the private-key rule, so no line of it shows.
    if bytes.get(value_start..)?.starts_with(b"-----BEGIN ") {
        return None;
    }
    while let Some(&byte) = bytes.get(cursor) {
        if quote.is_some() && byte == b'\\' {
            // An escaped quote does not end the value.
            cursor = (cursor + 2).min(bytes.len());
            continue;
        }
        let ended: bool = quote.map_or_else(
            || byte.is_ascii_whitespace() || matches!(byte, b',' | b';' | b'&'),
            |delimiter| byte == delimiter,
        );
        if ended {
            break;
        }
        cursor += 1;
    }
    let value: &[u8] = &bytes[value_start..cursor];
    // Counts such as `max_tokens: 4096` are not secrets, but a numeric password is.
    let count: bool = value.iter().all(u8::is_ascii_digit)
        && WORDS
            .iter()
            .filter(|word| contains_ignore_case(key, word))
            .all(|word| *word == b"token");
    (!value.is_empty() && !count).then_some((value_start, cursor))
}

fn is_key(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
}

fn prefixed_token_at(bytes: &[u8], index: usize) -> Option<(usize, usize, Kind)> {
    type FixedToken = (&'static [u8], usize, fn(u8) -> bool, Kind);
    const FIXED: &[FixedToken] = &[
        (b"AKIA", 16, is_upper_alnum, Kind::AwsKey),
        (b"ASIA", 16, is_upper_alnum, Kind::AwsKey),
        (b"ghp_", 36, is_alnum, Kind::GithubToken),
        (b"gho_", 36, is_alnum, Kind::GithubToken),
        (b"AIza", 35, is_token, Kind::GoogleApiKey),
    ];
    const VARIABLE: &[(&[u8], usize, Kind)] = &[
        (b"github_pat_", 30, Kind::GithubToken),
        (b"sk-ant-", 24, Kind::AnthropicToken),
        (b"sk-", 24, Kind::OpenAiToken),
    ];
    if index > 0 && is_token(bytes[index - 1]) {
        return None;
    }
    let rest: &[u8] = bytes.get(index..)?;
    for &(prefix, payload_length, valid, kind) in FIXED {
        if rest.starts_with(prefix) {
            return fixed_token(bytes, index, prefix.len(), payload_length, valid)
                .map(|end| (index, end, kind));
        }
    }
    if rest.starts_with(b"xox")
        && rest.get(3).is_some_and(u8::is_ascii_alphabetic)
        && rest.get(4) == Some(&b'-')
    {
        return variable_token(bytes, index, 20, is_token)
            .map(|end| (index, end, Kind::SlackToken));
    }
    for &(prefix, minimum, kind) in VARIABLE {
        if rest.starts_with(prefix) {
            return variable_token(bytes, index, minimum, is_token).map(|end| (index, end, kind));
        }
    }
    None
}

fn fixed_token(
    bytes: &[u8],
    start: usize,
    prefix_length: usize,
    payload_length: usize,
    valid: fn(u8) -> bool,
) -> Option<usize> {
    let payload_start: usize = start.checked_add(prefix_length)?;
    let end: usize = payload_start.checked_add(payload_length)?;
    let payload: &[u8] = bytes.get(payload_start..end)?;
    if payload.iter().copied().all(valid) && bytes.get(end).is_none_or(|byte| !is_token(*byte)) {
        Some(end)
    } else {
        None
    }
}

fn variable_token(
    bytes: &[u8],
    start: usize,
    minimum: usize,
    valid: fn(u8) -> bool,
) -> Option<usize> {
    let mut end: usize = start;
    while bytes.get(end).is_some_and(|byte| valid(*byte)) {
        end += 1;
    }
    (end - start >= minimum).then_some(end)
}

fn jwt_at(bytes: &[u8], index: usize) -> Option<usize> {
    if !bytes.get(index..)?.starts_with(b"eyJ") || (index > 0 && is_base64_url(bytes[index - 1])) {
        return None;
    }
    let first_end: usize = segment_end(bytes, index);
    let second_start: usize = first_end.checked_add(1)?;
    let second_end: usize = segment_end(bytes, second_start);
    let third_start: usize = second_end.checked_add(1)?;
    let third_end: usize = segment_end(bytes, third_start);
    if bytes.get(first_end) == Some(&b'.')
        && bytes.get(second_end) == Some(&b'.')
        && first_end - index >= 8
        && second_end - second_start >= 8
        && third_end - third_start >= 8
        && bytes
            .get(third_end)
            .is_none_or(|byte| !is_base64_url(*byte))
    {
        Some(third_end)
    } else {
        None
    }
}

fn segment_end(bytes: &[u8], start: usize) -> usize {
    let mut end: usize = start;
    while bytes.get(end).is_some_and(|byte| is_base64_url(*byte)) {
        end += 1;
    }
    end
}

fn entropy_at(bytes: &[u8], index: usize) -> Option<usize> {
    if !bytes
        .get(index)
        .is_some_and(|byte| is_secret_alphabet(*byte))
        || (index > 0 && is_secret_alphabet(bytes[index - 1]))
    {
        return None;
    }
    let mut end: usize = index;
    while bytes.get(end).is_some_and(|byte| is_secret_alphabet(*byte)) {
        end += 1;
    }
    let candidate: &[u8] = &bytes[index..end];
    // A path is not a secret, but base64 can hold `/`, so a path-like run is judged by its
    // segments: one long random segment is enough.
    let random: bool = if looks_like_path(candidate) {
        candidate
            .split(|&byte| byte == b'/')
            .any(|segment| segment.len() >= PATH_SEGMENT && is_random(segment, 12))
    } else {
        candidate.len() >= 32 && is_random(candidate, 6)
    };
    if !random {
        return None;
    }

    // `=` is base64 padding only where the token ends, so `NAME=value` stays two words.
    let mut padded: usize = end;
    while padded - end < 2 && bytes.get(padded) == Some(&b'=') {
        padded += 1;
    }
    if bytes
        .get(padded)
        .is_none_or(|byte| !is_secret_alphabet(*byte))
    {
        end = padded;
    }
    Some(end)
}

/// The shortest path segment judged on its own as a possible secret.
const PATH_SEGMENT: usize = 24;

/// Pure hex is not random here: in a terminal it is almost always a digest or commit
/// (Cargo.lock checksums alone were 3% of lines), and hex secrets after `token=` and the like
/// are caught by the pair rule. Random base64 of 24+ chars has a digit 99.8% of the time, while
/// long `UPPER_SNAKE` names have none. Base64url has about one `-` or `_` per 32 chars, so a run
/// with one per `chars_per_separator` or more is words. Path segments use a stricter 12, since
/// file names such as `0032-api-thread-and-m3-order` are where such words show up. 3.75 bits/char rejects repetitive identifiers.
fn is_random(run: &[u8], chars_per_separator: usize) -> bool {
    let separators: usize = run
        .iter()
        .filter(|&&byte| matches!(byte, b'-' | b'_'))
        .count();
    separators * chars_per_separator <= run.len()
        && !is_hex(run)
        && !is_uuid(run)
        && run.iter().any(u8::is_ascii_digit)
        && run.iter().any(u8::is_ascii_alphabetic)
        && shannon_entropy(run) > 3.75
}

fn shannon_entropy(bytes: &[u8]) -> f64 {
    let mut counts: [usize; 128] = [0; 128];
    for &byte in bytes {
        counts[usize::from(byte)] += 1;
    }
    let length: f64 = bytes.len() as f64;
    counts
        .iter()
        .filter(|&&count| count > 0)
        .fold(0.0, |entropy, &count| {
            let probability: f64 = count as f64 / length;
            entropy - probability * probability.log2()
        })
}

fn is_hex(bytes: &[u8]) -> bool {
    bytes.iter().all(u8::is_ascii_hexdigit)
}

fn is_uuid(bytes: &[u8]) -> bool {
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn looks_like_path(bytes: &[u8]) -> bool {
    bytes.first() == Some(&b'/') || bytes.iter().filter(|&&byte| byte == b'/').take(2).count() == 2
}

fn is_alnum(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
}

fn is_upper_alnum(byte: u8) -> bool {
    byte.is_ascii_uppercase() || byte.is_ascii_digit()
}

fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn is_base64_url(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'=')
}

fn is_secret_alphabet(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+' | b'/')
}
