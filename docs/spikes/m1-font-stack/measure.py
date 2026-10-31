#!/usr/bin/env python3
"""Runs each rasterizer on each font and prints the medians. Usage: measure.py <bin dir> <font:index>..."""
import pathlib, statistics, subprocess, sys

bins = pathlib.Path(sys.argv[1])
print("| Font | Size px | Stack | Baseline MB | Load MB | Load + glyphs MB | Mask KB | us per glyph |")
print("| --- | --- | --- | --- | --- | --- | --- | --- |")
for spec in sys.argv[2:]:
    path, index = spec.rsplit(":", 1)
    mb = pathlib.Path(path).stat().st_size / 1e6
    for size in ("14", "28"):
        for stack in ("fontdue", "swash"):
            runs = [subprocess.run([bins / f"with-{stack}", path, index, size], capture_output=True, text=True, check=True).stdout.split() for _ in range(5)]
            cols = [statistics.median(float(run[i]) for run in runs) for i in range(5)]
            print(f"| {pathlib.Path(path).name} ({mb:.1f} MB) | {size} | {stack} | {cols[0]:.2f} | {cols[1]:.2f} | {cols[2]:.2f} | {cols[3] / 1000:.0f} | {cols[4]:.0f} |")
