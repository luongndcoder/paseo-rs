# Golden traffic capture

Records the HTTP + SSE traffic between the TS `OpenCodeAgentClient` (legacy 1.x path of
`packages/server/src/server/agent/providers/opencode-agent.ts`) and a real `opencode serve`.

- `tee-opencode.mjs`: launch command for the provider (`command: {mode: "replace", argv: [this file]}`). Called as
  `tee-opencode serve --port N` it starts the real `opencode serve` on a private port, listens on `N` itself and
  forwards everything, appending JSONL to `$PASEO_TEE_LOG`: `req` / `res` pairs and one `sse` record per
  `data:` frame of `/global/event`. Every record carries `proc` (proxy pid) and `t` (ms). Authorization header
  values are never written. Override the real binary with `PASEO_TEE_REAL_OPENCODE`.
- `run-scenarios.mjs [n...]`: drives eight scenarios through the TS adapter and writes `../../fixtures/NN-*.jsonl`,
  then scrubs them (`scrub.mjs`). Run it with tsx from `packages/server` so the TS sources resolve:
  `cd packages/server && CAPTURE_SCRATCH=<tmp parent> npx tsx <this dir>/run-scenarios.mjs`.
  `CAPTURE_MODEL` / `CAPTURE_MODEL_B` pick the two models (defaults are free OpenCode models).
  The temp working directory is scrubbed to `/tmp/paseo-opencode-fixture`.

| # | Scenario |
| - | -------- |
| 1 | prompt + reply |
| 2 | tool permission asked (`bash: ask`), allowed |
| 3 | tool permission asked, denied with a message |
| 4 | interrupt during a long reply |
| 5 | persist, close, resume, `streamHistory`, second prompt |
| 6 | mode `plan`, model switch, mode `build` |
| 7 | catalog (models + modes) and slash-command listing (two server processes) |
| 8 | prompt with an image part |

Scrubbing (`scrub.mjs`) also keeps fixtures small: `GET /provider` keeps connected providers and five models,
`GET /command` keeps eight commands with clipped templates, `GET /agent` clips prompts and permission lists,
repeated `plugin.added` boot events are capped at two per file. Credentials (`key`, `apiKey`, org ids) become
`REDACTED`. One recording quirk remains: the model's own text deltas can contain the temp path split across two
deltas.

Gotchas found while recording:
- Request ids restart per proxy process, and one scenario can start several servers (the TS manager stops the
  current server when its last lease is released). Always key on `(proc, id)`.
- The proxy forces `accept-encoding: identity`; otherwise bodies are gzip and unreadable.
- Port 0 is not "any port" for `opencode serve` 1.x (it falls back to 4096). The proxy picks a free port itself.
