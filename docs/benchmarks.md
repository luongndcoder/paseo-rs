# Benchmarks (2026-10-17)

Rust terminal worker, Rust edge (`paseod`) and two crate microbenches against the TS implementation.
All numbers are the median of 3 runs unless stated. Raw per-run output lived in a scratch dir; the
scripts are in `bench/` and reproduce every table.

## Machine and conditions

- Apple M1 Pro, 32 GiB RAM, macOS (Darwin 25.6.0), Node 26.3.0, Rust release profile (`cargo build --release`).
- Not a perfectly quiet machine: no cargo/vitest/rustc was running, but the user's main Paseo daemon
  (port 6767) and unrelated MCP processes were idle in the background. Treat differences under ~10% as noise.
- TS side: integration worktree `integration/rust-all` (`.claude/worktrees/integration`, see STATUS doc).
  TS code runs through `tsx` / Node `--experimental-strip-types` (dev mode), not the built `dist/`. This inflates
  TS cold start and RSS relative to a packaged daemon.
- Never touched port 6767 or `~/.paseo`; every run used a fresh temp `PASEO_HOME`.

## Method

| Script | What it does |
|---|---|
| `bench/terminal-bench.mts` | Drives `createWorkerTerminalManager` (the real `worker-terminal-manager.ts`) with `PASEO_TERMINAL_WORKER` unset (Node worker) or `=rust`. Echo: `/bin/cat` pty, one `a` keystroke every 10 ms, 600 samples (100 warm-up dropped), time to first output event. Throughput: `cat` of a 64 MiB base64 text file in a pty. Snapshot: `getTerminalState` on a live terminal holding that 64 MiB in scrollback, and on the small cat terminal; median of 7. RSS: `ps` of the worker process only (shells excluded). |
| `bench/edge-bench.mts` | In-process TS test daemon (`createTestPaseoDaemon`) with `PASEO_EDGE` unset (client talks to the daemon port) or `=rust` (client talks to `paseod`, which is attached to the daemon). `ping` RTT, and JSON frame round trip via `fs.file.write.request` with 1 KiB / 16 KiB / 256 KiB `content` (client to daemon frame is the sized one, the response is small). Load: 8 extra clients sending 256 KiB writes for 5 s while a 9th measures `ping`. |
| `bench/daemon-mem-bench.mjs` | Spawns the full supervised daemon (`scripts/supervisor-entrypoint.ts --dev`) on a free port, measures time until the port accepts TCP, waits 20 s, sums RSS over the process tree. |
| `crates/paseo-timeline/examples/bench_projection.rs` + `bench/timeline-bench.mts` | `project_rows` / `projectTimelineRows` in projected mode over `bench/timeline-300.json` (300 rows: assistant and reasoning chunk bursts, tool-call running/completed pairs). Both produce 27 projected entries. 20k iterations x 5, median. |
| `crates/paseo-git/examples/bench_status.rs` + `bench/git-status-bench.mts` | `get_checkout_status` / `getCheckoutStatus` on the `paseo-vietnamese-voice` repo (5.7k tracked files, many worktrees), 30 calls after 3 warm-ups. |

## (a) Terminal worker: Node vs Rust (`PASEO_TERMINAL_WORKER`)

| Metric | Node worker | Rust worker | Rust vs Node |
|---|---|---|---|
| Cold start (spawn worker + first terminal ready) | 282 ms | 82 ms | 3.4x faster |
| Keystroke echo p50 | 2.18 ms | 0.50 ms | 4.4x faster |
| Keystroke echo p99 | 7.23 ms | 4.30 ms | 1.7x faster |
| Throughput, 64 MiB `cat` | 44.7 MB/s | 62.8 MB/s | 1.4x faster |
| Snapshot, small terminal | 2.2 ms | 2.7 ms | 1.2x slower (was 2.3x) |
| Snapshot, terminal with 64 MiB output | 85 ms | 196 ms | 2.3x slower (was 3.4x); about 90 ms of it is the TS reviver, see below |
| Worker RSS, idle (1 terminal) | 111 MB | 9.5 MB | 12x smaller |
| Worker RSS, after the 64 MiB output and 7+7 snapshots | 303 MB | 37 MB | 8x smaller (was 240 MB) |
| Worker RSS, 11 terminals | 304 MB | 39 MB | 8x smaller |

Reading: echo, throughput and RSS are better than Node. Snapshot rows were re-measured after the snapshot
rewrite (median of 3 runs, `bench/terminal-bench.mts`, machine load average 7 to 13 from other work, so absolute
numbers are noisy; the Node row of the same runs was 84.8, 84.3 and 101 ms).

Snapshot cost, before and after:

- The worker built the state as a `serde_json::Value` tree (one map per cell, about 1M cells for a full
  scrollback), then printed it. That was most of the time and, since the allocator keeps the freed tree, most of the
  RSS (240 MB). `emulator/snapshot.rs::write_state` now streams the JSON straight into one pre-sized buffer
  (attribute halves of the cell JSON cached per fg/bg pair) and the line goes to stdout with no copy. Output is
  byte-identical to the old `Value` printout (checked on a 8.9 MB and a 210 KB snapshot with `cmp`, and the golden
  tests assert `write_state == serde_json::to_string(snapshot)` for every snapshot step).
- Worker-side snapshot time measured with a plain JSONL client: 8.9 MB state 99 to 140 ms before, 47 ms after
  (includes the client's `JSON.parse`); 210 KB state 2.3 to 4.5 ms before, 0.8 ms after.
- The scrollback limit is 1000 lines, same as `terminal.ts` (`scrollback: 1000`). The 240 MB RSS was the snapshot
  `Value` trees, not retained scrollback.
- Remaining gap on the big snapshot is on the TS side: `stdio-terminal-worker.ts` parses every line with a
  `JSON.parse` reviver (for `$b64`). On the 8.9 MB state a plain `JSON.parse` takes 17 ms and the reviver version
  105 ms. Skipping the reviver for lines that do not contain `"$b64"` would put the Rust path at or below Node. Not
  changed here (outside the worker crate).

## (b) Edge: direct vs through `paseod`

| Metric | Direct | Via paseod | Delta |
|---|---|---|---|
| `ping` RTT p50 / p99 | 0.11 / 0.25 ms | 0.16 / 0.48 ms | +0.05 / +0.23 ms |
| 1 KiB frame p50 / p99 | 0.30 / 0.52 ms | 0.41 / 0.76 ms | +0.11 / +0.24 ms |
| 16 KiB frame p50 / p99 | 0.36 / 0.54 ms | 0.51 / 0.73 ms | +0.15 / +0.19 ms |
| 256 KiB frame p50 / p99 | 1.41 / 1.89 ms | 1.67 / 2.40 ms | +0.26 / +0.51 ms |
| Connection setup p50 (WS connect + hello) | 1.07 ms | 2.92 ms | +1.85 ms |
| Daemon start incl. edge spawn (harness) | 110 ms | 113 ms | about +3 ms |
| 256 KiB write throughput, 8 clients, 5 s | 783 frames/s | 583 frames/s | 25% lower |
| `ping` under that load p50 / p99 | 2.74 / 5.83 ms | 2.82 / 8.48 ms | |
| paseod RSS idle | n/a | 6.8 MB | plan target < 30 MB: met |
| paseod RSS under load (end of 5 s) | n/a | 15.8 MB | |

Plan acceptance "added p99 < 1 ms" holds for every size measured (worst +0.51 ms at 256 KiB). The 25% drop in
saturated throughput comes from the extra hop sharing one CPU with the load generator and the Node daemon; the
bench process (daemon + 9 clients in one process) is not a clean saturation test.

## (c) Whole-daemon memory, idle (20 s after ready)

Full supervised daemon, dev (`tsx`) mode, no terminals created.

| Variant | Supervisor | TS daemon | Terminal worker | paseod | esbuild | Total |
|---|---|---|---|---|---|---|
| TS + Node worker | 107 MB | 909 MB | 111 MB | n/a | 42 MB | 1169 MB |
| TS + Rust worker | 107 MB | 907 MB | 2.3 MB | n/a | 43 MB | 1059 MB |
| TS + Rust worker + paseod | 107 MB | 931 MB | 2.3 MB | 4.7 MB | 42 MB | 1086 MB |

The TS daemon dominates (about 0.9 GB in dev mode, with large run-to-run spread: 792 to 940 MB). Moving the
terminal worker to Rust saves about 110 MB; adding paseod costs about 5 MB. A packaged `dist/` daemon would be much
smaller and was not measured (the copied `dist/` predates the merges). The whole-daemon-in-Rust comparison is not possible yet:
paseod does not host the core in a running daemon (see STATUS).

## (d) Cold start

| Item | Time |
|---|---|
| Node terminal worker, spawn to first terminal | 282 ms |
| Rust terminal worker, spawn to first terminal | 82 ms |
| paseod spawn to `paseo:ready` (attached mode) | about 3 to 20 ms (harness delta, includes process spawn) |
| Full TS daemon, spawn to TCP accepting, dev mode | 2018 ms |
| Same with `PASEO_EDGE=rust` | 267 ms |

The last row is not an apples-to-apples start: paseod binds the public port immediately and the TS backend
finishes starting behind it, so the port accepts connections long before the daemon can answer. Backend readiness
time is unchanged (about 2 s in dev mode).

## Crate microbenches

| Bench | TS | Rust | Note |
|---|---|---|---|
| Timeline projection, 300-row bursty fixture | 45.8 us | 143.9 us | Rust 3.1x slower. Same output (27 entries). Row to entry conversion clones every item; not optimized. |
| `get_checkout_status` on a 5.7k-file repo, p50 / p90 | 299 / 315 ms | 196 / 281 ms | Both shell out to `git`; Rust is about 1.5x faster at p50. |

Neither is on a latency-critical path (the projection runs once per timeline append batch, about 0.1 ms per 300 rows).

## Caveats

- Single machine, single OS, dev-mode TS. No Linux/Windows numbers.
- Echo latency is measured at the manager API, not browser keydown to xterm commit (`docs/terminal-performance.md`
  browser specs were not run).
- The existing `scripts/benchmark-terminal-latency.ts` (isolated daemon, agent load levels L0..L3) was not run; `bench/terminal-bench.mts`
  isolates the worker instead so the Node/Rust difference is attributable.
- The frame-size benchmark measures client-to-daemon sized frames only; daemon-to-client large frames were not measured.
