#!/usr/bin/env python3
"""Times `pinfold` until it exits, on a new core and then with a second window on that core.

    python3 docs/spikes/m2-cold-start/measure.py target/release 3

Each run copies the binaries and uses a runtime directory of its own, and kills only processes
whose binary is in that copy. It opens real windows, so it needs a display.
"""

import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time


def running(directory, name):
    table = subprocess.run(["ps", "-axo", "pid=,comm="], capture_output=True, text=True)
    rows = (line.split(None, 1) for line in table.stdout.splitlines())
    return [int(pid) for pid, program in rows if program.strip() == f"{directory}/{name}"]


def stop(directory, name):
    for pid in running(directory, name):
        os.kill(pid, signal.SIGKILL)


def launch(directory, environment):
    start = time.monotonic()
    done = subprocess.run([f"{directory}/pinfold"], env=environment, capture_output=True, text=True)
    return f"{time.monotonic() - start:.3f} s, exit {done.returncode} {done.stderr.strip()}"


def main(source, runs):
    for run in range(1, runs + 1):
        directory = os.path.realpath(tempfile.mkdtemp(prefix="pf"))
        for name in ("pinfold", "pinfold-core", "pinfold-window"):
            shutil.copy2(f"{source}/{name}", directory)
        os.mkdir(f"{directory}/run", 0o700)
        environment = dict(os.environ, XDG_RUNTIME_DIR=f"{directory}/run")
        try:
            print(f"run {run}, first window: {launch(directory, environment)}", flush=True)
            time.sleep(1)
            stop(directory, "pinfold-window")
            time.sleep(0.5)
            print(f"run {run}, second window: {launch(directory, environment)}", flush=True)
            time.sleep(1)
        finally:
            stop(directory, "pinfold-window")
            stop(directory, "pinfold-core")
            time.sleep(0.3)
            shutil.rmtree(directory, ignore_errors=True)


main(sys.argv[1], int(sys.argv[2]))
