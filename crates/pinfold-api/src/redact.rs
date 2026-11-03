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

fn match_at(bytes: &[u8], index: usize) -> Option<(usize, usize, Kind)> {
    private_key_at(bytes, index)
        .map(|end| (index, end, Kind::PrivateKey))
        .or_else(|| pair_value_at(bytes, index).map(|(start, end)| (start, end, Kind::Password)))
        .or_else(|| prefixed_token_at(bytes, index))
        .or_else(|| jwt_at(bytes, index).map(|end| (index, end, Kind::Jwt)))
        .or_else(|| entropy_at(bytes, index).map(|end| (index, end, Kind::HighEntropy)))
}

fn private_key_at(bytes: &[u8], index: usize) -> Option<usize> {
    const BLOCKS: &[(&[u8], &[u8])] = &[
        (b"-----BEGIN PRIVATE KEY-----", b"-----END PRIVATE KEY-----"),
        (
            b"-----BEGIN RSA PRIVATE KEY-----",
            b"-----END RSA PRIVATE KEY-----",
        ),
        (
            b"-----BEGIN EC PRIVATE KEY-----",
            b"-----END EC PRIVATE KEY-----",
        ),
        (
            b"-----BEGIN DSA PRIVATE KEY-----",
            b"-----END DSA PRIVATE KEY-----",
        ),
        (
            b"-----BEGIN OPENSSH PRIVATE KEY-----",
            b"-----END OPENSSH PRIVATE KEY-----",
        ),
        (
            b"-----BEGIN ENCRYPTED PRIVATE KEY-----",
            b"-----END ENCRYPTED PRIVATE KEY-----",
        ),
    ];
    for &(begin, end) in BLOCKS {
        if bytes.get(index..)?.starts_with(begin) {
            let body_start: usize = index + begin.len();
            let block_end: usize =
                find_bytes(bytes, body_start, end).map_or(bytes.len(), |found| found + end.len());
            return Some(block_end);
        }
    }
    None
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
    const KEYS: &[&[u8]] = &[b"password", b"passwd", b"secret", b"token", b"api_key"];
    if index > 0 && is_word(bytes[index - 1]) {
        return None;
    }
    let rest: &[u8] = bytes.get(index..)?;
    let key: &[u8] = KEYS
        .iter()
        .copied()
        .find(|key| starts_ascii_case_insensitive(rest, key))?;
    let mut cursor: usize = index + key.len();
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
    while let Some(&byte) = bytes.get(cursor) {
        let ended: bool = quote.map_or_else(
            || byte.is_ascii_whitespace() || matches!(byte, b',' | b';' | b'&'),
            |delimiter| byte == delimiter,
        );
        if ended {
            break;
        }
        cursor += 1;
    }
    (cursor > value_start).then_some((value_start, cursor))
}

fn starts_ascii_case_insensitive(input: &[u8], prefix: &[u8]) -> bool {
    input
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
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
    // Pure hex is skipped: in a terminal it is almost always a digest or commit (Cargo.lock
    // checksums alone were 3% of lines), and hex secrets after `token=` and the like are
    // caught by the pair rule. Random base64 of 32+ chars has a digit 99.9% of the time,
    // while long `UPPER_SNAKE` names have none.
    if candidate.len() < 32
        || is_hex(candidate)
        || is_uuid(candidate)
        || looks_like_path(candidate)
        || !candidate.iter().any(u8::is_ascii_digit)
        || !candidate.iter().any(u8::is_ascii_alphabetic)
    {
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
    // 3.75 bits/char rejects repetitive identifiers and prose.
    (shannon_entropy(candidate) > 3.75).then_some(end)
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

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
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
