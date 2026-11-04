# Known issues in the macOS beta

What testers of `0.1.0-beta.1` should know before reporting a bug. A report about any of these is
still welcome if it adds a case not listed here. Report bugs with the
[beta bug form](https://github.com/0xPiranhaCodes/pinfold.sh/issues/new?template=beta-bug.yml).

## Platforms

- **macOS only.** Linux builds and CI tests exist, but the beta is for macOS, and Linux desktop
  checks wait for v0.1 ([ADR-0031](../adr/0031-macos-beta-first.md),
  [#47](https://github.com/0xPiranhaCodes/pinfold.sh/issues/47)).
- **No minimum macOS version is enforced yet.** The automated tests run on macOS 26.6 on Apple
  Silicon and on Intel, and the app has been run by hand only on macOS 27 on Apple Silicon. Older
  versions and the app on Intel are untested. If PinFold does not start on your Mac, report it
  with your macOS version ([#190](https://github.com/0xPiranhaCodes/pinfold.sh/issues/190)).
- **No update check.** To update, download the new `.dmg` and replace the app.
- **No `pinfold --version`.** The version is in PinFold > About PinFold.

## Memory

- **The window's headroom is small.** Every memory budget passes, but on an M1 Pro with a Retina
  display the window reads either about 45.5 MB or about 51 MB of its 55.86 MB budget, depending
  on how Metal lays out its memory pool. The worst reading so far left 4.40 MB, less than that
  5 MB spread ([#169](https://github.com/0xPiranhaCodes/pinfold.sh/pull/169),
  [ADR-0030](../adr/0030-macos-window-fixed-part.md)).
- **The Dock menu's memory is not measured**, as no script can right-click the Dock (#169).

## Panes and sessions

- **On macOS a descriptor numbered above 65,536 can reach a pane's program.** Every child starts
  with only the descriptors meant for it, but the check stops at 65,536
  ([#188](https://github.com/0xPiranhaCodes/pinfold.sh/pull/188)).
- **A pane can take about 11 ms longer to start** when the core was started from a shell whose
  `ulimit -n` is very high. Opened from the Dock or Finder, the limit is 256 and the cost is
  within noise (#188).
- **Restore brings back the layout, not the screen.** After a restart, panes get fresh shells
  with no scrollback ([ADR-0034](../adr/0034-m5-sessions-plan.md)).
- **A shell pane does not remember what ran in it.** Only a pane started with its own program,
  such as by `pinfold agent`, offers `was running: <command>` after a restore
  ([#145](https://github.com/0xPiranhaCodes/pinfold.sh/pull/145)).
- **Only OSC 8 hyperlinks get Open Link and Copy Link.** A plain `https://…` in output gets
  neither ([#171](https://github.com/0xPiranhaCodes/pinfold.sh/pull/171)).

## Not yet checked on an installed build

The Dock menu ([#170](https://github.com/0xPiranhaCodes/pinfold.sh/pull/170)), Open Link and Copy
Link (#171) and the About panel ([#185](https://github.com/0xPiranhaCodes/pinfold.sh/pull/185))
pass their automated tests, but nobody has yet clicked them in a signed install. The
[manual checks](manual-checks.md) cover them, and their results table records each Mac checked.

## Builds from source

A build from `cargo` lacks the hardened runtime, so another program running as you can read its
memory. The signed `.dmg` has it ([#109](https://github.com/0xPiranhaCodes/pinfold.sh/issues/109)).
