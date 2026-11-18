//! Webhook building blocks (ADR-0051 section 5): the URL rules, the body built from status
//! fields only, the config `curl` reads from standard input, and the [`Sender`] that runs it.
//!
//! The config's `[alerts] webhook` is read when the core starts. A URL these rules refuse turns
//! webhooks off with a [`Refused`] reason, which never quotes the URL: its path or query can
//! hold a secret, as an ntfy topic does.

use std::fmt::{self, Write as _};

use crate::status::Status;

mod send;

pub use send::{MOST, PER_PANE, PROXIES, QUEUE, Record, Sender, WINDOW, find_curl};

/// The longest URL taken, in bytes.
pub const MAX_URL: usize = 2048;

/// The most bytes of a hostname or an agent name put in a body; a longer one is cut.
pub const MAX_NAME: usize = 64;

/// Why a URL turns webhooks off. It names the rule, never the URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Longer than [`MAX_URL`].
    TooLong,
    /// Neither `https://` nor `http://`.
    Scheme,
    /// `http://` to anything but `127.0.0.1` or `[::1]`.
    PlainHttp,
    /// Userinfo, an `@` in the authority.
    Userinfo,
    /// A host outside `A-Z a-z 0-9 . -` or a bracketed IPv6 literal.
    Host,
    /// A port that is not 1–65535.
    Port,
    /// A path or query byte outside printable ASCII, or `"`, `\` or `#`.
    Path,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLong => "the URL is longer than 2048 bytes",
            Self::Scheme => "the URL is not https://",
            Self::PlainHttp => "http:// is only for 127.0.0.1 or [::1]",
            Self::Userinfo => "the URL holds a user name or password",
            Self::Host => "the URL's host is not a name or an IP address",
            Self::Port => "the URL's port is not 1-65535",
            Self::Path => "the URL's path holds a space, quote, backslash, # or control byte",
        })
    }
}

/// How a body is written: `[alerts] webhook_format`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    /// One JSON object.
    #[default]
    Json,
    /// One line in PinFold's words, for ntfy-style relays.
    Text,
}

/// A URL the rules allow.
///
/// ```
/// use pinfold_core::webhook::{Refused, Webhook};
///
/// let hook = Webhook::new("https://ntfy.sh/secret-topic")?.unwrap();
/// assert_eq!(hook.host(), "ntfy.sh");
/// assert!(Webhook::new("")?.is_none(), "off");
/// assert_eq!(Webhook::new("http://localhost:8080/").err(), Some(Refused::PlainHttp));
/// # Ok::<(), Refused>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Webhook {
    url: Box<str>,
    /// Where the host, without its port, sits in `url`.
    host: (u16, u16),
    loopback: bool,
}

impl Webhook {
    /// Checks `url` against the rules; an empty one means webhooks are off.
    ///
    /// # Errors
    ///
    /// The rule `url` breaks.
    pub fn new(url: &str) -> Result<Option<Self>, Refused> {
        if url.is_empty() {
            return Ok(None);
        }
        if url.len() > MAX_URL {
            return Err(Refused::TooLong);
        }
        let (https, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(Refused::Scheme);
        };
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, path) = rest.split_at(end);
        if authority.contains('@') {
            return Err(Refused::Userinfo);
        }
        let (host, port) = if authority.starts_with('[') {
            let close = authority.find(']').ok_or(Refused::Host)? + 1;
            let inner = &authority[1..close - 1];
            let ipv6 = inner.contains(':')
                && inner
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.');
            if !ipv6 {
                return Err(Refused::Host);
            }
            authority.split_at(close)
        } else {
            let colon = authority.find(':').unwrap_or(authority.len());
            let host = &authority[..colon];
            if !host.bytes().all(name_byte) {
                return Err(Refused::Host);
            }
            authority.split_at(colon)
        };
        if host.is_empty() {
            return Err(Refused::Host);
        }
        if let Some(port) = port.strip_prefix(':') {
            let digits =
                !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit());
            if !digits || !matches!(port.parse::<u32>(), Ok(1..=65_535)) {
                return Err(Refused::Port);
            }
        } else if !port.is_empty() {
            return Err(Refused::Host);
        }
        if !path
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\\' | b'#'))
        {
            return Err(Refused::Path);
        }
        // Never a name, as `localhost` can resolve elsewhere.
        let loopback = matches!(host, "127.0.0.1" | "[::1]");
        if !https && !loopback {
            return Err(Refused::PlainHttp);
        }
        let start = url.len() - rest.len();
        // Both fit: the URL is at most 2048 bytes.
        let host = (
            u16::try_from(start).unwrap_or(u16::MAX),
            u16::try_from(start + host.len()).unwrap_or(u16::MAX),
        );
        Ok(Some(Self {
            url: url.into(),
            host,
            // Plain `http` is loopback only, and `https://127.0.0.1` is still `https`.
            loopback: !https,
        }))
    }

    /// The URL's host, without its port: all that logs and the window name.
    #[must_use]
    pub fn host(&self) -> &str {
        self.url
            .get(usize::from(self.host.0)..usize::from(self.host.1))
            .unwrap_or_default()
    }

    /// Whether the URL is plain `http` to loopback.
    #[must_use]
    pub const fn loopback(&self) -> bool {
        self.loopback
    }

    /// Writes into `out` the config `curl --disable --config -` reads from standard input to
    /// post `body`, written as `format`, so the URL never reaches the process table.
    ///
    /// ```
    /// use pinfold_core::webhook::{Format, Webhook};
    ///
    /// let hook = Webhook::new("https://ntfy.sh/topic")?.unwrap();
    /// let mut config = String::new();
    /// hook.curl_config("{}", Format::Json, &mut config);
    /// assert!(config.starts_with("url = \"https://ntfy.sh/topic\"\ndata = \"{}\"\n"));
    /// # Ok::<(), pinfold_core::webhook::Refused>(())
    /// ```
    pub fn curl_config(&self, body: &str, format: Format, out: &mut String) {
        let kind = match format {
            Format::Json => "Content-Type: application/json",
            Format::Text => "Content-Type: text/plain",
        };
        let proto = if self.loopback { "=http" } else { "=https" };
        for (name, value) in [
            ("url", &*self.url),
            ("data", body),
            ("header", kind),
            ("user-agent", "pinfold"),
            ("proto", proto),
        ] {
            out.push_str(name);
            out.push_str(" = ");
            quote(value, out);
            out.push('\n');
        }
        out.push_str(
            "max-redirs = 0\nconnect-timeout = 5\nmax-time = 10\nsilent\n\
             output = \"/dev/null\"\nwrite-out = \"%{http_code}\"\n",
        );
    }
}

/// Writes `value` into `out` as one double-quoted `curl` config value: `\` and `"` escaped,
/// tab, newline and carriage return as `\t`, `\n` and `\r`, and other control bytes dropped, so
/// the value ends at its closing quote on its own line and can add no option.
///
/// ```
/// let mut out = String::new();
/// pinfold_core::webhook::quote("a\"b\nurl = x", &mut out);
/// assert_eq!(out, r#""a\"b\nurl = x""#);
/// ```
pub fn quote(value: &str, out: &mut String) {
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The fixed fields of one alert, all a body is built from: no message, label, title, command,
/// directory or pane text has a place here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Post<'a> {
    /// The state that alerted.
    pub status: Status,
    /// Whether output, not the agent's report, set it.
    pub from_output: bool,
    /// The agent profile's name from the config.
    pub agent: Option<&'a str>,
    /// The machine's hostname.
    pub host: Option<&'a str>,
    /// The tab's number.
    pub tab: u32,
    /// The pane's id.
    pub pane: u32,
    /// When it alerted, in Unix seconds.
    pub at: u64,
    /// When the pane came to the state, in Unix seconds.
    pub since: u64,
}

impl Post<'_> {
    /// Writes the body into `out`. A hostname or agent name is cut to [`MAX_NAME`] bytes and
    /// dropped if it then holds anything but `A-Z a-z 0-9 . -` (and `_` in an agent's).
    ///
    /// ```
    /// use pinfold_core::status::Status;
    /// use pinfold_core::webhook::{Format, Post};
    ///
    /// let post = Post {
    ///     status: Status::NeedsYou,
    ///     from_output: false,
    ///     agent: Some("claude"),
    ///     host: Some("studio"),
    ///     tab: 2,
    ///     pane: 7,
    ///     at: 1_763_112_000,
    ///     since: 1_763_111_958,
    /// };
    /// let mut body = String::new();
    /// post.body(Format::Json, &mut body);
    /// assert_eq!(
    ///     body,
    ///     r#"{"pinfold":1,"state":"needs-you","source":"agent","agent":"claude","host":"studio","tab":2,"pane":7,"at":1763112000,"since":1763111958}"#
    /// );
    /// body.clear();
    /// post.body(Format::Text, &mut body);
    /// assert_eq!(body, "◆ needs you · claude · tab 2 pane 7 · studio");
    /// ```
    pub fn body(&self, format: Format, out: &mut String) {
        let agent = self.agent.and_then(|name| clean(name, b'_'));
        let host = self.host.and_then(|name| clean(name, b'.'));
        // Writing to a `String` cannot fail.
        let _ = match format {
            Format::Json => {
                let field = |value: Option<&str>| {
                    value.map_or_else(|| "null".into(), |v| format!("\"{v}\""))
                };
                write!(
                    out,
                    r#"{{"pinfold":1,"state":"{}","source":"{}","agent":{},"host":{},"tab":{},"pane":{},"at":{},"since":{}}}"#,
                    self.status.name(),
                    if self.from_output { "output" } else { "agent" },
                    field(agent),
                    field(host),
                    self.tab,
                    self.pane,
                    self.at,
                    self.since,
                )
            }
            Format::Text => {
                let _ = write!(out, "{} {}", self.status.glyph(), self.status.word());
                if self.from_output {
                    out.push_str(" · from output");
                }
                if let Some(agent) = agent {
                    let _ = write!(out, " · {agent}");
                }
                let _ = write!(out, " · tab {} pane {}", self.tab, self.pane);
                if let Some(host) = host {
                    let _ = write!(out, " · {host}");
                }
                Ok(())
            }
        };
    }
}

const fn name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')
}

/// `name` cut to [`MAX_NAME`] bytes, if every byte left is a [`name_byte`] or `extra`.
fn clean(name: &str, extra: u8) -> Option<&str> {
    let name = name.get(..name.len().min(MAX_NAME)).unwrap_or(name);
    (!name.is_empty() && name.bytes().all(|b| name_byte(b) || b == extra)).then_some(name)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use proptest::prelude::*;
    use regex_lite::Regex;

    use super::*;
    use crate::status::Tracker;
    use crate::vt::{AlacrittyEngine, VtEngine};

    /// How `curl`'s config reader takes a quoted value (`tool_parsecfg.c`): `\t`, `\n`, `\r`
    /// and `\v` are those bytes, any other escaped byte is itself, and an unescaped `"` ends it.
    /// Returns the value and what follows the closing quote.
    fn unquote(line: &str) -> Option<(String, &str)> {
        let mut chars = line.strip_prefix('"')?.char_indices();
        let mut value = String::new();
        while let Some((at, c)) = chars.next() {
            match c {
                '"' => return Some((value, &line[at + 2..])),
                '\\' => value.push(match chars.next()?.1 {
                    't' => '\t',
                    'n' => '\n',
                    'r' => '\r',
                    'v' => '\x0b',
                    other => other,
                }),
                c => value.push(c),
            }
        }
        None
    }

    fn post<'a>(
        status: Status,
        from_output: bool,
        agent: Option<&'a str>,
        host: Option<&'a str>,
    ) -> Post<'a> {
        Post {
            status,
            from_output,
            agent,
            host,
            tab: 2,
            pane: 7,
            at: 1_763_112_000,
            since: 1_763_111_958,
        }
    }

    #[test]
    fn the_url_rules_allow_https_and_loopback_http_only() {
        let long = format!("https://ntfy.sh/{}", "a".repeat(MAX_URL - 16 + 1));
        let fits = &long[..MAX_URL];
        for (url, expected) in [
            ("https://ntfy.sh/topic?x=1", Ok("ntfy.sh")),
            (
                "https://hooks.example.com:8443/a/b",
                Ok("hooks.example.com"),
            ),
            ("https://[2001:db8::1]:443/", Ok("[2001:db8::1]")),
            ("http://127.0.0.1", Ok("127.0.0.1")),
            ("http://127.0.0.1:9/relay", Ok("127.0.0.1")),
            ("http://[::1]:8080/x", Ok("[::1]")),
            (fits, Ok("ntfy.sh")),
            (&long, Err(Refused::TooLong)),
            ("ftp://ntfy.sh/", Err(Refused::Scheme)),
            ("HTTPS://ntfy.sh/", Err(Refused::Scheme)),
            ("ntfy.sh/topic", Err(Refused::Scheme)),
            ("http://ntfy.sh/topic", Err(Refused::PlainHttp)),
            ("http://localhost:8080/", Err(Refused::PlainHttp)),
            ("http://127.0.0.2/", Err(Refused::PlainHttp)),
            ("https://user:pass@ntfy.sh/", Err(Refused::Userinfo)),
            ("http://evil.com@127.0.0.1/", Err(Refused::Userinfo)),
            ("https://ntfy.sh/topic#frag", Err(Refused::Path)),
            ("https://ntfy.sh#frag", Err(Refused::Path)),
            ("https://ntfy.sh/a\"b", Err(Refused::Path)),
            ("https://ntfy.sh/a\\b", Err(Refused::Path)),
            ("https://ntfy.sh/a b", Err(Refused::Path)),
            ("https://ntfy.sh/a\nurl = x", Err(Refused::Path)),
            ("https://ntfy.sh/\x1b[0m", Err(Refused::Path)),
            ("https://ntfy.sh/é", Err(Refused::Path)),
            ("https://ntfy\x07.sh/", Err(Refused::Host)),
            ("https://ntfy_sh/", Err(Refused::Host)),
            ("https:///topic", Err(Refused::Host)),
            ("https://:443/", Err(Refused::Host)),
            ("https://[zz::1]/", Err(Refused::Host)),
            ("https://[::1/", Err(Refused::Host)),
            ("https://[::1]x/", Err(Refused::Host)),
            ("https://ntfy.sh:0/", Err(Refused::Port)),
            ("https://ntfy.sh:65536/", Err(Refused::Port)),
            ("https://ntfy.sh:/", Err(Refused::Port)),
            ("https://ntfy.sh:+1/", Err(Refused::Port)),
            ("https://ntfy.sh:1:2/", Err(Refused::Port)),
        ] {
            let got = Webhook::new(url).map(|hook| hook.map(|hook| hook.host().to_owned()));
            assert_eq!(got, expected.map(|host| Some(host.to_owned())), "{url:?}");
        }
        assert_eq!(Webhook::new(""), Ok(None));
    }

    #[test]
    fn a_refusal_never_quotes_the_url() {
        for refused in [
            Refused::TooLong,
            Refused::Scheme,
            Refused::PlainHttp,
            Refused::Userinfo,
            Refused::Host,
            Refused::Port,
            Refused::Path,
        ] {
            let text = refused.to_string();
            assert!(!text.contains("secret") && !text.is_empty(), "{text}");
        }
        let hook = Webhook::new("https://ntfy.sh/secret-topic")
            .unwrap()
            .unwrap();
        assert!(!hook.host().contains("secret"));
    }

    #[test]
    fn a_webhook_body_holds_only_status_fields_whatever_the_pane_printed() {
        // An agent pane told it needs you by its output, which also tried to name the tab, set
        // a title and notify with a token, and whose agent sent a message holding a secret.
        let secret = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
        let mut engine = AlacrittyEngine::new(4, 20);
        let printed = format!(
            "\x1b]7;file://evil/{secret}\x07\x1b]2;{secret}\x07\x1b]9;{secret}\x07\
             {secret}\r\n\x1b]777;notify;{secret};body\x1b\\\u{202e}\"}}\\"
        );
        engine.feed(printed.as_bytes(), &mut Vec::new());
        let mut tracker = Tracker::new(Instant::now());
        tracker.read_agent_signals(0);
        tracker.observe_agent(engine.agent_marks(), Instant::now());
        assert_eq!(tracker.status(), Status::NeedsYou);

        let json = Regex::new(
            r#"^\{"pinfold":1,"state":"(needs-you|error)","source":"(agent|output)","agent":(null|"[A-Za-z0-9._-]{1,64}"),"host":(null|"[A-Za-z0-9.-]{1,64}"),"tab":[0-9]+,"pane":[0-9]+,"at":[0-9]+,"since":[0-9]+\}$"#,
        )
        .unwrap();
        let text = Regex::new(
            r"^(◆ needs you|✕ error)( · from output)?( · [A-Za-z0-9._-]{1,64})? · tab [0-9]+ pane [0-9]+( · [A-Za-z0-9.-]{1,64})?$",
        )
        .unwrap();
        let hostile = [
            None,
            Some("claude"),
            Some(secret),
            Some("a\"b"),
            Some("a\nb"),
            Some("\u{202e}x"),
            Some(""),
            Some(printed.as_str()),
        ];
        let mut body = String::new();
        for status in [tracker.status(), Status::Error] {
            for agent in hostile {
                for host in hostile {
                    let post = post(status, tracker.from_output(), agent, host);
                    for (format, grammar) in [(Format::Json, &json), (Format::Text, &text)] {
                        body.clear();
                        post.body(format, &mut body);
                        assert!(grammar.is_match(&body), "{body:?}");
                        assert!(
                            !body.contains(secret) || agent == Some(secret) || host == Some(secret)
                        );
                        assert!(!body.starts_with('@'), "curl's data would read a file");
                    }
                }
            }
        }
        // A name is cut to 64 bytes before it is checked, so a long clean one stays.
        body.clear();
        post(Status::NeedsYou, true, None, Some(&"h".repeat(100))).body(Format::Text, &mut body);
        assert_eq!(
            body,
            format!(
                "◆ needs you · from output · tab 2 pane 7 · {}",
                "h".repeat(64)
            )
        );
        body.clear();
        post(Status::Error, false, None, None).body(Format::Json, &mut body);
        assert_eq!(
            body,
            r#"{"pinfold":1,"state":"error","source":"agent","agent":null,"host":null,"tab":2,"pane":7,"at":1763112000,"since":1763111958}"#
        );
    }

    #[test]
    fn the_curl_config_has_fixed_options_and_quoted_values() {
        let hook = Webhook::new("http://127.0.0.1:9/relay?t=1")
            .unwrap()
            .unwrap();
        let mut body = String::new();
        post(Status::NeedsYou, false, Some("claude"), Some("studio")).body(Format::Json, &mut body);
        let mut config = String::new();
        hook.curl_config(&body, Format::Json, &mut config);
        let names: Vec<_> = config
            .lines()
            .map(|line| line.split(' ').next().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "url",
                "data",
                "header",
                "user-agent",
                "proto",
                "max-redirs",
                "connect-timeout",
                "max-time",
                "silent",
                "output",
                "write-out"
            ]
        );
        let data = config
            .lines()
            .nth(1)
            .unwrap()
            .strip_prefix("data = ")
            .unwrap();
        assert_eq!(unquote(data), Some((body, "")));
        assert!(config.contains("proto = \"=http\"\n"));
        config.clear();
        Webhook::new("https://ntfy.sh/t")
            .unwrap()
            .unwrap()
            .curl_config("x", Format::Text, &mut config);
        assert!(config.contains("proto = \"=https\"\n"));
        assert!(config.contains("header = \"Content-Type: text/plain\"\n"));
        // A loopback host over `https` stays `https`, or `curl` refuses the URL (exit 1).
        config.clear();
        let local = Webhook::new("https://127.0.0.1:9").unwrap().unwrap();
        assert!(!local.loopback());
        local.curl_config("x", Format::Json, &mut config);
        assert!(config.contains("proto = \"=https\"\n"));
    }

    #[test]
    fn the_parsed_webhook_keeps_only_its_url() {
        // ADR-0051: nothing resident beyond the parsed config.
        assert!(size_of::<Webhook>() <= 24, "{}", size_of::<Webhook>());
    }

    proptest! {
        /// No value, whatever it holds, ends its line early or adds an option: it reads back as
        /// itself without control bytes, closed by its quote with nothing after it.
        #[test]
        fn a_quoted_value_reads_back_and_adds_no_option(value in any::<String>()) {
            let mut out = String::new();
            quote(&value, &mut out);
            prop_assert!(!out.contains(['\n', '\r']));
            let kept: String = value
                .chars()
                .filter(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r'))
                .collect();
            prop_assert_eq!(unquote(&out), Some((kept, "")));
        }

        /// Every URL the rules allow and every body put in the config read back as themselves.
        #[test]
        fn every_allowed_url_and_body_reads_back(
            url in "https?://([a-z0-9.-]{1,20}|127\\.0\\.0\\.1|\\[::1\\])(:[0-9]{1,5})?[!-~]{0,40}",
            agent in any::<Option<String>>(),
            host in any::<Option<String>>(),
            text in any::<bool>(),
        ) {
            let Ok(Some(hook)) = Webhook::new(&url) else { return Ok(()) };
            let format = if text { Format::Text } else { Format::Json };
            let mut body = String::new();
            post(Status::NeedsYou, false, agent.as_deref(), host.as_deref()).body(format, &mut body);
            let mut config = String::new();
            hook.curl_config(&body, format, &mut config);
            let mut lines = config.lines();
            let url_line = lines.next().unwrap().strip_prefix("url = ").unwrap();
            prop_assert_eq!(unquote(url_line), Some((url.clone(), "")));
            let data = lines.next().unwrap().strip_prefix("data = ").unwrap();
            prop_assert_eq!(unquote(data), Some((body, "")));
            prop_assert_eq!(config.lines().count(), 11);
        }
    }
}
