# P4 Core Spec

Audience: implementer agents. Contract style: files, signatures, acceptance checks. Source of truth for behavior is the TS daemon (`packages/server/src/server`); this doc names only what is not derivable from reading it quickly. Paths below are relative to `packages/server/src/server` unless prefixed.

## 1. Scope and non-goals

In scope (P4): the Rust "core" that replaces TS `AgentManager`, `AgentStorage`, project/workspace registries, directory-sync, owned-subscription delivery, agent-updates, stream coalescer, timeline store/projection/paging, Paseo tool catalog + `/mcp/agents`, authorization tables, Hub relationship + executions, orchestration-skills installer, and the provider adapter seam (mapping to timeline/`ToolCallDetail`).

Non-goals:
- No new persisted fields, no schema changes, no timeline persistence (provider history stays the transcript authority; `durableTimelineStore` is unwired in TS and is not ported).
- No timeline eviction (would make `gap` reachable; needs a protocol decision).
- No per-principal grants/pairing/credentials beyond what TS implements (everyone direct = `owner`, Hub = service principal).
- No port of `paseo-tool-call-detail.ts` (client-only presentation).
- No OpenCode v2 native port (stays host-side), no OMP native in P4.
- Plugins, voice, browser tools broker stay TS-resident; reached via `internal.*` RPC.
- Notification routing, git/checkout, files, terminals beyond the existing crates are other domains.

Rules inherited from `docs/plan.md`: single writer per file; dotted `internal.<domain>.<op>.request|response` RPC for TS readers of moved stores; every shim tagged `// COMPAT(name): added in vX, remove after <date>`; protocol stays backward-compatible.

## 2. Crate layout and dependency graph

New crates under `crates/`:

| Crate | Purpose | Deps (internal) |
|---|---|---|
| `paseo-fs-atomic` | `write_file_atomic`, `write_json_file_atomic`, `FsyncPolicy{Off,File,FileAndDir}`, `quarantine_corrupt`, private (0600/0700, symlink-following) variant. Extracted from `paseo-schedule/src/atomic.rs`; schedule re-exports. | none |
| `paseo-agent-types` | Hand-written serde: `ToolCallDetail`, `ToolCallItem`, `AgentTimelineItem`, `AgentStreamEvent` (all 14 variants), permission types, `AgentUsage`, `PersistenceHandle`, `ProviderSubagentInputEvent`; `to_wire()` allow-list/strip/validate. | paseo-protocol |
| `paseo-tool-detail` | Pure ports: tool-call-mapper-utils, detail primitives, per-provider detail parsers, `merge_tool_call_items`, `limit_item_content`, `coerce_tool_call_id`, `build_tool_call_display_model`. | paseo-agent-types |
| `paseo-timeline` | Pure, no tokio/IO: `Row`, `ProjectedRow`, `Projection`, `select_page`, `TimelineState`, `Coalescer` state machine, prompt index, search, `project_agent_message`. | paseo-agent-types, paseo-tool-detail |
| `paseo-store` | `AgentStore`, `JsonRegistry`/`ProjectRegistry`/`WorkspaceRegistry`, label store + transaction, registry bootstrap/backfill, path utils (`are_equivalent_paths`, `project_dir_name_from_cwd`). | paseo-fs-atomic, paseo-agent-types |
| `paseo-directory` | `VersionedCollection<T>`, `DirectorySync`. Pure, sync. | paseo-protocol |
| `paseo-authz` | `Permission` bitset, generated inbound/outbound requirement tables, `SessionAuthz(ArcSwap)`. | paseo-protocol |
| `paseo-provider-api` | Object-safe `AgentClient`/`AgentSession` traits, `EventStream`, shared `run_provider_turn`. | paseo-agent-types |
| `paseo-provider-map` | Per-provider stateful mappers (`RawMapper`), pure/synchronous. | paseo-provider-api, paseo-tool-detail |
| `paseo-providers-native` | Adapters: raw provider crate + mapper + turn/permission controller. | paseo-provider-map, paseo-{pi,acp,opencode,codex,claude} |
| `paseo-core` | `AgentActor`, `AgentManager`, `SessionActor`, delivery, agent-updates, fan-out, `CoreApi`, `ScheduleAgentHost` impl, manager events. | all of the above, paseo-provider-host, paseo-schedule, paseo-git, paseo-files, paseo-file-observer |
| `paseo-tools` | `PaseoTool` trait, `ToolCatalog`, 41 tools over `CoreApi`. | paseo-core (trait only: `CoreApi`), paseo-agent-types |
| `paseo-mcp` | rmcp streamable-HTTP service for `/mcp/agents`. | paseo-tools, paseo-authz |
| `paseo-hub` | `HubRelationshipActor`, `DaemonExecutions`, `HubExecutionController`. | paseo-core, paseo-authz, paseo-fs-atomic |
| `paseo-skills` | `SkillsActor`, transaction recovery, sync manifest. | paseo-fs-atomic |

Existing crates: `paseo-provider-host` is extended (section 11) and becomes the host adapter implementing `paseo-provider-api`. `paseod` gains `Domain::Core` entries and mounts `paseo-mcp`. `paseo-schedule` keeps its crate; `paseo-core` implements its `ScheduleAgentHost`. Dependency direction: leaf pure crates (types, tool-detail, timeline, directory, authz, fs-atomic) -> store/provider-api/map -> core -> tools/mcp/hub -> paseod. `paseo-tools` depends on `paseo-core` only through the `CoreApi` trait, defined in `paseo-core` (or a tiny `paseo-core-api` if a cycle appears).

## 3. State ownership and concurrency model

Actors for anything that interleaves provider events with API calls; locks only for tiny pure state.

- `AgentActor` (one tokio task per live agent, bounded mailbox 1024). Owns: provider `Box<dyn AgentSession>` and its `EventStream`, `TimelineState`, `Coalescer`, live managed state, run/foreground waiters, pending permissions, steer barrier, staged events. Loop `select!` over `{cmd rx, provider events, coalescer deadline}`. Input enum `AgentMsg { SessionEvent | Api(Command, oneshot) | Timer | Lifecycle(Close|Archive|Reload|Unarchive) }`. Sequential processing replaces `enqueueSessionEvent`, `stagedEvents`, `steerEventBarriers`, `lifecycleMutationTails`, `drainSessionEvents`, `foregroundMutationTails`. Staged-while-starting and steer-barrier become explicit sub-states that buffer events until released. `setMode`/`setThinking` drain pending provider events first (drainSessionEvents parity).
- Event pipeline order inside the actor (fixed): provider event -> staging check -> `limit_item_content` -> `attach_turn_identity` -> drop stale terminal -> coalescer -> terminal bookkeeping -> per-type handler -> `record_timeline` (assign seq) -> dispatch. `flush_for(agent)` runs before dispatch of any non-coalescable event.
- `AgentManager`: `DashMap<AgentId, ActorHandle>` registry plus `ManagerEvent` fan-out to per-subscriber bounded mpsc (not lossy broadcast). Overflow closes/resyncs that subscriber, never silently drops. Subscribers: SessionActors, DirectorySync feeder, plugin-hook forwarder, persistence.
- Persistence is a subscriber: on `agent_state` (except lifecycle=closed) call `AgentStore::apply_snapshot`; explicit persists (archive/close/rewind) are awaited through the same store lane.
- `SessionActor` per `(principal, clientId)`: owns delivery, agent-updates, subscription registries, client capability set, 90 s reconnect grace (cancelled on resume). One mpsc of `SessionCmd::{Inbound, SocketAttached, SocketDetached, AgentEvent, ProjectUpdate, Shutdown}`. Long handlers run as sub-tasks reporting back; `ReqCtx.active` flag gives `owner.active=false` semantics.
- `SocketWriter` per physical socket (edge-owned, section 9): bounded queue of `Outbound`, sole enforcement point of ownership `permits()`, stringifies once (`Arc<str>` shared), tracks queued bytes (`AtomicUsize`), closes socket at high-water.
- `JsonRegistry` is an actor owning an `IndexMap` plus a mailbox of mutation closures; reads via `ArcSwap` snapshot. `get_or_create_active_by_root` is a message to the same actor.
- `DirectorySync`: shared `Arc` with `std::sync::Mutex` (never across `.await`). Read-then-respond must be atomic w.r.t. seq (TS got this from synchronous execution).
- `HubRelationshipActor`, `SkillsActor`, `HubExecutionController`: actors with generation/epoch stale-event rejection (section 12).
- AuthZ hot path: one `ArcSwap` load per outbound frame. Never hold a lock across an await into `AgentManager`.
- Time: all timers use `tokio::time::Instant`; tests use `start_paused = true`. Timestamps are `Date.toISOString()` format (`YYYY-MM-DDTHH:MM:SS.mmmZ`, always ms, `Z`). Agent `updatedAt` is monotonic per agent (+1 ms if clock did not advance).

## 4. Persistence

Byte-compatible with existing `$PASEO_HOME` files. Single-writer: once `PASEO_CORE=rust`, TS must not construct `AgentStorage`/`FileBacked*` registries (guard `bootstrap.ts:956-963`); TS callers use `RemoteAgentStorage`/`RemoteWorkspaceRegistry` adapters over `internal.agents.*`, `internal.workspaces.*`, `internal.projects.*`.

Writer format (`paseo-fs-atomic`): `JSON.stringify(v, null, 2)`, no trailing newline, temp `.<base>.<pid>.<ms>.<uuid>.tmp` in same dir, rename, remove temp on error. fsync default Off (parity); revisit once flag is default.

Files owned by P4:

| File | Notes |
|---|---|
| `agents/<projectDirNameFromCwd>/<id>.json` (+ root `agents/*.json` read) | `STORED_AGENT_SCHEMA` (`agent/agent-storage.ts:45-78`) is authoritative, not `docs/data-model.md`. Struct field order = `toStoredAgentRecord` order. `undefined` omitted, `null` kept (use `Nullable<T>{Missing,Null,Value}`). Every record carries `#[serde(flatten)] extra` so unknown keys survive rewrite (deliberate deviation, TS strips). Lenient load: invalid file logged once and skipped. Missing `lastStatus` -> `closed`, `labels` -> `{}`. |
| `projects/projects.json`, `projects/workspaces.json` | Whole-array writes, insertion order preserved. Schemas per `workspace-registry.ts:22-107`; nullish -> `null` normalization; `labels` omitted when missing; `archivedAt` required-nullable. Every upsert/update writes (parity), listeners fire on no-op updates. Project listener errors reject after write; workspace listener errors are logged. |
| `projects/workspace-labels.json`, `projects/workspace-labels.transaction.json` | Prepared/committed compound commit, recovery before registry serves reads, freeze on uncertain outcome (`workspace_label_storage_uncertain`). |
| `agent-requests/<sha256>.json` | Message receipts if send path moves; owned by whichever side owns `send_agent_message*`. |
| `hub-relationship.json` (0600) | Section 12. |
| Skills dirs + `transaction.json` | Section 12. |

Not persisted (must stay volatile): timeline rows/epoch/seq, provider-subagent store, directory-sync generation/seq, subscriptions, pending permissions, retired internal agents. Do not invent files for them.

Behavior rules (port exactly):
- Agent write lane: per-agent FIFO; mutation closure executes inside the lane against latest cache value; `apply_snapshot` preserves existing `archivedAt`, `createdAt`, and title unless override key present. `remove()` sets permanent `deleting`; later queued writes no-op. `flush()` waits all lanes.
- cwd change: write new path, then unlink old. Deviation: duplicate files for one id on load resolve to max `updatedAt`, unlink losers (TS is readdir-order nondeterministic). Document in `docs/data-model.md`.
- `project_dir_name_from_cwd`: win32 root parsing on all platforms, strip trailing separator, separators -> `-`, root-only -> sanitized root or `root`.
- Internal agents (`config.internal`) never touch disk; keep `retired_internal` LRU (500).
- Registry mutation: stage copy -> updater -> write whole array if changed -> swap cache -> afterCommit. Failed write leaves cache unchanged. Post-commit events pushed to one ordered mpsc.
- Corrupt registry (invalid JSON or one bad record): deviation, `quarantine_corrupt` (rename `<name>.corrupt-<ts>`), start empty, log error. TS truncates silently. Needs sign-off (open question Q1).
- Project identity: `get_or_create_active_by_root` picks oldest active record with `are_equivalent_paths` root; refresh kind/projectKey and bump `updatedAt` if differ; archived-only matches are not resurrected. Paths lexical (`path.resolve` equivalent), never realpath; Windows case-fold only for Windows-looking paths. IDs `wks_`/`prj_` + 16 lowercase hex.
- Boot order: attach persistence -> `AgentStore.initialize` -> bootstrap registries (re-materialize from agent records only if projects.json OR workspaces.json missing; workspaceId backfill runs every boot) -> label service initialize/recover -> reconciliation service start -> `reconcile_now` fire-and-forget.
- COMPAT tags to carry: `workspaceIdBackfill` (remove after 2026-12-16), `worktree-branch-identity`, `projectKey`, `autoArchivedChangeRequestUrl`.

## 5. Timeline and coalescing

`paseo-timeline`, golden-tested against TS. Single-threaded `&mut self` API, no interior locking.

- `TimelineItem` = `{type, #[flatten] raw: Map}` with typed accessors; unknown variants round-trip. Identity via `timeline-identity.ts` (tool_call -> callId, plugin -> pluginId/id).
- `TimelineState { epoch: Uuid, projection, min_seq, next_seq }`. `next_seq` starts 1; `min_seq` never advances; empty window `{0,0,1}`. Epoch minted on: init, resume/register, forced hydrate, reload, rewind. Never persisted.
- Seq assigned AFTER coalescing. Row recorded BEFORE `agent_stream{seq,epoch,timestamp}` dispatch, on the actor, so a gap-recovering client can always fetch it.
- Projection invariants: `seq == seqEnd`; tool-call updates with same callId and same turnId merge into the FIRST row (`merge_tool_call_items`: `unknown` detail never overwrites known; metadata shallow-merge; error set on failed, nulled on completed/canceled); different turnId starts a new row and takes the identity index; plugin items replace the item but keep `seqStart`. Text merge only into the LAST row, only if `prev.seqEnd+1 == entry.seqStart`, same turnId, assistant messageId rule (explicit differing id breaks; undefined merges). `sourceSeqRanges` merged contiguous (arithmetic, same result as per-seq loop). Re-projection of projected rows is idempotent.
- Paging: port `selectProjectedTimelinePage` and Tail/After/Before selectors verbatim (backward extension loop, contiguous `endSeq` advance, `after` ordering by first source seq then re-sort by display index). `reset = staleCursor || gap`; gap only for `direction=after` with rows and `cursor.seq < minSeq-1`; when reset force tail but echo requested direction. Session defaults: `direction = msg.direction ?? (cursor ? after : tail)`, `limit` 0 for after, else 200; `canonical` served as projected; `mergeWindow` echoed only when true; `supportsTimelineItem` filter AFTER paging; strip `reasoning_merge` for clients lacking `reasoningMergeEnum`.
- Content limits: 64 KiB by UTF-16 code units on shell output, failed shell `error.content`, `plain_text` detail; suffix `[Content truncated]`; plugin data cap 64 KiB. Applied on record, on dispatch, on hydration. Lone-surrogate case: back off to a code-point boundary (documented divergence, Q4).
- Hydration: `stream_history` collected fully, `project_agent_message` + `limit_item_content` applied, then one actor step replaces `TimelineState` (new epoch if forced) and dispatches `timeline_replacement`. Failure leaves state untouched. Imported agents seed `next_seq = rows.len()+1`. Clients without `timelineReplacementInvalidation` get a reconstructed replay (COMPAT, `session.ts:4785-4815`).
- Submitted prompts: canonical `user_message` once per `clientMessageId`; provider echo only sets internal `providerMessageId`; rewind of an un-enriched row errors with the TS message.
- Coalescer (pure state machine): `handle(agent, event, now) -> Vec<Flush>`, `next_deadline()`. 60 ms leading-edge + trailing; coalescable = assistant_message, reasoning, tool_call; empty text swallowed; terminal tool status flushes immediately; tool entries merge by callId in place; text concatenates only when buffer-adjacent with same kind/messageId (assistant)/provider/turnId; `flush_and_discard` on close/delete. Never shrink the window for smoothing (client concern).
- Timeline memory unbounded and fetch is O(N); keep semantics first, add copy-on-write `Arc<Vec<ProjectedRow>>` snapshot for fetches later.
- Child timelines (`ProviderSubagentStore`) use the same `TimelineState`, key `parentAgentId\0subagentId`, never persisted.

## 6. Subscriptions and fan-out

- `DirectorySync` (`paseo-directory`): 3 `VersionedCollection`s (projects, workspaces, agents). Per-collection seq from 1; bump only on fingerprint change (canonical serialization of the descriptor, golden-tested so equal TS `JSON.stringify` inputs never bump); tombstones cap 5000 in insertion order, trimming advances `expired_through`; `read_after` falls to snapshot `cursor_expired` if `after < expired_through || after > head`; cursor resolution order: generation mismatch (`generation_changed`, or `no_cursor`) -> missing afterSeq (`no_cursor`) -> `read_after`. Snapshot reads call `replace_all` BEFORE reading (reads mutate). `generation` = uuid per boot; background-including readers see `<generation>:background` in stamps/reads, bare generation for internal `sequence_for`. Project updates always sequenced; agent/workspace updates only when subscriber sent `sync`. `changes_too_large` never produced. Preserve known quirks: removals not filtered, background-flip vanishes without tombstone for non-background readers, duplicate observations re-emit same seq.
- Delivery (replaces ALS + WeakMap proofs): every outbound is `Outbound { to: SourceId, owner: Option<OwnerId>, body: Frame }`; proof travels in the envelope. `Source{modern, active, handshake_pending, cancel, subs, ops}`; new `SourceId` per physical socket. Modern: host-assigned subscription ids, client-supplied id rejected, requestId required to open, tag every emit with `subscriptionId`, deny-by-default (one-shot `server_info` bypass during handshake). Legacy: client-chosen id, one subscription per legacy slot, strip `subscriptionId` per `legacyMessage` except `fs.file.update`/`checkout_diff_update`, `terminals_changed` loses `workspaceId`. Egress gate lives ONLY in the socket writer. `is_reply` ported with the exception tables (`replies.ts:28-48`) and a unit test iterating every `*.request` type in `paseo-protocol` to assert classification. Release: cancel first, then `stop().await`, memoized (`Shared`/`OnceCell`), forget only on `Ok`. `detach`/`close` use join-all and return first error. Binary: modern needs proof via subscription emit or file-reply authorization (`file_explorer_request` only). COMPAT(ownedSubscriptions) branches isolated in `legacy.rs` with tags (remove after 2027-03-09).
- Agent updates (`agent_updates/`): register subscription (`isBootstrapping=true`) BEFORE snapshot; emit response; then `flush_bootstrapped` with strict `<` updatedAt drop (equal replayed), removes always replayed; buffer last-write-wins per agent; provider visibility checked at buffer time and replay (through the owning source's capabilities); non-matching agent produces `remove`; null project produces `remove`; per-agent serialized job chain; `to_agent_payload(agent)` snapshot taken SYNCHRONOUSLY before enqueue; enrichment (title/archivedAt from store) async; errors logged and swallowed; MCP credentials never projected.
- Stream fan-out (`session.ts:1301-1368` behavior): `attention_required` suppressed for modern/selective sockets (separate `agent_attention_required` message); `notification`/`plugin` items gated by capabilities (COMPAT `timelineItemCapabilities`, 2027-03-07); timeline delivery targets = agentIds in each socket's `agent.timeline.set_subscription` set (deduped, sorted). Updates are async via the tail and streams immediate; keep that asymmetry.
- Producers are demand-driven (`refreshObservationProducers`): subscribe to the manager only when demand exists; manager subscribe replays `agent_state` for all non-internal agents.
- Backpressure: no per-frame drop; socket closed when `buffered + frame > cap`. Cap constant conflict (TS 64 MiB vs plan.md 8 MiB): Q2.
- Wire serializer parity (`messages.ts:9-39`): `to_wire` is allow-list (thread_started, turn_*, timeline, permission_*, attention_required), strips unknown keys, drops on validation failure with a loud `warn`. `usage_updated`, `mode_changed`, `model_changed`, `thinking_option_changed`, `provider_subagent` never reach `agent_stream`. `unknown` detail always serializes `input`/`output` as value-or-null (never omitted); `failed` requires non-null error (default `{message:"Tool call failed"}`).

## 7. Tools catalog and MCP (rmcp)

- `paseo-tools`: `trait PaseoTool { fn def() -> ToolDef; async fn call(&self, ctx:&ToolCtx, input: Value, cancel: CancellationToken) -> ToolResult }`; `ToolCatalog{ IndexMap<&'static str, Arc<dyn PaseoTool>> }` (registration order = list order); `ToolCatalogFactory::build(RuntimeCtx{caller_agent_id, policy, enable_voice, voice_only})` built per MCP request and per agent launch. Tools depend on narrow `CoreApi` (agents, storage, workspaces, terminals, schedules, providers, config, voice, browser); TS-resident domains implemented as `CoreApi` adapters over `internal.<domain>.<op>`.
- 41 tools (list per `agent/tools/paseo-tools.ts`): speak, create/list/archive/rename_workspace, workspace scripts (list/start/stop), create_agent, send_agent_prompt, get_agent_status, list_agents, cancel/archive/kill/update_agent, terminals (list/create/kill/capture/send_keys), schedules and heartbeats (create/inspect/list/pause/resume/delete/update/logs/run_once), provider tools (list_providers/models/profiles/inspect_provider), get_agent_activity, set_agent_mode, list_pending_permissions, respond_to_permission, browser tools.
- Fidelity rules: input parse = typed struct with `#[serde(flatten)] extra` (passthrough); tool `inputSchema`/`outputSchema`/descriptions are golden snapshots captured from TS (`toJsonSchemaCompat(strictUnions, pipeStrategy:'input')`), never regenerated from Rust types without review; result text synthesis exact: per array key `<k>_count=N`, `<k>_ids=a,b` when all items have string `id`, blank line, `JSON.stringify(x,null,2)`; unknown tool -> `Paseo tool not found: <name>`; disabled tools absent from list AND execute; `speak` registered only when voice enabled, voiceOnly returns right after speak; browser tools only with broker.
- Behavior to preserve: caller scoping (child cwd = lockedCwd > requested cwd resolved against parent > parent cwd; agent-scoped create_agent makes subagent in caller workspace; top-level without workspaceId makes new local workspace; providerOptions inherited only for same provider; top-level `notifyOnFinish` default false); `send_agent_prompt` order dispatch -> arm finish-notification -> wait run start with timeout -> status, returns on dispatch ack, `stopNotification` on post-arm error.
- COMPAT: `scheduleEveryInput` (remove after 2027-01-17; every -> five-field cron, every+cron rejected, timezone only with cron), `nestedCreateAgentPlacement` (legacy workspace/relationship shapes; remove after 2027-01-17).
- `paseo-mcp`: rmcp streamable-HTTP, stateless (fresh service per request), POST only, GET/DELETE -> 405 JSON-RPC `-32000`; server info `agent-mcp` `2.0.0`; DNS-rebinding protection off (host allowlist upstream); `callerAgentId` from query; auth = no password -> open, else constant-time compare against per-process random capability token (minted by core, never persisted) or valid daemon password; 404 when MCP disabled, 401 unauthorized. Cancel on request abort via `CancellationToken`.
- Injection: core injects the internal `paseo` MCP entry (http, `?callerAgentId=<id>`, Bearer capability token) into launch contexts only when user did not define `paseo`; stripped before persisting configs; stripped from launch config for providers with native tools. Reject user MCP server named `paseo`.
- Native tools (OpenCode bridge, OMP host-tools): core sends tool defs in `session.start` params (gated by `supportsNativePaseoTools` + policy); host invokes via NEW host->core request `tool.call {handle,name,input}`; `$/cancel` forwards abort. Everything else uses the HTTP MCP path pointed at the Rust listener.
- `/mcp/agents` moves from reverse proxy to in-process rmcp only together with the agent manager.

## 8. Authorization

`paseo-authz`: 9 permissions (`DAEMON_PERMISSIONS` names), `Requirement{Always, One(P), Any(&[P])}`. Tables generated from the protocol message-type list; a test fails on any unmapped variant (replaces TS `satisfies`); a missing entry denies. Golden: dump TS `INBOUND_PERMISSION`/`OUTBOUND_PERMISSION` to JSON (`packages/server/scripts`) and assert equality.
- Inbound denial: `rpc_error {code:"access_denied", error:"Session is not authorized for <type>"}` only if message has `requestId`, handler not run. Outbound denial silently drops, evaluated per message at emit time. Outbound `status` special case: needs `[workspace.write, hub.execute]` iff status in `{agent_created, agent_create_failed}`, else `daemon.read`.
- `SessionAuthz(ArcSwap<PermSet u16>)`; `replace` is atomic; losing `workspace.write` also releases the browser-host delivery family; `update_principal_permissions` updates pending admissions and attached sessions.
- Direct/relay admission: principal `owner` with all permissions. Hub principal id `hub:<daemonId>:<base64url(sha256(secret))>`.
- COMPAT `semanticHubPermissions`: `legacy_hub_scopes` (only `hub.execution.*` -> `hub.execute`).
- Authz runs inside the core session, not the edge (Hub sessions bypass the edge).

## 9. Edge integration (paseod)

- Add `Domain::Core` to `OwnedTypes` (`wip/paseod/src/routing.rs`), claimed per slice (section 14): P4a registries/directory/labels/workspace messages; P4b agents/timeline/subscriptions/lifecycle/provider requests; P4c `agent.skills.*`, `hub.management.*`; unowned types still forwarded as raw bytes to TS.
- Edge keeps lease (45 s/10 s), pong, hello, relay crypto, physical high-water. It exposes `ClientRegistry::register(socket_id, ClientInfo{client_id, capabilities, subscriptions}) -> OutboundSink` writing into the SAME per-socket writer queue used by the proxy pump, so there is exactly one writer. Core evaluates COMPAT capability gating from the replayed `hello`.
- Ownership authority: SessionDelivery + directory + agent-stream move together. TS-resident controllers (checkout, files, voice, terminals) register/emit owned subscriptions via `internal.delivery.begin.request/response` and `internal.delivery.emit` so there is one Source registry.
- Cross-domain ordering within one socket is not guaranteed once two processes write: keep `agent_update`/`agent_stream`/`agent_state` for one agent in one domain; keep `workspace_update` after agent creation and canonical user row before turn liveness (`docs/timeline-sync.md:218-223`) in one process per phase.
- TS stops emitting `agent_*` once P4b flips.
- Hub stays inside TS until the session/websocket layer is Rust (no split brain on `hub-relationship.json`).

## 10. Provider adapter trait and mapping

Trait seam (`paseo-provider-api`, `#[async_trait]`, object-safe):
- `AgentClient`: `provider`, `capabilities`, `create_session`, `resume_session`, `fetch_catalog`, `is_available`, `list_importable_sessions`, `import_session`, `archive_native_session`, `unarchive_native_session`, `shutdown`.
- `AgentSession`: `subscribe() -> EventStream` (single-consumer bounded mpsc with await, never drop), `start_turn`, `steer_active_turn`, `interrupt`, `respond_to_permission -> Option<followUpPrompt>`, `stream_history`, `try_handle_out_of_band`, `set_mode/model/thinking/feature`, `list_commands`, `revert_*`, `describe_persistence`, `get_runtime_info`, `pending_permissions`, `close`.
- `run()` is not a trait method: shared `run_provider_turn` (subscribe before start_turn, buffer until turnId known, filter by turnId, reducers replace / append-or-replace-growing, `turn_failed` rejects, `turn_canceled` resolves).
- Raw crates with lossy broadcast (codex, opencode, cap 4096): replace with per-subscriber bounded mpsc, or treat `Lagged` as fatal session failure (synthesize `turn_failed` + history resync). `paseo-pi` sync std-mpsc wrapped via bridging thread with `blocking_send`.
- Mappers are deterministic state machines (`feed(raw, cx) -> SmallVec<Out>`); history conversion reuses the SAME mapper instance in `MapMode::History` (TS reuses `mapBlocksToTimeline`; caches are order-sensitive).

Manager-side rules mappers rely on: `attach_turn_identity` (only when provider omits turnId; `turn_started` without id gets active foreground id else `autonomous-<uuid>`; history keeps absent ids); text events are deltas not cumulative; tool_call is an upsert stream by callId (callId missing -> drop); failed-turn handling appends `[System Error]` timeline row (deduped against last item) and resolves pending permissions; host death -> exactly one terminal event per active turn plus pending-permission cleanup; `thread_started` refreshes persistence from `describe_persistence`; `usage_updated` sets lastUsage, `turn_completed` merges usage (never clears); `mode/model/thinking_changed` update state and are not dispatched; `user_message` with `clientMessageId` reconciles against submitted prompts. `project_agent_message` ported (COMPAT `legacySystemEnvelope`, `agentMessageToolEnvelope`, remove after 2027-04-08).

JS-semantics ports: `coerce_tool_call_id` = wrapping i32 `(h<<5)-h+code_unit` over `encode_utf16`, abs, base36; 64 KiB caps and emitted-length offsets in UTF-16 units.

Per-provider work items (beyond shared types/tool-detail):

| Provider | Work items |
|---|---|
| Pi (first) | `PiMapper` (tool lifecycle, extensionHost overrides, turn end on `agent_end` unless `willRetry` then `agent_settled`); controller: `interruptingTurn` deferral, `completeTurn`, usage poller; extension marker parser (`pi/agent.ts:1940-2070`) and shipping `pi/extensions/*`; per-session MCP config + extension tmp files; session-file persistence handle (nativeHandle = path); history mapper. COMPAT piSteerFallback, piClearQueueFallback, piAgentSettled, piCumulativeMessageUpdate, piGetStateFallback. |
| ACP (+ wrappers) | `AcpMapper`: merged tool snapshot per toolCallId, kind-switch `mapToolDetail`, user-chunk buffering, `synthesizeCanceledToolCalls`, early-update queue, history replay into persistedHistory; controller: auto-accept, FS/terminal callbacks routed to Rust terminal worker/`paseo-files`; wrapper hooks (copilot, cursor, kimi, kiro, trae, generic); drop expiring COMPATs where floor allows (`deepseek-tui-acp-id`, `copilotAutopilotMode`). |
| OpenCode v1 | `OpenCodeV1Mapper` (materialized parts, hydrated fingerprints, child-session hydration, auto-approve, usage merge); controller: abort cap 2 s (COMPAT opencodeSlowAbort), SSE watchdogs (5 s/30 s, backoff <=5 s), server lease; version probe `opencode --version`; v2 (>= 2.10) stays host-side. Bridge plugin files under `$PASEO_HOME/runtime/opencode` and managed-process records move to core (host owns no `$PASEO_HOME`). |
| Codex | `CodexMapper`: dual-channel dedupe (canonical `item/*` vs legacy `codex/event/*`; per-turn sets), streamed-text suffix logic, assistant boundary `\n\n---\n\n`, sub-agent routing table (32 threads x 128 notifications), `codex-turn-N` ids, `apply_patch` -> unified diff; controller: plan approval synthesis, async questions, followUpPrompt, elicitation resolution, version gates (0.115/0.128/0.143/0.156; rollback removed -> fork). COMPAT codexLegacyCollabAgentToolCall (2027-01-09), codexFastPreference. |
| Claude (last) | `ClaudeMapper`: `TimelineAssembler` (message-id keyed delta/absolute reconciliation, finalized ids), tool_use cache + partial-JSON, ExitPlanMode -> `plan_approval`, task protocol, sidechain tracker (200 entries/160 chars), task state, context usage and compaction; controller: foreground/autonomous turns, steer, interrupt with stale-result suppression, runtime-exit and missing-resume recovery, permission flow. Stays behind the host until SDK<->CLI replay parity passes. COMPAT claudeFable5*. |
| OMP | Out of P4 scope; host only. |
| Plugins | Host only (`plugin-provider.ts`); core accepts `plugin` timeline items via `agent.timeline.append.request`. |

Persistence handle compatibility is bidirectional: Claude `{provider, sessionId, nativeHandle: sessionId, metadata: config minus providerOptions}`, Codex thread id, Pi file path, OpenCode/ACP session id. TS-created agents resume in Rust and back; match `coerceSessionMetadata`.

## 11. Bridge (`paseo-provider-host`) changes

- TS side does not exist: write `packages/server/src/provider-host/main.ts` implementing exactly `crates/paseo-provider-host/RPC.md` over `ProviderRegistry`. Must ship pi extensions and opencode plugins, and must not write `$PASEO_HOME`. `RPC.md` is referenced but not present at `wip/paseo-provider-host/RPC.md`; confirm location before WP.
- `wire.rs`: add `ModeChanged`, `ModelChanged`, `ThinkingOptionChanged`, `AttentionRequired`, `ProviderSubagent` (currently decode to Unknown); decode `Timeline.item` into `AgentTimelineItem` with raw fallback; add `import_session`, `try_handle_out_of_band`, `listCommands`/`listFeatures`/`getCapabilities`; make `stream_history` chunked (frame size); assert per-handle `seq == last+1`, gap = session failure; synthesize `turn_failed{error:"Provider host exited"}` per active handle on host exit; add host->core request frames (`tool.call`, `host.log`).
- `HostAgentClient/HostAgentSession` implement `paseo-provider-api`. `ProviderRegistry{by_id}` selects `native|host` per provider (`PASEO_PROVIDER_<ID>`).
- One `ProviderHostClient` per daemon owned by `paseo-core`.

## 12. Hub and skills

Hub (`paseo-hub`, moves only after session/websocket layer is Rust):
- `HubRelationshipActor` sole owner of `hub-relationship.json` (0600, atomic, symlink-following) and socket. Commands: Connect, UpdatePermissions, Disconnect, Status, Stop; internal events carry `generation` and `enrollment_generation`; stale events no-op. `trait HubRemote` (reqwest 15 s, tokio-tungstenite with Bearer + `x-paseo-daemon-id` + `x-paseo-session-protocol: 1`) with fake; injectable `Clock`/`RetryPolicy`.
- State machine v2: pending -> active -> revoked (401/403/close 4403); any -> disconnecting (persisted before remote revoke) -> file deleted. Startup: any record -> connecting; persisted `disconnecting` DELETED with warning (COMPAT `hubUnilateralDisconnect`, remove after 2027-02-13). Load failure: rename to `hub-relationship.invalid-<ms>-<uuid>.json` (0600), start empty. v1 -> v2 migration (only `hub.execution.*` -> `hub.execute`; COMPAT `semanticHubPermissions`).
- Enrollment: persisted `idempotencyKey`, `daemonId`, `credentialVerifier=base64url(sha256(secret))` sent on each retry; raw secret never sent; response must echo daemonId and same permission SET else retryable. Retry delay `max(1, round(min(30000, 500*2^attempt)*(0.75+rand*0.5)))`, one attempt counter shared by enroll/reconnect. `updatePermissions`: `(current - revoke) + grant` -> remote PATCH -> require response set == local -> persist -> propagate.
- `DaemonExecutions`: create idempotent by `daemonId\0executionId` (in-flight map + persisted `find_by_daemon_execution`, requires workspaceId); validation order as TS (authority -> existing -> authority -> reject `paseo` MCP name -> preapproved server must be in same request's mcpServers -> create -> authority -> require workspaceId); failed-create cleanup runs every step (closeAgent/deleteAgentState, cleanupFailedCreate(createdWorktree only if `created==true`), `AgentStore::remove`) and rethrows the original; control serialized per key, deduped per (key, action), waits for pending create; authority invalidate bumps generation and awaits settled. `AgentStore` needs atomic find-by-(daemon, execution) with create.
- Hub cwd must be absolute and non-blank.

Skills (`paseo-skills`): `SkillsActor` = the serialized queue (even `status` runs recovery first). Transaction format byte-identical (`transaction.json` owner `paseo-skills-transaction`, version 1, phase capturing|converging; per-skill `.paseo-managed-files.json`) so a TS crash is recoverable by Rust and vice versa; ownership proven only by readable `transaction.json`; delete ops need `confirmedRemovals`; save: max 3 attempts, selection write failure -> rollback. Config key `skills.selection` via `ConfigStore` trait (single writer: Rust calls TS over internal RPC or takes the key). Ship bundled `skills/` next to binary or `include_dir` (Q6).

## 13. Authority matrix: who calls whom in the strangler phase

| Need | Mechanism |
|---|---|
| TS reads/writes agent/project/workspace records | `internal.agents.*`, `internal.workspaces.*`, `internal.projects.*` request/response; TS `Remote*` adapters implement existing interfaces |
| TS emits owned subscriptions | `internal.delivery.begin/emit` |
| Core -> TS plugin lifecycle hooks (`agent.archived`, `agent.closed`, `workspace.archived`, stream publish) | notification `internal.plugins.lifecycle.emit` (async, ordering audit required) |
| Core -> TS-resident domains (git, checkout, voice, browser) | `internal.<domain>.<op>.request` via `CoreApi` adapters; in-process once the domain is Rust |
| Core -> provider host | JSONL-RPC + `tool.call` reverse request |

## 14. Migration order: work packages

Each WP lists files (crate paths), pub signatures, acceptance (TS tests/fixtures proving parity). Capture scripts live under `packages/server/scripts/`. Do not run full TS suites locally; run single files.

WP0. Fixtures and capture harness.
- Files: `packages/server/scripts/capture-{timeline,directory,authz,tool-catalog,mapper-vectors,provider-events}-fixtures.ts`; Rust `crates/*/tests/fixtures/`.
- Output: JSON goldens from `InMemoryAgentTimelineStore` (many (direction, cursor, limit) tuples, incl. tool interleavings, turnId change, messageId change, plugin identity, unknown detail, 64 KiB boundary with astral chars, stale epoch, gap), directory-sync vectors, `INBOUND/OUTBOUND_PERMISSION`, `tools/list` + `tools/call` transcripts, pure mapper (input, output) JSONL, raw-fixture -> `AgentStreamEvent[]` per provider with fake clock/uuid.
- Acceptance: fixtures committed; TS reproduces them deterministically.

WP1. `paseo-fs-atomic`.
- Files: `crates/paseo-fs-atomic/src/lib.rs`; schedule re-exports.
- Sig: `write_file_atomic(path:&Path, bytes:&[u8], policy:FsyncPolicy)->io::Result<()>`; `write_json_file_atomic<T:Serialize>(path, &T, policy)`; `write_private_atomic(path, bytes)`; `quarantine_corrupt(path)->io::Result<PathBuf>`.
- Acceptance: byte-equal output vs TS `writeJsonFileAtomic`; existing `paseo-schedule` tests unchanged; temp cleaned on failure; crash test (kill between write and rename leaves no truncated target).

WP2. `paseo-agent-types` + `paseo-tool-detail`.
- Files: `paseo-agent-types/src/{tool_call,timeline_item,stream_event,permission,usage,wire}.rs`; `paseo-tool-detail/src/{status,gutter,shell_output,detail/*,merge,limits,display}.rs`.
- Sig: `to_wire(&AgentStreamEvent)->Option<WireEvent>`; `merge_tool_call_items(&ToolCallItem,&ToolCallItem)->ToolCallItem`; `limit_item_content(AgentTimelineItem)->AgentTimelineItem`; `coerce_tool_call_id(&Value)->String`; `normalize_tool_call_status(..)`; `build_tool_call_display_model(..)`.
- Acceptance: byte-equal JSON (preserve_order) against WP0 mapper vectors and `agent-stream-coalescer.test.ts`/`timeline-projection` unit tables; `to_wire` output validates against `paseo-protocol` fixtures (`agent_mock.jsonl`); unknown-detail null rule and failed-error rule tested; UTF-16 hash/limit tests.

WP3. `paseo-timeline`.
- Files: `paseo-timeline/src/{model,projection,paging,store,limits,coalescer,prompt_index,search}.rs`.
- Sig: `TimelineState::{append(Row)->Seq, fetch(FetchOptions)->FetchResult, enrich_submitted_user_message, initialize(seed)}`; `Projection::append`; `select_page(..)->PageSelection`; `Coalescer::{handle(agent,event,now)->Vec<Flush>, next_deadline()->Option<Instant>, flush_for, flush_and_discard}`.
- Acceptance: exact JSON equality with WP0 timeline goldens; proptest incremental == batch and re-projection idempotent; coalescer cases mirror `agent-stream-coalescer.test.ts`; `timeline-projection` and `agent-timeline-store` TS test cases ported; golden `bursty-stream` trace replay (timestamps ignored).

WP4. `paseo-store` (P4a part 1).
- Files: `paseo-store/src/{agent_store,agent_record,registry,project_registry,workspace_registry,labels,bootstrap,paths}.rs`.
- Sig: `AgentStore::{initialize, list, get, list_by_workspace, find_by_daemon_execution, apply_snapshot, set_title, remove, flush}`; `ProjectRegistry::get_or_create_active_by_root`; `WorkspaceRegistry::{upsert, update, mutate_many, subscribe}`; `commit_workspace_label_mutation(stage, hooks)`; `bootstrap_registries(..)`; `project_dir_name_from_cwd(&str)->String`; `are_equivalent_paths(&str,&str)->bool`.
- Acceptance: every real `$PASEO_HOME/agents/**`, `projects/*` file round-trips to identical JSON value and key order; ports of `agent-storage`, `workspace-registry`, `workspace-registry-bootstrap`, `workspace-labels`, `backfill-workspace-id.migration`, `utils/path` TS tests; crash tests (cwd-change duplicate, label prepared/committed recovery, corrupt registry quarantine); dir name vectors (`C:\`, UNC, trailing slash).

WP5. `paseo-directory` (P4a part 2).
- Files: `paseo-directory/src/{versioned_collection,lib}.rs`.
- Sig: `DirectorySync::{synchronize_{projects,workspaces,agents}(snapshot,cursor,include_background), sequence_{project,workspace,agent}_update(..)}`.
- Acceptance: `directory-sync/index.test.ts` and `versioned-collection.test.ts` vectors (including repeated observation same seq, tombstone cap, `cursor_expired`, `generation_changed`, `:background` generation); fingerprint golden.

WP6. `paseo-authz`.
- Files: `paseo-authz/{build.rs,src/lib.rs}`.
- Sig: `required_inbound(&str)->Requirement`; `required_outbound(&OutboundRef)->Requirement`; `SessionAuthz::{allows_inbound, allows_outbound, replace}`; `legacy_hub_scopes`.
- Acceptance: table equality with WP0 dump; exhaustiveness test over every protocol message type; `authorization` TS tests ported; outbound status special case.

WP7. `paseo-provider-api` + `paseo-provider-host` extension + TS `provider-host/main.ts`.
- Files: `paseo-provider-api/src/lib.rs`; `paseo-provider-host/src/{wire,client,session,adapter}.rs`; `packages/server/src/provider-host/{main,protocol}.ts`.
- Sig: traits per section 10; `HostAgentClient::new(ProviderHostClient)->impl AgentClient`.
- Acceptance: `RPC.md` contract tests both sides; `session.event` seq-gap test; host-kill yields one terminal event per active turn; `*.real.e2e.test.ts` per host-backed provider pass against a Rust harness; `tool.call` round trip with OpenCode bridge.

WP8. `paseo-core` agent manager (P4b part 1).
- Files: `paseo-core/src/{agent_actor,agent_manager,hydrate,lifecycle,persistence_sub,run,permissions,internal_agents}.rs`.
- Sig: `AgentManager::{create_agent, resume_agent, ensure_agent_loaded, send_message, cancel, archive, unarchive, delete, fetch_timeline, rewind, subscribe(replay_state)}`; `trait ProviderBridge`; `MockSession` for tests; implements `ScheduleAgentHost`.
- Acceptance: ported cases from `agent-manager.test.ts` (ordering, staged events, steer barrier, archive cascade, submitted-prompt reconcile, hydration all-or-nothing, internal agents not persisted); `paseo-schedule` host tests; agent record parity file diffs against TS; Rust vs TS daemon run on copies of same home with scripted client diff timeline pages.

WP9. `paseo-core` session layer (P4b part 2): delivery, agent-updates, fan-out, edge.
- Files: `paseo-core/src/{session_actor,delivery/*,agent_updates/*,fanout/*}.rs`; `wip/paseod/src/{routing,client_registry}.rs`.
- Sig: `Delivery::{begin(ctx,family,id,stop), release, detach, close, is_reply}`; `AgentUpdates::{begin_subscription, flush_bootstrapped, forward_live_agent}`; `ClientRegistry::register`; `internal.delivery.*` RPC.
- Acceptance: `owned-subscriptions`, `agent-updates-service` TS tests ported (source retention, close with paused request, failed teardown, bootstrap replay `=`, MCP non-leak); golden JSONL for fetch_agents sync snapshot -> changes -> `cursor_expired` -> `generation_changed`; legacy vs modern client on the same event stream; `daemon-client.e2e.test.ts` and `client-activity.e2e.test.ts` through the edge; app ordering assumptions checked.

WP10. Native providers, in order: Pi, ACP, OpenCode v1, Codex, Claude (P5-aligned; each flips behind `PASEO_PROVIDER_<ID>=native`).
- Files: `paseo-provider-map/src/<provider>.rs`, `paseo-providers-native/src/<provider>.rs`.
- Sig: `ProviderMapper::feed`, `NativeAgentClient` per provider.
- Acceptance per provider: raw-fixture -> `AgentStreamEvent[]` goldens (WP0) match, including history conversion; provider `*.real.e2e.test.ts` passes; TS-created agent resumes in Rust and reverse.

WP11. `paseo-tools` + `paseo-mcp`.
- Files: `paseo-tools/src/{catalog,context,serialization,tools/*}.rs`; `paseo-mcp/src/lib.rs`.
- Sig: `ToolCatalogFactory::build(RuntimeCtx)->ToolCatalog`; `ToolCatalog::{tools, get_tool, execute_tool}`; `serialize_tool_input_parameters(&ToolDef)->Value`; `mcp_router(core, token)->axum::Router`.
- Acceptance: `tools/list` golden byte-equal; `tools/call` transcripts incl. synthesized text; `mcp-server.test.ts`, `mcp-parity.e2e.test.ts`, `agent-mcp.e2e.test.ts`; auth cases (token, password, 401, 404, 405); COMPAT shim tests.

WP12. `paseo-skills`.
- Files: `paseo-skills/src/{actor,transaction,sync,selection,paths,operations}.rs`.
- Sig: `SkillsHandle::{status, save_selection, reconcile, uninstall, import_legacy_selection}`.
- Acceptance: TS skills tests (~3.5k LOC) ported; TS-written crashed `transaction.json` (capturing/converging) recovered by Rust and reverse; `agent.skills.*` wire responses equal TS.

WP13. `paseo-hub` (last; requires WP9 done).
- Files: `paseo-hub/src/{actor,remote,record,executions,controller}.rs`.
- Sig: `HubHandle::{connect, update_permissions, disconnect, status}`; `DaemonExecutions::{create, control, validate, subscribe, invalidate_authority}`.
- Acceptance: `relationship-controller.test.ts` (37 KB) with fake clock/remote/retry, `relationship-remote`, `daemon-executions`, `execution-controller`, `execution-session.websocket` tests; `hub-relationship.json` fixtures for every state, v1, invalid; kill -9 between persist steps; failed-create cleanup order test.

WP14. Cutover and docs.
- Flip `PASEO_CORE=rust` default behind flag, remove TS construction of moved stores, update `docs/data-model.md` (schema drift, duplicate-file rule, quarantine, unknown-key preservation), `docs/architecture.md`; audit plugin lifecycle consumers for async ordering.
- Acceptance: cross-implementation diff run (same home, scripted client, directory snapshots and timeline pages), all WP acceptance suites green in CI.

Dependencies: WP0 first; WP1/2/5/6 parallel; WP3 after WP2; WP4 after WP1/2; WP7 after WP2; WP8 after WP3/4/7; WP9 after WP5/6/8; WP10 after WP7 (parallel with WP8-9, flip after WP9); WP11 after WP8; WP12 any time after WP1 (route via edge; single-owner flag); WP13 after WP9 and WP11; WP14 last. P4a (WP4+5, registries/directory/labels) ships strictly before P4b (WP8+9).

## 15. Risks

1. Cross-domain ordering on one socket during P4a/P4b; mitigate by keeping message families in one process per phase.
2. Ownership/proof model: over-delivery leaks another client's subscription traffic, under-delivery strands modern clients. Needs legacy-capability e2e fixtures (about 12 COMPAT sites).
3. Single-writer cutover: every TS caller of the moved stores must use the remote adapter in the same release.
4. Timeline determinism: projection edge cases are implicit and subtle; port line by line and fuzz against TS; coalescer tests need fake clock; live runs assert invariants (`seqEnd` anchor, `sourceSeqRanges`) not text splits.
5. Wire silent drops: Rust omitting `null` fields or null `failed.error` invalidates events with no error; mitigate with `to_wire` validation + warn.
6. Stateful mappers shared live/history; Claude alone ~7k LOC; port as state machines with replay goldens.
7. Provider host spec unimplemented, no reverse channel in RPC.md, `RPC.md` location unverified, five event types decode as Unknown today.
8. Lossy/unbounded channels in raw crates vs TS lossless synchronous delivery.
9. Tool schema drift (models depend on exact descriptions/schemas/text); authz table drift; both need golden/exhaustiveness tests, never default-allow.
10. Hub coupling to TS session internals; callback staleness in naive multi-threaded port.
11. Skills dirs and `config.json` multiple writers if both runtimes run; enforce one owner via flag.
12. Plugin hooks become async notifications; plugins assuming synchronous ordering relative to persisted state may change behavior.
13. Whole-array registry writes scale with record count; unbounded timeline memory and O(N) fetch.
14. Windows path semantics (`std::path` differs); reimplement lexically, port `utils/path.test.ts` vectors.
15. Rolling back to TS after Rust added unknown keys: TS strips them; freeze schema until after cutover.
16. Tool coverage gap: only about 12 of 41 tools were reviewed in depth; per-tool review during WP11.
17. Packaging: bundled `skills/`, pi extensions, opencode plugins must ship with the right process.

## 16. Open questions

- Q1. Corrupt registry policy: approve quarantine-and-start-empty (deviation) vs TS truncate parity.
- Q2. Socket high-water: 64 MiB (TS code) vs 8 MiB (plan.md). Lowering may close legitimate slow mobile clients.
- Q3. Unknown-key preservation on agent/registry records: approve (deviation) or strip for parity.
- Q4. Lone surrogate after UTF-16 truncation: back off to code-point boundary vs replace with U+FFFD.
- Q5. Fsync policy default once flag is default (registries and agent records).
- Q6. Packaging of bundled skills, pi extensions, opencode plugins (embed vs next to binary).
- Q7. Does the core own `usage_updated`-style internal events from day one, and who owns provider snapshot/catalog caching (core vs host)?
- Q8. Is OMP in P4/P5 scope; keep host-only?
- Q9. Should durable timeline be implemented at all (unwired in TS)? Current answer: no.
- Q10. Does `config.json` skills key stay TS-owned (internal RPC) or move to Rust at WP12?
- Q11. Add streaming `stream_history` before WP8 acceptance, or accept single-frame arrays for now?
- Q12. Background-flipped entity without tombstone, unfiltered removals: reproduce or fix behind a flag?
- Q13. Plugin lifecycle consumers: audit result needed before WP14.
- Q14. Does the app depend on cross-domain frame ordering beyond what `timeline-sync.md:218-223` lists?
