# Rust Daemon Migration Plan (strangler)

Scope: `packages/server` only (~188k LOC non-test TS). App, CLI, relay server, desktop UI stay TS.
Strategy: Rust runs alongside the TS daemon; domains move one at a time; wire protocol unchanged;
the TS daemon remains the reference implementation until cutover.

Hard invariants for every phase:

- Wire protocol is byte-compatible per `docs/protocol-compatibility.md`. No client change is required by any phase.
- Every `$PASEO_HOME` file has exactly one writer process at any time. Ownership moves with the domain.
- Each phase ships behind a flag, default off, until its acceptance checks pass in CI.
- Never touch the daemon on port 6767. Rust dev runs use `.dev/paseo-home`.

## Layout

```
Cargo.toml                       # workspace root
crates/
  paseo-protocol/                # generated wire types + golden conformance tests
  paseo-terminal-worker/         # P1 binary
  paseo-claude/                  # P5 native Claude CLI client
  paseo-codex/                   # P5 native Codex app-server client
  paseo-acp/                     # P5 ACP family client
  paseo-opencode/                # P5 OpenCode HTTP + SSE client
  paseo-pi/                      # P5 Pi / OMP RPC client
  paseo-git/                     # P3 checkout/git domain
  paseo-files/                   # P3 file explorer, suggestions, download/upload
  paseo-file-observer/           # P3 recursive file observer (notify)
  paseo-schedule/                # P3 scheduled agents
  paseo-provider-host/           # P4 client for the TS provider host
  paseo-relay-crypto/            # tweetnacl-compatible box (P2)
  paseod/                        # P2+ edge, later the full daemon
  paseo-speech/                  # speech: dictation, STT/TTS providers, VAD
```

npm scripts: `build:rust` (cargo build --release, copies binaries to `packages/server/dist/bin/<platform>`),
`lint:rust` (fmt --check + clippy -D warnings), `test:rust`. CI: one new job in the existing workflow, matrix macOS/Linux/Windows.

Crates (pin versions in workspace): tokio, axum, tokio-tungstenite, serde/serde_json, typify, portable-pty,
alacritty_terminal (or vt100 — decide in P1 spike), notify, tracing + tracing-appender, crypto_box, rmcp, reqwest, bcrypt.

## P0 — Protocol foundation

Files:
- `packages/protocol/scripts/export-json-schema.mjs` — `z.toJSONSchema()` for each top-level envelope and session message union → `crates/paseo-protocol/schema/*.json` (checked in).
- `crates/paseo-protocol/build.rs` — typify codegen from schema. Unknown/unsupported variants → `serde_json::Value` passthrough, never a hard error.
- `crates/paseo-protocol/tests/golden.rs` — reads `crates/paseo-protocol/fixtures/*.jsonl`.
- `packages/server/scripts/capture-wire-fixtures.ts` — runs the ad-hoc daemon harness (`docs/ad-hoc-daemon-testing.md`), records every inbound/outbound frame to JSONL.

Acceptance:
- Every fixture frame: `parse → serialize` is semantically JSON-equal to input.
- CI check fails when `messages.ts` changes without regenerated schema (`npm run protocol:schema && git diff --exit-code`).

## P1 — Rust terminal worker

Seam: `TerminalWorkerProcess` in `packages/server/src/terminal/worker-terminal-manager.ts` (`forkWorker` injection, line ~94).
Today the worker is a Node `fork()` over IPC (`stdio: [..., "ipc"]`).

Files:
- `packages/server/src/terminal/stdio-terminal-worker.ts` — spawns a binary, speaks JSONL over stdin/stdout, implements `TerminalWorkerProcess`. Same `TerminalWorkerRequest/Response/Event` (`terminal-worker-protocol.ts`).
- Output payloads: base64 inside JSONL first; binary side-channel only if the benchmark requires it.
- `crates/paseo-terminal-worker/` — PTY (portable-pty, ConPTY on Windows), headless emulator, `TerminalStateSnapshot` serializer matching `terminal.ts`, capture/stripAnsi, shell-integration scripts reused from `src/terminal/shell-integration`, activity hook env (`activityToken`/`activityUrl`), output coalescing matching `terminal-output-coalescer.ts`.
- Flag: `PASEO_TERMINAL_WORKER=rust`.

Acceptance:
- `worker-terminal-manager.test.ts`, `terminal-session-controller*.test.ts`, `terminal.posix.test.ts` pass with the flag on (run per file, `--bail=1`).
- Snapshot parity test: same byte script fed to xterm-headless and Rust → identical snapshot JSON.
- `docs/terminal-performance.md` benchmark: latency and RSS no worse than Node worker.

## P2 — Rust edge (front door)

`paseod` owns the listening port; the TS daemon becomes an internal backend.

Responsibilities moved to Rust:
- HTTP listen, static web UI, WS upgrade, `hello` handshake, top-level `ping`/`pong` lease, 64 MiB outbound high-water per physical socket (TS `MAX_PHYSICAL_SOCKET_BUFFERED_BYTES`, `packages/server/src/server/websocket/physical-socket.ts:4`; an earlier revision of this plan said 8 MiB, which was wrong), relay transport (`crates/paseo-relay-crypto`, test vectors cross-checked against `packages/relay` tweetnacl).
- Per physical client socket: open one internal WS to the TS daemon (loopback/UDS + internal token), replay the client's `hello` verbatim (clientId, capabilities). TS sees a normal client.
- Routing: peek only `type` / `session.message.type`. Table `OwnedTypes: HashMap<&str, Domain>`; miss → forward bytes unchanged. Binary frames: route by opcode family.
- `/mcp/agents` and other HTTP routes: reverse-proxy to TS.

Prereq: `docs/refactors/session-decomposition-plan.md` controllers — each controller's owned-type set is the unit that moves to Rust.

Flag: `PASEO_EDGE=rust` (supervisor spawns `paseod`, which spawns the TS worker on an internal port).

Acceptance:
- `daemon-client.e2e.test.ts`, `client-activity.e2e.test.ts` pass through the edge.
- Local added latency p99 < 1 ms on `agent_stream` frames; edge RSS < 30 MB idle.
- Relay: app connects via relay to edge-fronted daemon (QA per `docs/qa.md`).

## P3 — Leaf domains to Rust (one PR each)

Order (low coupling first): file-download/upload (binary frames) → file-explorer + directory suggestions → file-observer (notify; follow `docs/file-observation.md`) → checkout/git (shell out to `git`, same as TS) → workspace-labels → push → schedule.

Per domain:
- Move its owned types into `OwnedTypes`; delete nothing in TS until the Rust path is default for one release.
- Cross-domain reads go over an internal RPC on the edge↔TS link: `internal.<domain>.<op>.request/response` (dotted, per `docs/rpc-namespacing.md`), never shared files.
- Port that domain's tests as Rust tests; keep the TS e2e test green through the edge.

## P4 — Core: agent manager, storage, sessions

Move: `agent-manager.ts`, `agent-storage.ts`, timeline, `directory-sync/`, owned subscriptions, `authorization/`, `hub/`, tool catalog, MCP (rmcp).

Providers stay in TS, behind a new boundary:
- `packages/server/src/provider-host/` — TS process exposing `AgentClient`/session interface from `agent/agent-sdk-types.ts` as internal RPC (start, send, interrupt, permission respond, list models, events stream).
- Rust core is the only caller. Provider host owns no `$PASEO_HOME` files.

Acceptance: full provider e2e set (`*.real.e2e.test.ts`) per provider in CI against Rust core + TS provider host.

## P5 — Native providers (optional per provider)

| Provider            | Rust approach                                   | Effort |
| ------------------- | ----------------------------------------------- | ------ |
| Codex               | JSON-RPC over stdio (port `jsonl-rpc-process`) | M      |
| ACP family          | `agent-client-protocol` crate                   | M      |
| OpenCode            | HTTP + SSE via reqwest                          | M      |
| Pi / OMP            | local RPC process                               | S      |
| Claude              | native `claude` CLI client: stream-json stdio + control protocol (see below) | L |
| Plugins (`plugins/`)| stay in TS provider host (TS modules)          | —      |
| Speech/dictation    | sherpa-onnx Rust bindings + reqwest for cloud  | M      |

Claude native client (`crates/paseo-claude`): spawn `claude --input-format stream-json --output-format stream-json --verbose` and speak the same control protocol the TS Agent SDK uses (the Python Agent SDK is an independent implementation of it). Paseo uses: canUseTool, hooks, mcpServers (Paseo tools served from Rust over the SDK-MCP control channel or `/mcp/agents`), interrupt, setPermissionMode, setModel, rewindFiles, forkSession/resume, supportedCommands/Models, includePartialMessages. Drift control: pin the supported CLI version range; record SDK<->CLI stdio traffic from the TS provider as golden fixtures and replay them against the Rust client in CI; until parity passes, Claude runs through the TS provider host (P4 bridge).

End state: Rust daemon. Node provider host remains only for TS plugins (`plugins/`).

## P6 — Cutover

- Rust supervisor: `paseo.pid` publish/clear semantics from `docs/architecture.md#storage`, log rotation.
- `packages/cli` and `packages/desktop` stop importing `@getpaseo/server/daemon-control`; they launch the binary.
- Distribution: per-platform npm `optionalDependencies` (esbuild pattern), Electron bundles binary, Docker image (`docs/docker.md`), release flow (`docs/release.md`).
- Remove TS domains that have been Rust-default for one release.

## Implementation protocol (Sonnet subagents)

- One phase or one P3 domain per subagent, on its own branch/worktree.
- Each brief: files, interfaces, acceptance checks from this doc. Subagent runs only touched test files, `npm run typecheck`, `npm run lint`, `npm run format`, `cargo clippy`.
- Opus reviews each PR diff against the invariants above before merge.
