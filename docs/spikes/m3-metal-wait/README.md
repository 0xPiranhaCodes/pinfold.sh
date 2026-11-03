# M3: Metal completion-wait frame cost

## Verdict

Keep `waitUntilCompleted`. On a full 200x60 grid it blocks the window's thread for a median
0.44 to 0.47 ms a frame, p99 3.3 to 3.4 ms. That is under 6% of a 120 Hz frame at the median.
A three-buffer ring removes most of the wait, but needs two more instance buffers: at
32 bytes an instance, 12,000 quads take 384,000 bytes each, so 768,000 bytes more, most of the
window's 1.05 MB margin under ADR-0030. Revisit if dropped frames show up on a full screen of
output, with a ring of two first.

## Results

`./run.sh` (1000 frames after 120 warm-up), three runs, MacBookPro18,3 (M1 Pro), macOS 27.0.1
(26A434), rustc 1.99.1. Other builds ran on the machine (load average about 30), which
inflates the p99 columns.

| run | mode | frame/submit median ms | p99 ms | blocked median ms | blocked p99 ms | blocked total ms | completed ms/frame |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | wait after commit | 0.497 | 3.544 | 0.462 | 3.390 | 720.56 | 0.774 |
| 1 | 3-buffer submit | 0.023 | 0.206 | 0.069 | 1.357 | 148.52 | 0.182 |
| 2 | wait after commit | 0.479 | 3.443 | 0.435 | 3.270 | 639.32 | 0.688 |
| 2 | 3-buffer submit | 0.024 | 0.191 | 0.068 | 1.348 | 127.41 | 0.161 |
| 3 | wait after commit | 0.503 | 3.437 | 0.465 | 3.401 | 718.39 | 0.769 |
| 3 | 3-buffer submit | 0.024 | 0.157 | 0.071 | 1.197 | 129.09 | 0.160 |

## Method

The production call chain is `pinfold-render`'s `MetalRenderer::draw` to
`pinfold_sys::metal::Gpu::draw`. The latter writes one shared instance buffer, commits the
command buffer, and calls `waitUntilCompleted` before returning so the next frame can safely
rewrite that buffer.

This spike keeps the production shader and instance layout. Each frame clears a 2000x1200
render target and draws a full 200x60 text grid as 12,000 textured glyph quads. The synchronous
case uses one buffer and waits after every commit. The pipelined case uses a three-buffer ring,
waiting only before a still-in-flight slot is reused and once at the end. That makes its
frame/submit columns the CPU-side call time without the immediate wait; the separate blocked
columns and completed average keep deferred GPU work visible.

`./run.sh [frames] [warm-up-frames]` prints the machine, OS, toolchain and Markdown result table.
It requires an Apple Silicon Mac with Metal and does not build on other platforms. This crate is
an isolated workspace under `docs/spikes` as required by ADR-0019; it changes no production path
and adds no product dependency.
