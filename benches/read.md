# `pane.read` latency by `lines` (issue #257)

[`read.rs`](read.rs) starts a reactor with one 120 by 50 pane, feeds it 100,000 lines, every tenth
300 characters long so it wraps over three rows, and times 200 `Reactor::read` calls per `lines`
value. The store keeps 9,973 of those rows (`LINE_CAP`). `all` is a read without `lines`. Run it
with `cargo run --release -p pinfold-benches --bin read`.

Before #257 a read with `lines` copied the whole history and then dropped all but the last
lines. Now it copies the last `lines` plus a screen of rows, and doubles that until those rows
hold the lines. The bytes returned are the same before and after.

On one Apple M1 Pro, macOS 27.0.1, before:

| lines | p50 µs | p99 µs | bytes |
|---|---|---|---|
| all | 2933 | 3453 | 348515 |
| 0 | 2923 | 3049 | 0 |
| 1 | 2929 | 3100 | 3 |
| 50 | 2922 | 3087 | 2071 |
| 1000 | 2934 | 3150 | 41591 |
| 1000000 | 3003 | 3160 | 348515 |

After:

| lines | p50 µs | p99 µs | bytes |
|---|---|---|---|
| all | 2979 | 3625 | 348515 |
| 0 | 9 | 10 | 0 |
| 1 | 13 | 14 | 3 |
| 50 | 29 | 31 | 2071 |
| 1000 | 1043 | 1301 | 41591 |
| 1000000 | 3076 | 5332 | 348515 |

1,000 lines here take about 1,200 rows, more than the first guess of 1,050, so the read copies
1,050 rows and then 2,100. A `lines` past the history copies everything once, as `all` does.
