# ADR-0014: git-cliff for the changelog

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

The changelog follows Keep a Changelog and is generated from Conventional Commits. We
evaluated `git-cliff` and `release-plz`; both are maintained and Apache-2.0.

`release-plz` automates version PRs, tags and crates.io publishes. Our release and
publish workflows may only run on tags the maintainer creates, so that automation is
what we must switch off. `release-plz` itself uses `git-cliff` to write changelogs.

## Decision

Use `git-cliff` directly. Generating the changelog is a local step in release
preparation, and it has no write access to the repository's releases.

## Consequences

Version bumps stay manual, through `cargo xtask` release helpers. If those become a
burden, `release-plz` can be reconsidered with publishing disabled.
