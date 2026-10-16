# paseo-terminal-worker JSONL protocol

Transport: the binary reads one JSON object per line from stdin and writes one JSON object per line to stdout.
stderr is logs only (never parsed). Lines are UTF-8, terminated by `\n`, no embedded raw newlines (JSON escapes them).

Messages are exactly the objects of the Node IPC protocol in
`packages/server/src/terminal/terminal-worker-protocol.ts`:

- stdin: `TerminalWorkerRequest` (`createTerminal`, `registerCwdEnv`, `setActivity`, `clearAttention`,
  `killTerminal`, `killTerminalAndWait`, `getTerminalState`, `captureTerminal`, `killAll`, `send`).
- stdout: `TerminalWorkerResponse` (`{type:"response",requestId,ok,result?|error}`) and `TerminalWorkerEvent`
  (`terminalCreated`, `terminalMessage`, `terminalExit`, `terminalTitleChange`, `terminalCommandFinished`,
  `terminalActivityChange`).

## Byte envelope

Any field that is a Buffer/Uint8Array in TS is encoded as `{"$b64":"<standard base64, padded>"}`.
Today no message carries raw bytes: PTY output is a UTF-8 `string` in `terminalMessage.message.data`
(`{type:"output",data,revision}`), exactly as the Node worker sends it (lossy-decoded UTF-8, incomplete trailing
sequences are held back until complete). The `$b64` rule is reserved for future byte fields; the shim should
apply a generic reviver/replacer for it on both directions.

## Environment (set by the shim when spawning the binary)

| Var | Meaning |
| --- | --- |
| `PASEO_SHELL_INTEGRATION_DIR` | Directory containing `zsh/.zshenv` and `zsh/paseo-integration.zsh` (i.e. `src/terminal/shell-integration`). Fallback: `shell-integration/` next to the binary, then `../shell-integration`. |
| `PASEO_CLI` | Optional path to the paseo CLI; used for `PASEO_HOOK_CLI` and PATH prepend (same as TS). |
| `PASEO_TERMINAL_WORKER_LOG` | Optional tracing filter (default `warn`). |

## Semantics notes for the shim

- Request/response correlation is by `requestId`. Responses may arrive out of order relative to requests
  (except `createTerminal`, which is serialized inside the worker).
- Ordering guarantee kept from Node worker: `terminalCreated` event precedes the `createTerminal` response.
- `createTerminal.options` fields: `id, cwd, workspaceId (required), name, title, env, command, args, rows, cols,
  activityToken, activityUrl`. Missing `workspaceId` -> `ok:false, error:"workspaceId is required"`.
- `getTerminalState` result: `null` for unknown terminal, else `TerminalStateSnapshot` + `replayPreamble`.
- `captureTerminal` result for unknown terminal: `{"lines":[],"totalLines":0}`.
- `terminalMessage.message` variants: `output` (coalesced, ~5ms window, leading-edge flush), `snapshot`,
  `snapshotReady`, `titleChange`.
- stdin EOF: all child process groups are killed, then the binary exits 0. The shim should close stdin to stop it;
  if the shim dies, EOF follows automatically.
- Malformed input line: logged to stderr and skipped; if a `requestId` can be recovered, an `ok:false` response is sent.
