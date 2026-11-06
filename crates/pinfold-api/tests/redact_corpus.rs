//! Corpus and robustness tests for secret redaction.

use std::borrow::Cow;

use pinfold_api::{Kind, redact, redact_last_lines};

const PRIVATE_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nZmFrZS1rZXktbWF0ZXJpYWw=\n-----END OPENSSH PRIVATE KEY-----";
const AWS_KEY: &str = concat!("AK", "IAIOSFODNN7EXAMPLE");
const GITHUB_TOKEN: &str = concat!("gh", "p_0123456789abcdefghijklmnopqrstuvwxyz");
const SLACK_TOKEN: &str = concat!("xo", "xb-123456789012-123456789012-abcdefghijklmnop");
const OPENAI_TOKEN: &str = concat!("sk-p", "roj-abcdefghijklmnopqrstuvwxyz0123456789AB");
const ANTHROPIC_TOKEN: &str = concat!("sk-", "ant-api03-abcdefghijklmnopqrstuvwxyz0123456789AB");
const GOOGLE_KEY: &str = concat!("AI", "zaSyA1234567890abcdefghijklmnopqrstuv");
const JWT: &str = concat!(
    "e",
    "yJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.c2lnbmF0dXJlX2J5dGVzXzEyMzQ1Njc4OTA"
);
const HIGH_ENTROPY: &str = "A1b2C3d4E5f6G7h8I9j0K/L+m_N-oPqRsTuVwXyZ";
const POSITIVES: &[(Kind, &str, &str)] = &[
    (Kind::PrivateKey, PRIVATE_KEY, "«redacted:private-key»"),
    (
        Kind::PrivateKey,
        "-----BEGIN ENCRYPTED PRIVATE KEY-----\nZmFrZQ==\n-----END ENCRYPTED PRIVATE KEY-----",
        "«redacted:private-key»",
    ),
    (Kind::AwsKey, AWS_KEY, "«redacted:aws-key»"),
    (
        Kind::AwsKey,
        concat!("AS", "IAIOSFODNN7EXAMPLE"),
        "«redacted:aws-key»",
    ),
    (Kind::GithubToken, GITHUB_TOKEN, "«redacted:github-token»"),
    (
        Kind::GithubToken,
        concat!("gh", "o_0123456789abcdefghijklmnopqrstuvwxyz"),
        "«redacted:github-token»",
    ),
    (
        Kind::GithubToken,
        concat!(
            "githu",
            "b_pat_11AA22bb33C_abcdefghijklmnopqrstuvwxyz0123456789"
        ),
        "«redacted:github-token»",
    ),
    (Kind::SlackToken, SLACK_TOKEN, "«redacted:slack-token»"),
    (
        Kind::SlackToken,
        concat!("xo", "xz-123456789012-123456789012-abcdefghijklmnop"),
        "«redacted:slack-token»",
    ),
    (Kind::OpenAiToken, OPENAI_TOKEN, "«redacted:openai-token»"),
    (
        Kind::AnthropicToken,
        ANTHROPIC_TOKEN,
        "«redacted:anthropic-token»",
    ),
    (Kind::GoogleApiKey, GOOGLE_KEY, "«redacted:google-api-key»"),
    (Kind::Jwt, JWT, "«redacted:jwt»"),
    (
        Kind::Password,
        "password=hunter2",
        "password=«redacted:password»",
    ),
    (Kind::HighEntropy, HIGH_ENTROPY, "«redacted:high-entropy»"),
    (
        Kind::Password,
        "PIN_PASSWORD=12345678",
        "PIN_PASSWORD=«redacted:password»",
    ),
    (
        Kind::PrivateKey,
        "key: -----BEGIN RSA PRIVATE KEY-----\nMIIEow IBAAKCAQEA\n-----END RSA PRIVATE KEY-----",
        "key: «redacted:private-key»",
    ),
    (
        Kind::HighEntropy,
        "aB3_dE5-fG7_hJ9kLmN2pQ4rS6tUvWxYz0",
        "«redacted:high-entropy»",
    ),
    (
        Kind::Password,
        "DB_PASSWORD=x",
        "DB_PASSWORD=«redacted:password»",
    ),
    (
        Kind::Password,
        "export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        "export AWS_SECRET_ACCESS_KEY=«redacted:password»",
    ),
    (
        Kind::Password,
        "{\"api_key\": \"abc123\"}",
        "{\"api_key\": \"«redacted:password»\"}",
    ),
    (
        Kind::Password,
        "token=\"a\\\"b c\" next",
        "token=\"«redacted:password»\" next",
    ),
    (
        Kind::HighEntropy,
        "Zm9vYmFyYmF6cXV4MTIzNDU2/aGVsbG8gd29ybGQ0MjQyNTI2Nzg/c2Vj",
        "«redacted:high-entropy»",
    ),
];

#[test]
fn corpus_reports_detection_rates() {
    for kind in [
        Kind::PrivateKey,
        Kind::AwsKey,
        Kind::GithubToken,
        Kind::SlackToken,
        Kind::OpenAiToken,
        Kind::AnthropicToken,
        Kind::GoogleApiKey,
        Kind::Jwt,
        Kind::Password,
        Kind::HighEntropy,
    ] {
        let total: usize = POSITIVES
            .iter()
            .filter(|(found, _, _)| *found == kind)
            .count();
        let caught: usize = POSITIVES
            .iter()
            .filter(|(found, input, marker)| *found == kind && redact(input).contains(marker))
            .count();
        println!(
            "true-positive {}: {caught}/{total} ({:.1}%)",
            kind.as_str(),
            caught as f64 * 100.0 / total as f64
        );
        assert_eq!(caught, total, "missed {}", kind.as_str());
    }

    let negatives: &[&str] = &[
        "Compiling pinfold-api v0.0.0 (/work/pinfold/crates/pinfold-api)",
        "Finished `test` profile [unoptimized + debuginfo] target(s) in 1.42s",
        "error[E0308]: mismatched types\n  --> src/main.rs:12:9",
        "git status --short\n M crates/pinfold-api/src/lib.rs",
        "ea070fded3ca39c11e3f48e51e35ccfc1e482e83 refs/heads/main",
        "99a472502086d9588ce4e91c7394e6ca091c73c2",
        "550e8400-e29b-41d4-a716-446655440000",
        "drwxr-xr-x  12 user staff 384 Nov  3 10:15 crates",
        "/Users/example/Projects/pinfold/target/debug/pinfold-core",
        "https://github.com/0xPiranhaCodes/pinfold.sh/issues/102",
        "aGVsbG8= d29ybGQ= Zm9vYmFy",
        "token bucket allows 20 calls per second",
        "checksum = \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\"",
        "CLAUDE_CODE_ENABLE_ASK_USER_QUESTION_TOOL=true",
        "usage: input_tokens: 1234, output_tokens: 567",
        "-----BEGIN CERTIFICATE-----",
    ];
    let false_positives: usize = negatives
        .iter()
        .filter(|sample| !matches!(redact(sample), Cow::Borrowed(_)))
        .count();
    println!(
        "false-positive: {false_positives}/{} ({:.1}%)",
        negatives.len(),
        false_positives as f64 * 100.0 / negatives.len() as f64
    );
    assert_eq!(false_positives, 0);
}

/// A private-key block as `ssh-keygen -t ed25519` and `openssl genrsa 2048` print it: the same
/// labels and line widths, with deterministic noise for the key.
fn key_block(
    label: &str,
    first: &str,
    width: usize,
    rows: usize,
    last: (usize, &str),
) -> Vec<String> {
    const BASE64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut noise = |length: usize| -> String {
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                char::from(BASE64[(state % 64) as usize])
            })
            .collect()
    };
    let mut block: Vec<String> = vec![format!("-----BEGIN {label}-----")];
    block.push(format!("{first}{}", noise(width - first.len())));
    block.extend((1..rows).map(|_| noise(width)));
    block.push(format!("{}{}", noise(last.0 - last.1.len()), last.1));
    block.push(format!("-----END {label}-----"));
    block
}

#[test]
fn a_read_cut_at_any_row_of_a_key_block_hides_the_rest_of_it() {
    let openssh: String = ["OPENSSH", "PRIVATE", "KEY"].join(" ");
    let pkcs8: String = ["PRIVATE", "KEY"].join(" ");
    // The OpenSSH header is the same in every key; the last line holds the comment and padding.
    let ed25519 = key_block(
        &openssh,
        "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW",
        70,
        4,
        (26, "AAAAC2RldkBsYXB0b3ABAgME=="),
    );
    let rsa = key_block(
        &pkcs8,
        "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC",
        64,
        25,
        (24, "=="),
    );
    for (name, block) in [("ed25519", ed25519), ("rsa-2048", rsa)] {
        let text: String = format!("$ cat key\n{}\n$ ls", block.join("\n"));
        let body: &[String] = &block[1..block.len() - 1];
        // A cut at each row from BEGIN to END, so 1 to all of the block's lines are read.
        let cuts = 2..=block.len() + 1;
        let total: usize = cuts.clone().count();
        let mut hidden: usize = 0;
        for lines in cuts {
            let output: Cow<'_, str> = redact_last_lines(&text, lines);
            assert!(
                output.ends_with("\n$ ls"),
                "{name} cut to {lines}: {output}"
            );
            if body.iter().all(|line| !output.contains(line.as_str())) {
                hidden += 1;
            }
        }
        println!(
            "true-positive private-key cut ({name}): {hidden}/{total} ({:.1}%)",
            hidden as f64 * 100.0 / total as f64
        );
        assert_eq!(hidden, total, "{name}");
        assert_eq!(redact_last_lines(&text, 1), "$ ls");
    }
}

#[test]
fn a_closed_key_block_above_the_cut_leaves_the_rest_alone() {
    let label: String = ["RSA", "PRIVATE", "KEY"].join(" ");
    let text: String = format!("-----BEGIN {label}-----\nMIIE\n-----END {label}-----\nok\ndone");
    assert!(matches!(
        redact_last_lines(&text, 2),
        Cow::Borrowed("ok\ndone")
    ));
    assert_eq!(redact_last_lines(&text, 0), "");
    assert_eq!(
        redact_last_lines(&text, usize::MAX),
        "«redacted:private-key»\nok\ndone"
    );
}

#[test]
fn an_end_line_without_its_begin_redacts_what_came_before_it() {
    let label: String = ["OPENSSH", "PRIVATE", "KEY"].join(" ");
    // As when a read's byte cap took the BEGIN line.
    let text: String = format!("cut-tail\n-----END {label}-----\n$ ls");
    assert_eq!(
        redact_last_lines(&text, usize::MAX),
        "«redacted:private-key»\n$ ls"
    );
    assert_eq!(redact_last_lines(&text, 2), "«redacted:private-key»\n$ ls");
    assert_eq!(redact_last_lines(&text, 1), "$ ls");
    let certificate: &str = "-----END CERTIFICATE-----\nok";
    assert!(matches!(
        redact_last_lines(certificate, 2),
        Cow::Borrowed(_)
    ));
}

#[test]
fn key_value_pairs_keep_the_key_and_separator() {
    let input = "PASSWORD=one passwd:two secret = three token:\"four\" api_key='five'";
    let expected = "PASSWORD=«redacted:password» passwd:«redacted:password» secret = «redacted:password» token:\"«redacted:password»\" api_key='«redacted:password»'";
    assert_eq!(redact(input), expected);
}

#[test]
fn uri_passwords_keep_the_user_and_host() {
    assert_eq!(
        redact("postgres://alice:pw@db/app"),
        "postgres://alice:«redacted:password»@db/app"
    );
    assert_eq!(
        redact("custom+scheme://user:x@host.example/path"),
        "custom+scheme://user:«redacted:password»@host.example/path"
    );
    // Clients split user-info at the last `@`, so the user may hold one.
    assert_eq!(
        redact("https://alice@corp.com:hunter2@host/"),
        "https://alice@corp.com:«redacted:password»@host/"
    );
    let token = format!("ghp_{}", "a1B2".repeat(9));
    let url = format!("https://{token}:x-oauth-basic@github.com/o/r");
    let redacted = redact(&url);
    assert!(!redacted.contains(&token), "{redacted}");
    assert!(
        redacted.ends_with(":«redacted:password»@github.com/o/r"),
        "{redacted}"
    );
}

#[test]
fn at_signs_outside_uri_user_info_are_not_passwords() {
    for input in [
        "postgres://alice@db/app",
        "https://example.test/users/alice:pw@db",
        "alice:pw@example.test",
    ] {
        assert!(matches!(redact(input), Cow::Borrowed(_)), "changed {input}");
    }
}

#[test]
fn dummy_pgp_private_key_block_is_redacted() {
    let label = ["PGP", "PRIVATE", "KEY", "BLOCK"].join(" ");
    let input = format!("-----BEGIN {label}-----\nnot-a-real-key\n-----END {label}-----");
    assert_eq!(redact(&input), "«redacted:private-key»");
}

#[test]
fn borrows_text_without_a_match() {
    assert!(matches!(redact("ordinary output"), Cow::Borrowed(_)));
}

#[test]
fn handles_one_megabyte_of_lossy_random_bytes() {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut bytes: Vec<u8> = Vec::with_capacity(1_048_576);
    for _ in 0..1_048_576 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.push(state as u8);
    }
    let input: Cow<'_, str> = String::from_utf8_lossy(&bytes);
    let _output: Cow<'_, str> = redact(&input);
}

#[test]
fn fuzzish_inputs_never_panic_or_leave_inserted_secrets() {
    let mut state: u64 = 0xd1b5_4a32_d192_ed03;
    for case in 0..2_000_usize {
        let mut input: String = String::new();
        for _ in 0..(state as usize % 96) {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            if let Some(character) = char::from_u32((state >> 32) as u32 % 0x11_0000) {
                input.push(character);
            }
        }
        let secret: &str = POSITIVES[case % POSITIVES.len()].1;
        input.push_str(secret);
        let output: Cow<'_, str> = redact(&input);
        assert!(!output.contains(secret), "secret survived case {case}");
        let _cut: Cow<'_, str> = redact_last_lines(&input, case % 4);
    }
}

/// Measures the false-positive rate on real terminal output. Build the corpus with
/// `crates/pinfold-api/tests/redact-corpus.sh > /tmp/corpus.txt`, then run
/// `PINFOLD_REDACT_CORPUS=/tmp/corpus.txt cargo test -p pinfold-api --test redact_corpus -- --ignored --nocapture`.
#[test]
#[ignore = "needs a corpus file from redact-corpus.sh"]
fn real_output_false_positive_rate() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::var("PINFOLD_REDACT_CORPUS")?;
    let text = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = text.lines().collect();
    let changed: Vec<&&str> = lines
        .iter()
        .filter(|line| !matches!(redact(line), Cow::Borrowed(_)))
        .collect();
    println!(
        "real-output false-positive lines: {}/{} ({:.3}%)",
        changed.len(),
        lines.len(),
        changed.len() as f64 * 100.0 / lines.len().max(1) as f64
    );
    for line in changed.iter().take(15) {
        println!("  {}", redact(line));
    }
    Ok(())
}
