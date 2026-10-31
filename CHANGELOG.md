# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Entries are generated from Conventional Commits with git-cliff
([ADR-0014](docs/adr/0014-changelog-tool.md)).

## [Unreleased]

### Added

- Project governance, security policy, contribution guide and license files.
- Architecture decision records ADR-0001 to ADR-0024.
- Threat model with the PTY-based identity design.
- Roadmap, and memory targets updated from the M0 measurements.
- `cargo xtask mem-gate` now measures the core on Linux and macOS.
- Cargo workspace with empty crates, `cargo xtask`, CI and a dry-run release workflow.
- M0 spike results for caller identity, tiered scrollback, the process split and window memory, with the M0 report.
