# Governance

## Maintainer model

Pinfold has a single maintainer, listed in [MAINTAINERS.md](MAINTAINERS.md). The
maintainer reviews and merges all changes, and makes the final call on direction,
releases and project policy.

## How decisions are made

- **Everyday changes** are decided in pull request review.
- **Technical decisions** a future contributor would ask "why?" about are recorded
  as [ADRs](docs/adr/). Anyone can propose one in a PR.
- **Large or cross-cutting changes** start as an RFC in GitHub Discussions, then
  become an ADR. See [CONTRIBUTING.md](CONTRIBUTING.md#adrs-and-rfcs).
- **Changes to the security model, license or this document** need the maintainer's
  explicit approval, and the reason is recorded in the PR.

Disagreements are worked out in the open on the relevant issue or PR. The maintainer
explains the reason for a final decision.

## Becoming a maintainer

People who make sustained, high-quality contributions and show good judgment in
reviews can be invited to become maintainers. The current maintainers decide by
consensus. A maintainer's first step is usually review rights over one area, recorded
in `.github/CODEOWNERS`.

Once there are three or more maintainers, this document will be revised to cover
voting and how to step down.

## Stepping away

A maintainer who can no longer take part should say so in `MAINTAINERS.md`, so that
someone else can take over. If the sole maintainer is inactive for 12 months, the
community is free to continue the project in a fork under a new name, as the licenses
allow.
