# Golden traffic capture

Records what the TS Codex adapter (`packages/server` `dist`) and a real `codex app-server` say to each other.

- `tee-codex.mjs`: used as the provider command override (`command: {mode: "replace", argv: [tee-codex.mjs]}`). `app-server` invocations are run against the real `codex` (override with `PASEO_TEE_REAL_CODEX`) and every stdin/stdout line is appended to `$PASEO_TEE_LOG` as JSONL: a `{argv, env_keys}` header per process, `{dir:"in"|"out", line}` per line, then `{exit}`. Other invocations (`--version`) pass straight through. Env values are never recorded.
- `run-scenarios.mjs [n...]`: drives the adapter through nine scenarios and writes `../../fixtures/NN-*.jsonl`, scrubbed by `scrub.mjs`. Env: `CAPTURE_SCRATCH` (temp parent dir), `CAPTURE_MODEL` (cheap model, e.g. `gpt-6-luna`), `CAPTURE_MODEL_2` (second model for the switch scenario).
- `scrub.mjs [files...]`: rewrites temp paths to `/tmp/paseo-codex-fixture`, `/Users/x` to `/home/user`, drops tokens and emails, and blanks user-specific `config/read` / `skills/list` / MCP-status content. Idempotent; run it again after changing a rule.

| # | Fixture | Scenario |
|---|---------|----------|
| 1 | `01-single-prompt` | prompt and reply |
| 2 | `02-tool-allow` | command approval, allowed (`read-only` mode, escalation requested) |
| 3 | `03-tool-deny` | same, declined |
| 4 | `04-interrupt` | `turn/interrupt` mid-reply |
| 5 | `05-resume` | two app-server processes: create, then resume the thread |
| 6 | `06-mode-model-switch` | catalog process, then a session that switches mode, model and effort between turns |
| 7 | `07-catalog-commands` | catalog only (`model/list`, `config/read`) |
| 8 | `08-image-input` | prompt with an inline image (`localImage`) |
| 9 | `09-importable-sessions` | a session, then `thread/list` for its cwd |
| 10 | `10-list-commands` | hand-built from 01's `initialize` + `skills/list` (the TS client has no `listCommands`, so there is nothing to capture) |

Notes:

- `approval_policy = "untrusted"` is rejected by Codex 0.160 ("no longer supported"); scenarios 2 and 3 use `read-only` mode plus a prompt that asks for escalation instead.
- The TS adapter's `getUserSavedConfig` probe is answered with a JSON-RPC error by current Codex. It stays in the fixtures; the replay skips it (see `tests/common/mod.rs`).
- Captured with codex-cli 0.160.0. Re-capture after a Codex upgrade and re-run `cargo test`: the `golden_frames_round_trip_and_type` allow-list shows which notification methods the crate still passes through untyped.
