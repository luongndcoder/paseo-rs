# P4 golden fixtures (WP0)

Captured from the TS daemon by the scripts in
`packages/server/scripts/capture-*-fixtures.ts` (branch `feat/rust-p4-fixtures` of the TS repo,
worktree `.claude/worktrees/p4-fixtures`). Everything here is TS output; Rust tests copy or point at
these files and assert equality. Do not edit by hand.

Regenerate (from `packages/server` in the TS worktree):

```bash
export P4_CRATES_DIR=<paseo-rs>/crates      # input transcripts for mapper + provider-events
export LC_ALL=en_US.UTF-8                   # directory capture refuses non-English default locales
for s in timeline directory authz tool-catalog mapper-vectors provider-events; do
  ../../node_modules/.bin/tsx scripts/capture-$s-fixtures.ts --out <paseo-rs>/fixtures/p4
done
```

Determinism: no wall clock (Date and `crypto.randomUUID` are replaced with a counter clock and a
counter uuid where adapters run), seeded PRNG for generated traces, fixed epochs and timestamps.
Two full runs, and four concurrent runs of the adapter replays under CPU load, are byte-identical.

| Dir | Files | Consumed by |
|---|---|---|
| `timeline/` | `store-timelines.json`, `store-fetch-cases.jsonl`, `projection-cases.jsonl`, `tool-merge-cases.json`, `content-limits.json`, `prompt-index.json`, `coalescer-traces.json` | WP2 (merge, limits), WP3 (store, projection, paging, coalescer, prompt index) |
| `directory/` | `service-scenarios.json`, `versioned-collection-scenarios.json`, `fingerprint-cases.json`, `locale-compare.json` | WP5 |
| `authz/` | `permissions.json`, `allow-matrix.json`, `behavior.json` | WP6 |
| `tool-catalog/` | `tools-list.json`, `tools-call-transcripts.json`, `structured-content-text.json` | WP11 |
| `mapper-vectors/` | `*.jsonl` per group plus `README.json` | WP2, WP10 |
| `provider-events/` | `<provider>/<fixture>.json` plus `manifest.json` | WP10 |
| `INVESTIGATIONS.md` | Q13 plugin lifecycle audit, Q14 app ordering audit, Q2 check, corrections to the spec found while capturing | WP8, WP9, WP14 |

## File formats

- `timeline/store-timelines.json`: per timeline `seed` (passed to `initialize`), ordered `steps`
  (append with `timestamp`, `turnId`, `providerMessageId`, plus `returnedRow`), `enrich` results, and
  `expected` store state after all steps (`window`, `items`, `projectedRows`, `lastItem`,
  `lastAssistantMessage`, `submittedUserMessages`). Epoch is `epoch-1`, agent id `agent-1`.
- `timeline/store-fetch-cases.jsonl`: `{timeline, options, result}`. `options.cursor.epoch` is
  `epoch-1` (live) or `epoch-stale`. Covers default/tail/before/after, limits `undefined|0|1|2|5`,
  every source seq as cursor on small timelines, gap, stale epoch and reset.
- `timeline/projection-cases.jsonl`: `{name, rows, canonical, projected, reprojected, windows, pages,
  itemSelections}` for `projectTimelineRows`, `selectTimelineWindowByProjectedLimit`,
  `selectProjectedTimelinePage`, `selectItemsByProjectedLimit`.
- `timeline/content-limits.json`: 64 KiB cases use `{ "$gen": { parts: [{unit,count}] } }` for inputs
  and `{ "$str": { length16, sha256Utf16le, tail16Hex } }` for outputs (sha256 over the UTF-16LE
  bytes, so lone surrogates are preserved).
- `timeline/coalescer-traces.json`: fake clock starting at 0; each op carries `at` (ms) and the
  `handled` boolean; `flushes[].at` is the flush time (trailing flushes at their timer time).
- `directory/*`: scenarios replay on a fresh service/collection; every op carries its TS `result`.
  `locale-compare.json` pins ICU 78.3 `en-US` `localeCompare` order, which decides same-seq order.
- `authz/*`: tables come from the `SessionInbound/OutboundMessageSchema` option lists, so a new
  protocol message that lacks a table entry changes (or breaks) the capture.
- `tool-catalog/tools-list.json`: per catalog configuration, `catalogTools` (title, description,
  input and output JSON schema) and `mcpToolsList` (the MCP `tools/list` response).
- `mapper-vectors/*.jsonl`: `{fn, name, input, output, threw?}`; `input` keys ending `Undefined`
  mark arguments that were `undefined` (JSON has no such value).
- `provider-events/<provider>/<fixture>.json`: `{status, driver, notes, eventCount, events,
  segments?}`. `status` is `ok` (full adapter replay), `partial` (driver covers only the mapping seam,
  see `notes`) or `failed` (nothing to map: catalog/list-only fixture, or OMP which is host-only).

## Counts

| Area | Cases |
|---|---|
| timeline | 17 timelines, 2410 fetch cases, 17 projection cases, 28 tool-merge, 19+6 limit cases, 12 coalescer traces |
| directory | 13 service scenarios, 6 collection scenarios, 18 fingerprint cases, 1369 collation pairs |
| authz | 206 inbound types, 224 outbound types, 14 permission sets, status special case |
| tool-catalog | 6 catalog configurations (62/39/1/1/36/40 tools), 46 tools/call transcripts, 18 text-synthesis vectors |
| mapper-vectors | 991 vectors (669 utils, 97 claude, 29 codex, 22 opencode, 51 pi, 68 omp, 12 agent-messages, 43 wire) |
| provider-events | 58 crate fixtures: 42 replayed, 1496 events (pi 6 ok/2 partial, acp 9 partial, opencode 7 partial, codex 8 ok, claude 6 ok/4 partial); 16 `failed` = nothing to map (catalog/list-only, OMP host-only) |
