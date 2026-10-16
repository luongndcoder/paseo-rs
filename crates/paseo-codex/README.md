# paseo-codex

Native Rust client for the Codex CLI app-server (`codex app-server`, line-delimited JSON-RPC over stdio). Part of the Paseo daemon port (P5, native providers). Standalone cargo workspace; mirrors the shape of the sibling `paseo-claude` crate.

```
transport  (ProcessTransport | replay)        jsonl: \n-only framing
rpc        RpcClient: id matching, timeouts, server requests, exit with stderr tail
protocol   Frame -> Incoming -> ServerNotification / ServerRequest (typed, Value fallback), params
agent      AgentClient / AgentSession traits + raw AgentEvent (provider-agnostic)
client     CodexClient: create / resume, catalog, commands, importable sessions
session    CodexSession: one thread over one app-server process
```

```rust
let client = CodexClient::new(CodexOptions::new());           // or .command("/path/to/codex")
let session = client.create_session(SessionConfig { cwd, ..Default::default() }).await?;
let mut events = session.subscribe();                          // raw events, incl. permission requests
let result = session.run("hello".into(), RunOptions::default()).await?;
let handle = session.describe_persistence();                   // later: client.resume_session(&handle, None)
```

## Behaviour worth knowing

- Events are raw: normalized turn lifecycle (`TurnStarted/Completed/Failed/Canceled`, usage, permissions) plus every notification verbatim as `AgentEvent::Provider`. No timeline or `ToolCallDetail` mapping (that is P4).
- Mode, model and effort changes apply on the next `turn/start`, as in the TS adapter. A turn that is already running keeps its settings.
- A session reconnects lazily if the app-server dies between turns; the dead turn gets `TurnFailed`.
- The crate never answers an approval itself. `PermissionRequested` must be answered through `respond_to_permission`; unanswered requests are cancelled on `close()`.

## Tests

```
cargo test                                   # unit + process transport + golden replay (no codex needed)
cargo test --test live -- --ignored --nocapture   # real codex, spends a few tokens
```

Fixtures are in `fixtures/` (captured with `tools/capture`, see its README). `tests/common/mod.rs` documents how the replay matches client traffic to a recording.

## Gaps against the TS adapter

Not ported yet (each is a separate piece of the P4/P5 work):

- Timeline / tool-call mapping (`tool-call-mapper`, `tool-call-detail-parser`), history replay (`streamHistory`, persisted-history loading), sub-agent thread routing (`provider_subagent` events, child-thread buffering). Sub-agent notifications are forwarded raw and untagged by turn.
- Plan mode approval flow (`CodexPlanApproval`, `preparePlanImplementation`), async questions (`codex/async-questions.ts`), `/goal`, `/compact` and custom-prompt slash commands, `listCodexSkills` file fallback, `~/.codex/prompts` listing.
- Rewind (`revertConversation`: `thread/rollback` / `thread/fork`, `codex/rewind.ts`) and archive / unarchive of native sessions. The wire params exist in `protocol::params`; no session method uses them.
- `outputSchema` normalization (`normalizeCodexOutputSchema`): the schema is sent as given.
- `getUserSavedConfig` probe (removed from current Codex; only `config/read` is used), custom OpenAI-compatible provider config (`buildCodexCustomProviderConfig`), Windows Microsoft Store binary discovery, `CODEX_HOME`-aware env building beyond `CodexOptions::env`.
- Feature list (`buildCodexFeatures`) is not exposed; `CodexSession::set_feature` handles `service_tier` and `plan_mode`.
- Attachments other than inline images and skills (`AgentAttachment` rendered as text).
- Process shutdown: stdin close, then a 2 s grace, then SIGKILL of the process; the TS adapter uses a tree-kill with SIGTERM first.
