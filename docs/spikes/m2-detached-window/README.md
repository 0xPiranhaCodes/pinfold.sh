# M2 spike: a window started by a detached process on macOS

Question: when the Dock launches Pinfold and the launcher exits, can a detached process start
the window and still get a real app?

Method: copy `PinFold.app` to `Spike.app`, set `CFBundleExecutable` to `launcher.sh` and the
bundle ID to `sh.pinfold.Spike`, sign it ad hoc, and run `open Spike.app`. The launcher double
forks, calls `setsid` and execs the bundle's `pinfold`, then exits. Read the result with
`lsappinfo info`.

Result on macOS 27, once: the window's parent is pid 1, in no session, and `lsappinfo` lists it
under `sh.pinfold.Spike` at the bundle's path, and as in front.

Not confirmed: whether the window is visibly in front, or how it behaves across Spaces. No
person saw this run.
