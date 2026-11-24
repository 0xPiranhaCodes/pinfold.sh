# ADR-0053: The audit ring holds fixed-size records and builds text when read

- **Status:** Accepted (decided by the coordinator for the maintainer, 2026-11-22)
- **Date:** 2026-11-22

## Context

The core records every answered API call in the audit log (`crates/pinfold-api/src/audit.rs`;
threat model T1). The log is evidence of what agents did: `audit.read`, `pinfold ctl log` and
the optional `audit.log` file show each call's caller, method, target, scope, outcome and reason,
hash-chained. Each entry also kept its hashes, and three heap strings: the method, the scope
and the reason. The ring was capped at 256 KiB of strings and slots, about 1,101 typical entries
(`reports_typical_capacity`, removed by this ADR). Once the ring filled, the core kept about
320 KB live: 256 KiB of slots plus malloc's rounding of the three strings
([#506](https://github.com/0xPiranhaCodes/pinfold.sh/issues/506), from #502's diagnosis). An
agent that polls fills it in minutes. The maintainer's rule is no resident memory growth.

Most of that text repeats. The method and scope come from fixed tables. About 110 reasons are
literals in the core's sources. The rest are built at run time, for example
`the sandbox will not start: {error}`, which carries a path or bwrap's version output, and
`cannot save the session: {error}`. Those cannot be enums. Target panes, grant ids and send
sizes come from the request and are untrusted `u64` values, and the time is Unix milliseconds.

## Decision

The option 1 of #506, as the coordinator decided it, with the conditions below. It follows
ADR-0052's timeline pattern.

1. **A 32-byte record with no string.** The fields are: `at_ms` (`u64`), the target (`u64`),
   one `u64` argument and the caller (`u32`), and then one byte each for the method (an index
   into `METHODS`), the scope (an index into eight scopes), the flags and the reason. The flags
   hold the outcome, the terminal bit, whether there is a caller and a target, and the detail.
   A compile-time assert holds the size at 32 bytes. The argument is a send's byte count or a
   revoked grant's id, which the reply writes as `, N bytes` or `, grant N`. The core numbers
   its own panes, so a `u32` caller is never reached; one over it is refused, never cut.
2. **Fixed reasons are an index.** `audit/reasons.rs` holds every literal reason, sorted, and
   recording finds a reason by binary search. A test scans the sources the core answers from
   for `Err("…")`, `ok_or("…")` and `#[error("…")]` literals and fails if one is missing, so
   drift is caught in CI.
3. **Other reasons go to a fixed 8 KiB text ring.** It is allocated at the first such reason
   and never grows. New text overwrites the oldest text, never an entry, so a caller that
   provokes long reasons cannot flush the log: the sandbox's refusal quotes the pane's own
   directory, which an agent controls. The ring keeps all 2,048 entries whatever the reasons.
   Each such entry's 32-byte hash is kept in a buffer indexed by sequence, allocated at the
   first one, so the chain still links once its text is gone. Its reason then reads
   `reason no longer held in memory (N bytes)`, never another entry's text and never empty,
   and so does a reason longer than the whole ring. The disk file always gets the full text.
4. **Hashes are built when read.** The log keeps only the hash before its oldest entry and
   the newest hash. Eviction hashes the evicted entry once, which is one SHA-256 of about 100
   bytes. A read rebuilds each entry's text and links from the oldest one, and takes a kept
   hash for an entry whose reason is not in the table. The canonical bytes
   are unchanged, so the hashes, the `audit.read` reply and the file's lines match what the
   string ring wrote, byte for byte. `golden.jsonl`, written by the old code, checks this for
   every method and outcome. Existing `audit.log` files still verify.
5. **2,048 entries.** That is more than the 1,101 the old ring held, so `audit.read`'s 1,000
   are always there and a read of one pane reaches further back. The ring is at most 64 KiB of
   slots, plus 8 KiB of text and 64 KiB of hashes once reasons outside the table arrive, against
   about 320 KB before. The slots are allocated at the first call and the other two at the first
   reason outside the table, and pages are touched only as entries fill them, so the idle core
   does not change. A recorded call allocates nothing in memory, which a
   counting allocator test checks; `pinfold_sys::counting` holds it, because `unsafe` lives
   only there. Writing to disk with `api.audit_log` still builds the line, which is transient.

| Option | For | Against |
| --- | --- | --- |
| **32-byte records, a reason table and an 8 KiB text ring (chosen)** | Faithful for every field at its full width. Bounded at 136 KiB. Reply and hashes unchanged. No caller can shorten the log. | Twice the 16-byte timeline entry. An old reason outside the table can be replaced by a marker. |
| 16-byte records that spill wide values to the text ring | About 32 KiB less at a full ring. | A second encoding for every wide target, grant, size and time, for a saving within the mem-gate's noise. |
| Evict whole entries to make room for text | Every retained entry keeps its text. | A few long reasons evict every older entry: an agent could flush the log in two calls. |
| Every reason as an enum | No text ring. | It refactors every handler, well over 400 lines, and paths and io errors are not enums. |
| A ring file on disk (option 2 of #506) | The core keeps only a head. | Not needed: option 1 keeps the reply contract. |

## Consequences

- The `Core after 1,000 API calls` mem-gate row falls from about 0.65 MB over the idle core
  to about 0.25 MB over it on an M1 Pro, not to within noise as #506 hoped. The rest is not the
  ring, whose slots are 64 KiB for these calls, so its 4 MB budget stays until that is traced.
- A new literal reason must be added to `audit/reasons.rs`, and the test says so. A new method
  is appended to `METHODS` and to the timeline's table in the same order, which a test checks.
- `Log::iter` builds entries, with their text and hashes, on each read. That is about 2,048
  SHA-256s per `audit.read`, about a millisecond. The timeline reads `Log::briefs`, which builds
  nothing.
- `verify` recomputes the hashes of entries with table reasons. A kept hash is taken as it is,
  since memory is not what the chain guards: the file is, and `ctl log --verify` checks every
  line of it.
- Revisit if reasons built at run time become common enough that recent ones are marked, or if
  the audit must outlive the core in memory.
