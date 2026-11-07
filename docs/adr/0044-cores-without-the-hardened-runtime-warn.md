# ADR-0044: A macOS core without the hardened runtime warns and is marked

- **Status:** Proposed
- **Date:** 2026-11-07

## Context

On macOS, `PT_DENY_ATTACH` stops `ptrace` but not `task_for_pid`. `tests/hardening/audit.sh`
showed that a same-user process, ad hoc signed with the debugger entitlement, read a running
core's memory unless the core was signed with the hardened runtime (ADR-0020, threat model
"Core hardening"). This needs no admin rights, no Developer Mode and no Apple account.
`packaging/macos/bundle.sh` signs every binary in `PinFold.app` with the hardened runtime and
fails if one lacks it. Nothing else does, so the threat model left this open: should a core
without it warn or refuse a session ([#109])?

**What an attacker gains.** The core holds the scrollback of every pane, including output from
programs that have since exited, such as a printed `.env` or a key. It also holds the grant
table, the audit trail and the PTY masters of every pane. Bearer and owner tokens no longer
exist, so there are none to steal. A task port normally allows writes and thread control as
well, which `audit.sh` did not try. If it does, an attacker could change grants or write to
any pane's terminal, which is everything the API's authorization exists to stop. The window
also holds the panes it draws, and the same applies to it.

Same-user code can already do other harm that PinFold does not stop. ADR-0020 states that
same-user `ptrace` of pane programs and terminal writes are not stopped without a sandbox. An
attacker can also replace a binary under `~/.cargo/bin` before the next launch. The core's memory
matters because it is the one place that holds every pane at once, and the threat model says
it is hardened.

**Who is affected.** Every macOS build that `bundle.sh` did not sign:

| Build | Hardened runtime |
|---|---|
| `PinFold.app` from the release `.dmg`, or `bundle.sh` ad hoc | yes |
| `cargo install`, `cargo build` and `cargo run`, including CI's macOS jobs and `cargo test` | no |
| A Homebrew formula building from source | no, unless the formula signs it |
| Ad hoc `codesign -s -` without `-o runtime` | no |

Today `packaging/homebrew/pinfold.rb` is `depends_on :linux`, and the cask installs the
notarized `.dmg`, so no Homebrew install builds a macOS core yet. Linux is not affected:
`PR_SET_DUMPABLE(0)` refused both `ptrace` and `pidfd_getfd` in every build.

**A core can read its own flag.** `csops(pid, CS_OPS_STATUS, &flags, 4)` returns the code-signing
flags that the kernel holds for a running process. `CS_RUNTIME` (`0x00010000`) is the hardened
runtime, and the kernel sets it from the signature at `exec`. `tests/hardening/runtime-flag.c`
prints them, and `tests/hardening/runtime-flag.sh` runs it on itself in three signings and on a
real `pinfold-core --idle 1` before and after `bundle.sh` signs it. Output from macOS 27.0.1,
arm64, SIP on, Developer Mode off, Apple clang 17.0.0, with paths shortened:

```text
== probe reads itself; codesign: flags=0x20002(adhoc,linker-signed)
pid 28495 flags=0x22020201 runtime=no get-task-allow=no linker-signed=yes
== probe-runtime reads itself; codesign: flags=0x10002(adhoc,runtime)
pid 28499 flags=0x22011311 runtime=yes get-task-allow=no linker-signed=no
== probe-unsigned reads itself; codesign: not signed at all
Killed: 9
exit 137
== ./target/debug/pinfold-core --idle 1; codesign: flags=0x20002(adhoc,linker-signed)
pid 28551 flags=0x22020201 runtime=no get-task-allow=no linker-signed=yes
== $WORK/PinFold.app/Contents/MacOS/pinfold-core --idle 1; codesign: flags=0x10002(adhoc,runtime)
pid 28580 flags=0x22011311 runtime=yes get-task-allow=no linker-signed=no
```

A `cargo` build on Apple Silicon is linker-signed ad hoc and lacks the flag. A binary with no
signature at all is killed at launch on arm64, so it never gets to check. Intel was not tried.
Its linker does not sign by default, so the runtime flag should be clear there too. The answer must
also require `CS_GET_TASK_ALLOW` (`0x4`) clear, as that entitlement opens the task port again
under the hardened runtime.

`csops` is in libSystem but not in the public SDK's headers or in `libc` 0.2.191, so detection
needs one new `unsafe` block in `pinfold-sys`. It is described here and not added by this PR:

```rust
extern "C" {
    // xnu bsd/sys/codesign.h. libSystem exports it; the public SDK has no header.
    fn csops(pid: libc::pid_t, ops: libc::c_uint, useraddr: *mut libc::c_void, usersize: libc::size_t) -> libc::c_int;
}

/// Whether the kernel holds this process as hardened: `CS_RUNTIME` set, `CS_GET_TASK_ALLOW` clear.
pub fn hardened_runtime() -> io::Result<bool> {
    let mut flags: u32 = 0;
    // SAFETY: CS_OPS_STATUS (0) writes exactly `usersize` bytes, 4, into `flags`, which lives
    // for the call. The pid is our own, so it cannot name another process.
    let status = unsafe { csops(libc::getpid(), 0, (&raw mut flags).cast(), 4) };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(flags & 0x0001_0000 != 0 && flags & 0x0000_0004 == 0)
}
```

It is one system call at startup, with no allocation and no new dependency. Security.framework's
`SecCodeCopySelf` and `SecCodeCopySigningInformation` give the same answer through public API,
but need CoreFoundation, which the core's build of `pinfold-sys` does not link: its `menu` and
Metal features are off in the core (`crates/pinfold-sys/Cargo.toml`). The check cannot stop an attacker who
can already replace the binary. It tells an honest user that their build is weaker than the threat
model says.

Signing a source build needs no Apple account. The ad hoc signature with `-o runtime` that
`audit.sh` used refused the task port:

```sh
codesign --force --sign - --options runtime target/release/pinfold-core target/release/pinfold-window
```

## Options

The check runs where hardening already runs: in the core's `serve`, after `harden`, and in the
window. `--idle`, `--window` and `--replay` skip hardening on purpose and would skip this too.

**(a) Warn once in the window.** The first window after startup shows a PinFold-drawn notice
outside pane text, with the command above. A session started with no window warns in the log only.
- Dev: one notice per `cargo run` session. CI: no change, as tests ignore the notice. Homebrew: no change.
- The warning is gone after one dismissal, while the exposure lasts the whole session.

**(b) Refuse unless opted out.** A core without the flag exits before opening a PTY unless
`PINFOLD_ALLOW_UNHARDENED=1` or a config key is set.
- Dev: every `cargo run` fails until the developer signs the binary or sets the variable.
- CI: `crates/pinfold-cli/tests/session.rs` starts the cargo-built core in its default, hardened
  mode on `macos-latest` and `macos-26-intel`. It must sign its copy or set the variable.
- Homebrew: a future macOS formula must sign the binaries in `install`. Whether a bottle's
  relocation keeps the runtime flag was not tested.
- An opt-out set once, in a shell profile or config, is set for good. Then it is (d) with a
  harder first run. Same-user code can also write that variable or config.

**(c) Warn plus a status-bar marker.** As (a), plus a word in the status bar, such as
`unhardened`, drawn by PinFold for as long as the session lasts, in a colour that none of the
agent states uses. The window can check itself and its parent core with the same call on the
core's pid, so no link message is needed. If the core reports it instead, the link version rises
and ADR-0043 gains a row.
- Dev: developers see the marker in every unsigned session, which is honest and mildly noisy.
  Signing once after building removes it. CI: no change. Homebrew: no change, and a future
  formula that signs removes the marker.

**(d) Do nothing and document it.** The threat model and README say that only `PinFold.app` is
hardened on macOS.
- No cost to dev, CI or Homebrew. A user who builds from source is never told.

## Decision

Proposed: **(c)**. The core logs at `warn`, with no pane text, when it lacks the runtime. The
window shows the notice once with the `codesign` command and keeps the marker for the session.
The threat model and `CONTRIBUTING.md` give the same command for source builds. Linux is
unchanged.

Refusing (b) was not chosen. A same-user attacker can still `ptrace` pane programs and write to
terminals without a sandbox (ADR-0020), so refusing a session does not make an unsigned build
safe. It would break every developer's first run and CI's session tests, and an opt-out would
soon be set everywhere. (a) hides a lasting exposure after one click. (d) leaves the threat
model's promise silently false for anyone who builds from source.

The maintainer decides. This ADR changes no behaviour.

## Consequences

- `pinfold-sys` gains `hardened_runtime` on macOS, with one `unsafe` block described above. The
  core and window call it once at startup.
- Implementation tests: a unit test of the flag decoding. A macOS session test that the
  cargo-built core reports unhardened, and that a copy signed with `-o runtime` reports
  hardened, as `runtime-flag.sh` does by hand. A window snapshot with the marker.
- The marker takes status-bar width on every unsigned session, developers' included.
- Revisit if PinFold ships a source formula for macOS, or if Apple changes `csops` or the
  flag. If writes through the task port are shown to work, reconsider (b), as that makes the
  core worth more to an attacker than any one pane.

[#109]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/109
