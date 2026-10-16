# Golden traffic capture

- `tee-claude.mjs`: executable passed as `pathToClaudeCodeExecutable`. Spawns the real `claude` (override with `PASEO_TEE_REAL_CLAUDE`), logs to `$PASEO_TEE_LOG` as JSONL: header `{argv, env_keys}`, then `{dir:"in"|"out", line}` per stdin/stdout line, then `{exit}`. Env values are never recorded.
- `run-scenarios.mjs [n...]`: runs 10 scenarios through the TS SDK (haiku) and writes `../../fixtures/NN-*.jsonl`, then scrubs them (`scrub.mjs`). Set `CAPTURE_SCRATCH` to the temp parent dir. Scenario 8 (resume) needs scenario 1 in the same run.
- Scenario 6 note: after `interrupt()` the SDK iterator throws on the error result; the fixture still contains the full traffic.
- Scenario 10 note: requires `enableFileCheckpointing` plus `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING=1`, and a client-supplied `uuid` on the user message to rewind to.
