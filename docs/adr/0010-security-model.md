# ADR-0010: Security model: guardrails, plus optional sandboxing

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

Agents run as the user, so review finding B1 showed that API controls cannot contain
them. The controls must be useful against injected instructions, and honest about their
limits.

## Decision

Adopt the [threat model](../design/threat-model.md):

- scopes, with cross-pane actions needing an approval prompt drawn by the window
- protected panes (ssh, sudo, root, password prompts, plus user rules)
- sends to other panes split at newlines, re-checking the foreground process between
  lines
- secret redaction
- a hash-chained audit log
- security-relevant config changes confirmed in the window
- an optional sandbox for agent profiles (bubblewrap or Landlock on Linux, `sandbox-exec`
  on macOS)

Following finding M5, grants apply to one caller, target and action, and expire after 30
minutes. The docs must state that API controls are guardrails and only sandboxed agents
are contained.

## Consequences

Users of unsandboxed agents get visibility and friction, not isolation. Each threat
T1–T10 needs a test proving its control.
