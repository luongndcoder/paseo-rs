# paseo-acp

Native Rust client for [Agent Client Protocol](https://agentclientprotocol.com) agents (P5, native providers).
Port target: `packages/server/src/server/agent/providers/{acp-agent,generic-acp-agent,copilot-acp-agent,cursor-acp-agent,kimi-acp-agent,kiro-acp-agent,trae-acp-agent,acp-options}.ts`.
Standalone cargo workspace; deps are `serde`, `serde_json`, `tokio`.

## Layers

| Layer | Modules | TS counterpart |
| --- | --- | --- |
| Wire | `protocol/` | `@agentclientprotocol/sdk` 0.17 types + JSON-RPC framing |
| Transport | `transport`, `tee`, `connection` | `createLoggedNdJsonStream`, `ClientSideConnection` |
| Provider-agnostic surface | `client` (`AcpClient`), `session` (`AcpSession`), `agent`, `events`, `derive`, `permission`, `host` | `ACPAgentClient`, `ACPAgentSession`, `AgentClient`/`AgentSession` |
| Per-agent profiles | `profile` (`AgentProfile` + `ProviderHooks`), `providers/{generic,copilot,cursor,kimi,kiro,trae}` | the `*-acp-agent.ts` subclasses |
| Test support | `replay` (`Fixture`, `ReplayTransport`, `ReplayFactory`) | none |

Surface: `AcpClient::{create_session, resume_session, fetch_catalog, list_features, list_sessions, list_commands, is_available, diagnostic}`;
`AcpSession::{start_turn, run, subscribe, interrupt, respond_to_permission, set_mode, set_model, set_thinking_option, set_feature, list_commands, take_history, describe_persistence, close}`.
Prompts take text and images (`PromptInput::Blocks`). Events are raw (`SessionEvent::Update(SessionNotification)` etc.); timeline and `ToolCallDetail` mapping is P4.

## Official crate vs hand-rolled

Hand-rolled. Evidence: `agent-client-protocol-schema` 1.12.0 (`v1`) parses all 132 captured/synthetic frames, but re-serializing them changes 28:
it injects defaults (`mcpCapabilities`, `sessionCapabilities: {}`, `auth: {}`) and drops `models` from `session/new|load` responses (unstable feature gate), which Copilot and Paseo rely on.
The runtime crate (`agent-client-protocol` 3.x) is a role/builder framework that owns the connection loop; Paseo needs raw ext methods (`cursor/list_available_models`, `_kiro.dev/*`),
the stringified-id workaround, a recording tee, and unknown-variant passthrough. Every extensible union here is `Lenient<T>` (`Known(T) | Unknown(Value)`).
Revisit when the schema crate round-trips unknown fields and unstable features losslessly.

## Fixtures

`fixtures/*.jsonl`, format shared with `paseo-claude`: header, then `{dir:"in"|"out", line}` (`in` = client to agent), then `{exit}`.
Files starting with `{"synthetic": true}` are hand-written (`tools/synthetic/make_synthetic.py`).

| Fixture | Source |
| --- | --- |
| 01-prompt-reply, 02-permission-allow, 03-permission-deny, 04-interrupt, 05-resume, 06-model-mode-switch, 07-allow-all-unsupported, 08-catalog, 09-commands | captured: TS `CopilotACPAgentClient` through `tools/capture/tee-acp.mjs`, Copilot CLI 1.0.11, `gpt-4.1` |
| 10-copilot-allow-all | synthetic (installed Copilot 1.0.11 rejects `allow_all`; fixture 07 is that rejection) |
| 11-kiro-commands, 12-cursor-catalog, 12b-cursor-catalog-old-cli, 13-kimi-catalog, 14-generic-commands, 15-string-numeric-ids, 16-list-sessions, 17-resume-unstable | synthetic, payloads from the TS source and tests |

Cursor is installed but not logged in (`Authentication required`), Kiro/Kimi/Trae are not installed, Gemini is not authenticated; see `tools/capture/README.md`.

## Tests

`cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`

- `tests/replay.rs`: conformance (parse then serialize equals input, for every frame) and replay of each scenario through the Rust client; a deviating frame fails with a diff.
- `tests/flows.rs`: scripted in-memory agent for permissions, auto-accept, host fs/terminal requests, exits, errors.
- `tests/stdio.rs`: real child process (`sh`), tee recording, terminate/kill.
- `tests/units.rs`: ports of the TS unit tests for pure functions.

## Not ported yet

See the end of the task report; short list: tree-kill of agent process groups, `listImportableSessions` history previews, `resolveCreateConfig`, `providerOptions` beyond `supportsMcpServers`/`clientCapabilities`, tool snapshot merging and synthesized cancelled tool calls (P4), OpenCode (not ACP).
