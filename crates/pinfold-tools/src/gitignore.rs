//! A tree's `.gitignore` files, read as data by the checkpoint walk (ADR-0050 section 3), so no
//! git runs over the tree to learn what it ignores.
//!
//! Lines are parsed as git's `add_patterns_from_buffer` does, and matched by a port of git's
//! `wildmatch` with `WM_PATHNAME`. Its abort rules stop most retries, but git's own matcher is
//! still exponential on lines such as `**/**/**/…`, so a `.gitignore` gets a budget of steps for
//! each path, and the walk stops using one that runs out, which covers more, never less. Not
//! ported: `core.ignoreCase`, as the tree's configuration is never read, so a pattern that
//! differs from a name only in case leaves the name covered.

/// Longest line kept as a pattern. Dropping a longer one covers more, never less, and bounds
/// the matcher's recursion, one level per run of stars.
const MAX_LINE: usize = 1024;
/// Steps one `.gitignore`'s patterns may take on one path, far past what a real file needs: a
/// call of the matcher, or a character a star skips.
const STEPS: u32 = 1 << 18;

/// A `.gitignore` whose patterns ran past their budget on a path, so the walk drops it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OverBudget;

/// One line of a `.gitignore`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pattern {
    glob: Vec<u8>,
    negated: bool,
    dir_only: bool,
    /// Holds a `/` before its last character, so it matches the path from the `.gitignore`'s
    /// directory rather than the name alone.
    anchored: bool,
}

/// The patterns in a `.gitignore`'s text, in order.
pub(crate) fn parse(text: &[u8]) -> Vec<Pattern> {
    let text = text.strip_prefix(b"\xef\xbb\xbf").unwrap_or(text);
    text.split(|&c| c == b'\n').filter_map(pattern).collect()
}

fn pattern(line: &[u8]) -> Option<Pattern> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.first() == Some(&b'#') || line.len() > MAX_LINE {
        return None;
    }
    let line = trim_trailing_spaces(line);
    let (negated, line) = match line.strip_prefix(b"!") {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let (dir_only, line) = match line.strip_suffix(b"/") {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let anchored = line.contains(&b'/');
    let glob = line.strip_prefix(b"/").unwrap_or(line);
    (!glob.is_empty()).then(|| Pattern {
        glob: glob.to_vec(),
        negated,
        dir_only,
        anchored,
    })
}

/// Drops trailing spaces unless the first of them is escaped, as git's `trim_trailing_spaces`.
fn trim_trailing_spaces(line: &[u8]) -> &[u8] {
    let (mut end, mut i) = (line.len(), 0);
    let mut first_space = None;
    while i < line.len() {
        match line[i] {
            b' ' => {
                first_space.get_or_insert(i);
            }
            b'\\' => {
                i += 1;
                first_space = None;
            }
            _ => first_space = None,
        }
        i += 1;
    }
    if let Some(space) = first_space {
        end = space;
    }
    &line[..end]
}

/// Whether the last of `patterns` to match `path`, relative to their `.gitignore`'s directory,
/// ignores it (`Some(true)`) or brings it back (`Some(false)`); `None` if none matches.
///
/// # Errors
///
/// [`OverBudget`] if the patterns ran out of steps before an answer.
pub(crate) fn matches(
    patterns: &[Pattern],
    path: &[u8],
    is_dir: bool,
) -> Result<Option<bool>, OverBudget> {
    let name = path.rsplit(|&c| c == b'/').next().unwrap_or(path);
    let mut steps = STEPS;
    let found = (patterns.iter().rev())
        .filter(|p| is_dir || !p.dir_only)
        .find(|p| wild(&p.glob, if p.anchored { path } else { name }, &mut steps) == Wild::Match);
    match (found, steps) {
        (Some(p), _) => Ok(Some(!p.negated)),
        (None, 0) => Err(OverBudget),
        (None, _) => Ok(None),
    }
}

/// Takes one step from `steps`, or says there are none left.
fn spent(steps: &mut u32) -> bool {
    match steps.checked_sub(1) {
        Some(left) => {
            *steps = left;
            false
        }
        None => true,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wild {
    Match,
    NoMatch,
    /// Nothing later in the text can match: every outer star stops trying.
    AbortAll,
    /// A single star reached a `/`: only an outer `**` may go on trying.
    AbortToStarStar,
}

/// git's `dowild` with `WM_PATHNAME`: `*`, `?` and classes never match `/`, `**` between
/// slashes matches any number of directories.
fn wild(p: &[u8], t: &[u8], steps: &mut u32) -> Wild {
    if spent(steps) {
        return Wild::AbortAll;
    }
    let (mut pi, mut ti) = (0, 0);
    while pi < p.len() {
        let Some(&c) = t.get(ti) else {
            return if p[pi] == b'*' {
                star(p, pi, t, ti, steps)
            } else {
                Wild::AbortAll
            };
        };
        match p[pi] {
            b'\\' => {
                pi += 1;
                if p.get(pi) != Some(&c) {
                    return Wild::NoMatch;
                }
            }
            b'?' if c == b'/' => return Wild::NoMatch,
            b'?' => {}
            b'*' => return star(p, pi, t, ti, steps),
            b'[' => match class(p, pi, c) {
                Ok((end, true)) if c != b'/' => pi = end,
                Ok(_) => return Wild::NoMatch,
                Err(abort) => return abort,
            },
            literal if literal != c => return Wild::NoMatch,
            _ => {}
        }
        pi += 1;
        ti += 1;
    }
    if ti < t.len() {
        Wild::NoMatch
    } else {
        Wild::Match
    }
}

/// The star run at `p[pi]` against `t[ti..]`.
fn star(p: &[u8], mut pi: usize, t: &[u8], ti: usize, steps: &mut u32) -> Wild {
    let start = pi;
    while p.get(pi) == Some(&b'*') {
        pi += 1;
    }
    let next = p.get(pi).copied();
    let match_slash = pi - start > 1
        && (start == 0 || p[start - 1] == b'/')
        && (next.is_none_or(|c| c == b'/')
            || (next == Some(b'\\') && p.get(pi + 1) == Some(&b'/')));
    if match_slash && next == Some(b'/') && wild(&p[pi + 1..], &t[ti..], steps) == Wild::Match {
        return Wild::Match;
    }
    let rest = &t[ti..];
    match next {
        None if !match_slash && rest.contains(&b'/') => return Wild::AbortToStarStar,
        None => return Wild::Match,
        // One star then a slash: the star takes the rest of this component.
        Some(b'/') if !match_slash => {
            let Some(slash) = rest.iter().position(|&c| c == b'/') else {
                return Wild::AbortAll;
            };
            return wild(&p[pi..], &rest[slash..], steps);
        }
        _ => {}
    }
    let mut i = 0;
    while let Some(&c) = rest.get(i) {
        let mut c = c;
        let pc = p[pi];
        // Before a literal, skip to its next occurrence, never past a slash a star cannot take.
        if !matches!(pc, b'*' | b'?' | b'[' | b'\\') {
            while let Some(&now) = rest.get(i) {
                if now == pc || (!match_slash && now == b'/') {
                    break;
                }
                if spent(steps) {
                    return Wild::AbortAll;
                }
                i += 1;
            }
            match rest.get(i) {
                Some(&now) if now == pc => c = now,
                // The literal is nowhere a later start could reach either.
                _ if match_slash => return Wild::AbortAll,
                _ => return Wild::AbortToStarStar,
            }
        }
        match wild(&p[pi..], &rest[i..], steps) {
            Wild::NoMatch if !match_slash && c == b'/' => return Wild::AbortToStarStar,
            Wild::NoMatch => {}
            Wild::AbortToStarStar if match_slash => {}
            other => return other,
        }
        i += 1;
    }
    Wild::AbortAll
}

/// The class opening at `p[pi]` against `c`: where it ends (its `]`) and whether `c` is in it,
/// or the abort for a class with no end.
fn class(p: &[u8], mut pi: usize, c: u8) -> Result<(usize, bool), Wild> {
    pi += 1;
    let negated = matches!(p.get(pi), Some(b'!' | b'^'));
    if negated {
        pi += 1;
    }
    let (mut prev, mut hit) = (0u8, false);
    loop {
        let Some(&pc) = p.get(pi) else {
            return Err(Wild::AbortAll);
        };
        let mut this = pc;
        if pc == b'\\' {
            pi += 1;
            this = *p.get(pi).ok_or(Wild::AbortAll)?;
            hit |= this == c;
        } else if pc == b'-' && prev != 0 && p.get(pi + 1).is_some_and(|&n| n != b']') {
            pi += 1;
            let mut high = p[pi];
            if high == b'\\' {
                pi += 1;
                high = *p.get(pi).ok_or(Wild::AbortAll)?;
            }
            hit |= (prev..=high).contains(&c);
            this = 0;
        } else if pc == b'[' && p.get(pi + 1) == Some(&b':') {
            let name = pi + 2;
            let close = name
                + p[name..]
                    .iter()
                    .position(|&b| b == b']')
                    .ok_or(Wild::AbortAll)?;
            if close == name || p[close - 1] != b':' {
                // No `:]`: an ordinary `[` in the set.
                hit |= c == b'[';
            } else {
                hit |= named(&p[name..close - 1], c).ok_or(Wild::AbortAll)?;
                pi = close;
                this = 0;
            }
        } else {
            hit |= pc == c;
        }
        prev = this;
        pi += 1;
        if p.get(pi) == Some(&b']') {
            return Ok((pi, hit != negated));
        }
    }
}

/// Whether `c` is in the POSIX class `name`, or `None` for an unknown class.
fn named(name: &[u8], c: u8) -> Option<bool> {
    Some(match name {
        b"alnum" => c.is_ascii_alphanumeric(),
        b"alpha" => c.is_ascii_alphabetic(),
        b"blank" => c == b' ' || c == b'\t',
        b"cntrl" => c.is_ascii_control(),
        b"digit" => c.is_ascii_digit(),
        b"graph" => c.is_ascii_graphic(),
        b"lower" => c.is_ascii_lowercase(),
        b"print" => c.is_ascii_graphic() || c == b' ',
        b"punct" => c.is_ascii_punctuation(),
        b"space" => matches!(c, b' ' | b'\t' | b'\n' | b'\r'),
        b"upper" => c.is_ascii_uppercase(),
        b"xdigit" => c.is_ascii_hexdigit(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn ignored(text: &str, path: &str, is_dir: bool) -> bool {
        matches(&parse(text.as_bytes()), path.as_bytes(), is_dir) == Ok(Some(true))
    }

    #[test]
    fn globs_match_as_gits_wildmatch() {
        let m =
            |p: &str, t: &str| wild(p.as_bytes(), t.as_bytes(), &mut STEPS.clone()) == Wild::Match;
        assert!(m("*.[oa]", "x.o") && m("*.[oa]", "x.a") && !m("*.[oa]", "x.c"));
        assert!(!m("*.o", "d/x.o"), "a star stops at a slash");
        assert!(m("**/foo", "foo") && m("**/foo", "a/b/foo") && !m("**/foo", "afoo"));
        assert!(m("a/**/b", "a/b") && m("a/**/b", "a/x/y/b") && !m("a/**/b", "ab"));
        assert!(m("a/**", "a/x/y") && !m("a/**", "a"));
        assert!(
            m("a**b", "axxb") && !m("a**b", "a/b"),
            "** away from slashes is one star"
        );
        assert!(m("a/*/c", "a/b/c") && !m("a/*/c", "a/b/x/c"));
        assert!(m("[]]", "]") && m("[!a]", "b") && !m("[!a]", "a") && m("[^a]x", "bx"));
        assert!(m("[a-c]", "b") && !m("[a-c]", "d") && m("[-a]", "-") && m("[a-]", "-"));
        assert!(m("[[:digit:]]x", "7x") && !m("[[:digit:]]x", "ax") && m("[[:space:]]", " "));
        assert!(m("[[:x]", "[") && !m("[[:nope:]]", "n") && !m("[ab", "a"));
        assert!(m("\\*", "*") && !m("\\*", "a") && m("?", "a") && !m("?", "/"));
        assert!(!m("[/]", "/"), "a class never matches a slash");
    }

    #[test]
    fn lines_parse_as_git_reads_them() {
        assert!(ignored("# c\n\n*.log\n", "a.log", false));
        assert!(!ignored("#a.log\n", "#a.log", false), "a comment");
        assert!(ignored("\\#a.log\n", "#a.log", false), "an escaped hash");
        assert!(ignored("\\!keep\n", "!keep", false));
        assert!(
            ignored("a.log   \n", "a.log", false),
            "trailing spaces dropped"
        );
        assert!(
            ignored("a\\ \n", "a ", false),
            "an escaped trailing space kept"
        );
        assert!(ignored("\u{feff}*.o\r\n", "x.o", false), "a BOM and CRLF");
        assert!(ignored("build/\n", "build", true) && !ignored("build/\n", "build", false));
        assert!(ignored("/top\n", "top", false) && !ignored("/top\n", "a/top", false));
        assert!(
            ignored("top\n", "a/top", false),
            "no slash matches the name at any depth"
        );
        assert!(ignored("a/b\n", "a/b", false) && !ignored("a/b\n", "x/a/b", false));
        assert!(
            !ignored("*.log\n!keep.log\n", "keep.log", false),
            "the last match wins"
        );
        assert!(ignored("!keep.log\n*.log\n", "keep.log", false));
        assert_eq!(matches(&parse(b"x\n"), b"y", false), Ok(None));
        assert!(parse(b"/\n!\n#\n").is_empty());
        let long = format!("{}\n", "a".repeat(MAX_LINE + 1));
        assert!(
            parse(long.as_bytes()).is_empty(),
            "a line over the cap is dropped"
        );
    }

    #[test]
    fn many_stars_cost_little() {
        let start = Instant::now();
        let pattern = "*a".repeat(MAX_LINE / 2 - 1) + "b";
        let path = "a".repeat(4096);
        assert!(!ignored(&pattern, &path, false));
        // Exponential in git: 8 s at 22 repeats on a 40-component path.
        let pattern = "**/".repeat(MAX_LINE / 3) + "x";
        let path = "a/".repeat(2048) + "y";
        let patterns = parse(pattern.as_bytes());
        assert_eq!(matches(&patterns, path.as_bytes(), false), Err(OverBudget));
        assert!(
            ignored(&format!("{pattern}\n**/y\n"), &path, false),
            "a match ends the search"
        );
        assert_eq!(
            matches(&patterns, b"a/b", false),
            Ok(None),
            "the budget is per path"
        );
        assert!(start.elapsed().as_secs() < 2, "{:?}", start.elapsed());
    }
}
