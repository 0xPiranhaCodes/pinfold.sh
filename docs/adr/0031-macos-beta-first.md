# ADR-0031: A macOS beta comes before v0.1

- **Status:** Accepted
- **Date:** 2026-11-02

## Context

[ADR-0024](0024-tools-in-v0-2.md) sets what v0.1 holds. It does not say what ships first or on
which platforms. The maintainer decided that on 2026-11-02. Linux desktop checks need real X11 and
Wayland machines (#47), and they are the slowest part of a two-platform release.

## Decision

The first release is a beta for macOS only. v0.1, the first main release, covers macOS and Linux.

- **Blocks the beta:** the macOS window budget ([ADR-0030](0030-macos-window-fixed-part.md)), the
  manual Mac checks, and Developer ID signing and notarization.
- **Blocks v0.1, not the beta:** Linux desktop checks such as #47.
- **Stays as it is:** Linux in CI on every PR, so it does not rot.

## Consequences

Beta users are all on macOS, so Linux bugs surface later and may be found in bulk. Developer ID
signing moves ahead of the rest of M5's packaging work, and needs the maintainer's credentials.

This does not say which milestone the beta follows or when it ships. Tags and releases stay with
the maintainer.
