# ADR-0016: Hand-written release workflow, not cargo-dist

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

Releases need Linux tarballs, a signed and notarized macOS universal `.dmg`, Sigstore
signatures, a CycloneDX SBOM, attestations and a Homebrew formula. Every action is
pinned to a SHA, and workflows run only on tags the maintainer creates.

We evaluated `cargo-dist` v0.33.1, which is maintained and Apache-2.0. It covers
tarballs and Homebrew. Its installer documentation lists shell, PowerShell, Homebrew,
npm and MSI, with no `.dmg`, so our main macOS artifact would be built outside it. It also
generates its own CI workflow, which we would then have to keep within our hardening
rules.

We read its documentation and did not run a trial build.

## Decision

Keep a small hand-written `release.yml`. It starts as a dry run that publishes nothing.
Signing, notarization, SBOM and publishing steps are added in M5.

## Consequences

We own a little more workflow code. We should re-evaluate if `cargo-dist` gains `.dmg`
support, or if the workflow grows hard to maintain.
