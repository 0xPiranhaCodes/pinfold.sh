# M2 spike: how long a window takes to say hello

Question: how long should the core wait for a window before a launcher reports that it is not
ready? The wait was 2 s, and a debug build took 1.49 s on a fast Mac when cold.

Method: `python3 docs/spikes/m2-cold-start/measure.py target/release 3`, which times `pinfold`
until it exits, on a new core and then with a second window on that core. The launcher exits when
the window has said hello. Each run uses a fresh copy of the binaries. The file cache cannot be
emptied without root, so only the first run after a build is really cold.

Results on one Apple Silicon Mac, macOS 27, release profile, from two builds:

| Run                      | First window, build 1 | Second window, build 1 | First window, build 2 | Second window, build 2 |
| ------------------------ | --------------------- | ---------------------- | --------------------- | ---------------------- |
| 1, first after the build | 1.29 s                | 0.23 s                 | 1.65 s                | 0.31 s                 |
| 2                        | 0.46 s                | 0.22 s                 | 0.43 s                | 0.25 s                 |
| 3                        | 0.38 s                | 0.20 s                 | 0.37 s                | 0.20 s                 |

Decision: a core's first window gets 10 s, about 6 times the worst first window, and a later
window keeps 2 s, about 6 times the worst second window. A window that hangs is still reported,
and only the first one costs the API thread 10 s. The launcher's wait for each reply rises from
5 s to 12 s, as it can queue behind another launcher's first window and then have its own.

Not known: the time on a slow Mac. None was measured, so the factor of 6 is a margin and not a
measured ratio between machines. A window opened long after the core started may be colder than
these second windows, and it has only 2 s.
