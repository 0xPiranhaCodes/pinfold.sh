# ADR-0009: Agent API over a local Unix socket

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

Any language must be able to drive panes, and agents and plugins must not break between
releases (review finding H3).

## Decision

The API is newline-delimited JSON-RPC 2.0 over a Unix socket (0700 directory, 0600
socket, peer UID check). Every connection begins with a versioned `hello` handshake
carrying the protocol version and capabilities. Each method has a JSON Schema.
Deprecations last two minor versions, and MCP tool names are versioned. `pinfold ctl`
and `pinfold mcp` (MCP over stdio) are thin clients of the same API. There is no TCP
listener in v1.

## Consequences

The protocol is a public contract from its first release, and schema changes need
compatibility review.
