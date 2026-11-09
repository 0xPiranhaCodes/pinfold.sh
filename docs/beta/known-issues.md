# Known issues in the macOS beta

What testers of `0.1.0-beta.2` should know before reporting a bug. A report about any of these is
still welcome if it adds a case not listed here. Report bugs with the
[beta bug form](https://github.com/0xPiranhaCodes/pinfold.sh/issues/new?template=beta-bug.yml).

## Platforms

- **macOS only.** Linux builds and CI tests exist, but the beta is for macOS, and Linux desktop
  checks wait for v0.1 ([ADR-0031](../adr/0031-macos-beta-first.md),
  [#47](https://github.com/0xPiranhaCodes/pinfold.sh/issues/47)).
- **macOS 10.13.4 or later on Intel, macOS 11 or later on Apple Silicon.** That is the oldest
  macOS with every system call PinFold makes, and macOS refuses to open the app on anything
  older. But the automated tests run only on macOS 26.6 on Apple Silicon and on Intel, and the
  app has been run by hand only on macOS 27 on Apple Silicon. Older versions and the app on Intel
  are untested. If PinFold does not start on your Mac, report it with your macOS version
  ([#190](https://github.com/0xPiranhaCodes/pinfold.sh/issues/190)).
- **No update check.** To update, download the new `.dmg` and replace the app.
- **An update can end running programs.** A new window speaks to a session still running from
  the version before it ([ADR-0043](../adr/0043-window-serves-the-previous-link-version.md)). When
  the update skips one or more versions and the window cannot, the next launch restarts the
  session, which restores its tabs and directories but not what ran in them. If a pane is running
  a program, PinFold asks first, and "Not now" keeps it running.

## Install

- **In `0.1.0-beta.1`, opening PinFold from the disk image breaks it once the image is ejected**
  ([#279](https://github.com/0xPiranhaCodes/pinfold.sh/issues/279)). Its session keeps running
  from the image, a copy in Applications uses that session, and after the eject the session
  hangs, so opening PinFold makes the Dock icon bounce and no window opens. To recover in beta.1,
  run `pkill -9 pinfold-core` in Terminal.app, then open PinFold from Applications: it restores
  the session as last saved, which is at most a minute old. From `0.1.0-beta.2`, PinFold refuses
  to run from the disk image or a quarantined download and says to move it to Applications. A
  launch that finds an older session hung ends it and restores it after about 17 seconds, and one
  that finds a session running from another copy asks before restarting it.
- **Dragging PinFold to Applications before opening it may not move the icon in beta.1** (#279).
  `0.1.0-beta.2` makes the image's root folder readable by everyone (it was 0700) and opens the
  image with PinFold beside the Applications link, which likely fixes it, but nobody has dragged
  from the beta.2 image yet and no script can drag in Finder. If it still happens, note your macOS
  version, whether Finder showed "Verifying PinFold.dmg" and whether it worked after waiting a few
  seconds, and add it to #279.

## Memory

- **The window's headroom is small.** Every memory budget passes, but on an M1 Pro with a Retina
  display the window reads either about 45.5 MB or about 51 MB of its 55.86 MB budget, depending
  on how Metal lays out its memory pool. The worst reading so far left 4.40 MB, less than that
  5 MB spread ([#169](https://github.com/0xPiranhaCodes/pinfold.sh/pull/169),
  [ADR-0030](../adr/0030-macos-window-fixed-part.md)).
- **The Dock menu's memory is not measured**, as no script can right-click the Dock (#169).

## Panes and sessions

- **Restore brings back the layout, not the screen.** After a restart, panes get fresh shells
  with no scrollback ([ADR-0034](../adr/0034-m5-sessions-plan.md)).
- **A shell pane does not remember what ran in it.** Only a pane started with its own program,
  such as by `pinfold agent`, offers `was running: <command>` after a restore
  ([#145](https://github.com/0xPiranhaCodes/pinfold.sh/pull/145)).
- **Only OSC 8 hyperlinks get Open Link and Copy Link.** A plain `https://…` in output gets
  neither ([#171](https://github.com/0xPiranhaCodes/pinfold.sh/pull/171)).
- **A needs-you alert is a Dock bounce, and a banner needs your permission.** Agent panes show
  ◆ needs you and bounce the Dock icon. On macOS 27 a notification banner appears only after you
  allow PinFold's notifications, and clicking one to focus its pane is not checked yet
  ([ADR-0046](../adr/0046-agent-panes-read-needs-you-from-output.md)).

## Sandboxed agents

- **The sandbox does not keep every secret from the agent.** A sandboxed agent profile cannot
  write outside its working and temporary directories, open another terminal or type with
  TIOCSTI, and it cannot read `~/.ssh`, PinFold's config and state, or the common credential
  stores: `~/.aws`, `~/.azure`, `~/.docker`, `~/.gnupg`, `~/.kube`, `~/.netrc`,
  `~/.config/gcloud`, `~/.config/gh` and browser profiles
  ([configuration](../configuration.md#agent-profiles) lists them). A tool that needs one, such
  as `aws`, `gh`, `docker login` or `kubectl`, fails inside the sandbox. Nor does it get
  variables whose names look like secrets, such as `*_TOKEN`, `*_KEY` and `AWS_*`, so an agent
  CLI that reads its API key from the environment needs `keep_env` in its profile
  ([configuration](../configuration.md#agent-profiles)). The agent can read every other file you
  can, gets every other variable, can read and write the clipboard, and can reach the network
  ([ADR-0041](../adr/0041-sandbox-hides-writes-not-secrets.md),
  [#328](https://github.com/0xPiranhaCodes/pinfold.sh/issues/328)).

## Not yet checked on an installed build

The Dock menu ([#170](https://github.com/0xPiranhaCodes/pinfold.sh/pull/170)), Open Link and Copy
Link (#171) and the About panel ([#185](https://github.com/0xPiranhaCodes/pinfold.sh/pull/185))
pass their automated tests, but nobody has yet clicked them in a signed install. The
[manual checks](manual-checks.md) cover them, and their results table records each Mac checked.

## Builds from source

A build from `cargo` lacks the hardened runtime, so another program running as you can read its
memory. The signed `.dmg` has it. Such a core says so: its window shows the command that signs
it once and keeps `▲ unhardened` in the tab row ([#109](https://github.com/0xPiranhaCodes/pinfold.sh/issues/109)).
Signing removes both, and needs no Apple account:

```sh
codesign --force --sign - --options runtime target/release/pinfold-core target/release/pinfold-window
```
