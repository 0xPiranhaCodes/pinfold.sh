# Built-in tools: dependencies and a baseline

Evidence for [ADR-0045](../../adr/0045-tools-are-programs-in-their-own-pane.md), measured on
2026-11-07 on one Apple Silicon Mac with cargo 1.99.1.

`deps.sh` resolves each candidate crate in a throwaway project and checks it against this
repository's `deny.toml`:

| Crate | Crates in its tree | unsafe in its own src | Lines in its own src |
| --- | --- | --- | --- |
| pulldown-cmark 0.13.6 | 4 | 5 | 13840 |
| similar 2.7.0 | 1 | 0 | 5815 |
| unicode-width 0.2.3 | 1 | 0 | 23234 |
| ratatui 0.30.2 | 60 | 0 | 2351 |
| crossterm 0.29.0 | 26 | 17 | 11691 |
| syntect 5.3.0 | 28 | 1 | 11020 |
| ignore 0.4.34 | 13 | 0 | 8752 |
| notify 8.2.1 | 10 | 32 | 6989 |

No async runtime or C build. `cargo deny` rejects `hashbrown` 0.17.1 (Zlib) under
`ratatui-widgets`, `notify` (CC0-1.0) and ISC crates under it, and flags `bincode` 1.3.3 under
`syntect` as unmaintained (RUSTSEC-2025-0141).

`baseline.sh` measures the release `pinfold` binary idle in `pinfold mcp`:

```text
Idle pinfold mcp: 2.10 MB (median of 3, Darwin)
Binary size: 0.77 MB
```
