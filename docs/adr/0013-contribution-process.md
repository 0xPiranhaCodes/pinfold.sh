# ADR-0013: DCO, Conventional Commits, SemVer

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

Outside contributors need a low-friction way to contribute, and releases need
predictable versions and generated changelogs.

## Decision

Contributions use the DCO (`Signed-off-by`), not a CLA. Commit messages and PR titles
use Conventional Commits. Releases use SemVer.

## Consequences

Relicensing later would need every contributor's agreement. CI enforces sign-off and PR
title format.
