# Golden traffic capture (ACP)

- `tee-acp.mjs`: stdio tee used as the provider launch command. Spawns the real agent (`PASEO_TEE_REAL`, default `copilot`) with the same argv/env and logs every JSON-RPC line to `$PASEO_TEE_LOG` as JSONL (`{dir:"in"|"out", line}`; env values are never recorded). The file is truncated when the process starts, so use one log per spawned process.
- `run-scenarios.mjs [n...]`: drives the TS `CopilotACPAgentClient` (`packages/server/src/server/agent/providers/acp-agent.ts`) through the tee and writes `../../fixtures/NN-*.jsonl`, scrubbed by `scrub.mjs`. Run from `packages/server` with the repo's tsx:

  ```
  cd $PASEO_REPO/packages/server
  CAPTURE_SCRATCH=<tmp dir> ../../node_modules/.bin/tsx <this dir>/run-scenarios.mjs [1..9]
  ```

  Env: `PASEO_REPO`, `CAPTURE_SCRATCH`, `ACP_MODEL` (default `gpt-4.1`). Scenario 5 (resume) needs scenario 1 in the same run.
- Scenarios: 1 prompt+reply, 2 permission allow, 3 permission deny, 4 interrupt, 5 resume (`session/load`), 6 model/mode switch, 7 allow-all (rejected by Copilot 1.0.11), 8 catalog probe, 9 commands.

Notes: Copilot answers a cancelled prompt with `end_turn` (fixture 04). Scenario 8 and 9 are separate files because each spawned process truncates the log. Other agents: point `PASEO_TEE_REAL` at the binary and swap the client class; Cursor needs `agent login` first.
