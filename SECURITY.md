# Security policy

## Supported versions

Pinfold is pre-alpha and has no releases, so no version is supported yet. Once
releases begin, the latest minor version receives security fixes as patch releases.

| Version     | Supported |
| ----------- | --------- |
| Pre-release | No releases yet; report issues against `main` |

## Reporting a vulnerability

**Please do not open a public issue.** Use either channel:

- [GitHub private vulnerability reporting](https://github.com/0xPiranhaCodes/pinfold.sh/security/advisories/new)
- Email satheesh.101097@gmail.com

Include the affected version or commit, your platform, reproduction steps and the
impact you expect.

## Coordinated disclosure

We follow a 90-day coordinated disclosure policy. We will work with you on a fix and
a disclosure date, and publish an advisory crediting you unless you prefer otherwise.
If a report is under active exploitation, we may agree to a shorter timeline.

## Scope

The [threat model](docs/design/threat-model.md) defines what Pinfold defends against,
in threats T1–T10.

In scope, for example:

- A pane acting as another pane, or an agent getting past a scope or approval prompt
- Program output or file contents turning into input, commands or network traffic
- Faking a prompt drawn by Pinfold
- Crashing the core with PTY bytes, API requests, config or files
- Secrets reaching disk or the network, contrary to the threat model

Out of scope:

- Actions by an **unsandboxed** agent using its own user rights. The API controls are
  guardrails, not containment; only sandboxed agents are contained. Getting out of a
  **sandboxed** profile is in scope.
- Attacks that need root, or a kernel compromise
- Vulnerabilities in dependencies that Pinfold cannot reach. Please report those
  upstream.

## Bug bounty

There is no bug bounty at this time.
