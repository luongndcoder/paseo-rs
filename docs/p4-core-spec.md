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

- TS serializes with four independent per-agent mechanisms plus JS single-thread atomicity. Port each separately; one mailbox for everything changes behavior (a lifecycle op must not stall provider events, and cancel/steer must keep consuming them).
  - Event lane (`sessionEventTails`; `enqueueSessionEvent` agent-manager.ts:3889-3953, `drainSessionEvents` :3955). Replaced by the actor mailbox.
  - Lifecycle lane (`lifecycleMutationTails`, `runLifecycleMutation` :2409): resume, reload, close, archive, unarchive, detach, labels, metadata, cascade of children. Keyed by agent id, not by actor existence (resume runs before the agent is live), so it lives in `AgentManager` as `LaneMap<AgentId>`: FIFO of async closures, a previous failure is swallowed, entry removed when its tail is last. It never blocks the actor: a lane step is a sequence of `ask(actor, Cmd)` calls with awaits between, as in TS.
  - Foreground lane (`foregroundMutationTails`, `runForegroundMutation` :2936): only `cancel_agent_run` and steer admission. Held across the provider `interrupt`/`steer_active_turn` await; events keep flowing meanwhile.
  - Registration tracking (`agentRegistrationTasks`, `acceptingAgentRegistrations`, :885, :3683-3693): `prepare_for_shutdown()` flips a flag synchronously; create/resume/reload/import re-check it after every await (`assert_agent_registration_active`); `flush_for_shutdown` joins them (`AtomicBool` + `JoinSet`).
- Every TS block with no `await` is atomic. In Rust each is ONE actor command so nothing interleaves. Known blocks: stream admission (check no run, create pending run, clear `lastError`, :2602-2650); turn accept (:2672-2712: run Started, `activeForegroundTurnId`, `lifecycle=running`, touch, dispatch `turn_started`, record submitted prompt, re-route staged events, `emit_state`); `prepare_agent_for_closure` (:3827); `finalize_foreground_turn` (:2737); `emit_state` (includes attention edge detection); record-then-dispatch of a timeline row; each synchronous stretch of `handle_stream_event`.
- `AgentActor` (one tokio task per live agent, bounded mailbox 1024). Owns: provider session handle and `EventStream`, `TimelineState`, the agent's `Coalescer` slice, managed fields, `RunSlot`, `EventGate`, foreground waiters, pending permissions (+ buffered resolutions, in-flight responses), `finalized_turn_ids`. Loop `select!` over `{cmd rx, provider events, coalescer deadline}`. Provider calls that can block (`start_turn`, `interrupt`, `steer_active_turn`, `set_*`, `close`, `stream_history`, `get_*` refreshes) are spawned as sub-tasks that report back `AgentMsg::Done{op, result}`; the loop never awaits them. Input enum `AgentMsg { SessionEvent | Api(Cmd, oneshot) | Timer | Done }`. The actor terminates on close/archive (TS removes the agent from `agents` in `prepare_agent_for_closure`, so `get_agent` returns none and later calls fail `Unknown agent '<id>'`).
- Run state (`agent-run-state.ts`): per agent ONE slot `RunSlot = None | Foreground{token, start: Pending|Started(turn)|Failed(err), staged, settled} | Autonomous{token, turn: Option, settled}`; `settled` is a `watch<bool>`. `create_pending_run` overwrites. `settle_terminal_run(turn)`: Foreground ignored unless `Started` with the same turn; Autonomous ignored only when both ids are known and differ. `settle_foreground_run(token)` ignores a token mismatch (an old cancel cannot settle a newer run). `finalized_turn_ids` insertion-ordered, evict oldest past 50 (`rememberFinalizedTurn`).
- Event routing at enqueue (:3908-3919), highest priority first: (1) steer barrier present: push to barrier; (2) Foreground run with `start == Pending`: push to `run.staged`; (3) event lane. Barrier release and staged drain re-route each event through the same function (a released event can be staged again by a replacement run). Staged events are dropped when the run settles without reaching Started; rewind relies on this by parking a pending run as a lock (:3283).
- Event pipeline in the actor (`handle_stream_event` :4303-4372, fixed order): `limit_item_content` (timeline only) -> `attach_turn_identity` -> terminal whose turn id is in `finalized_turn_ids`: drop (no dispatch, no waiter notify) -> live only: `touch_updated_at`, `coalescer.handle` (buffered: return, waiters are notified later from the flush callback with the recorded event), else `flush_for(agent)` -> terminal: `apply_active_turn_terminal` -> per-type handler (may clear dispatch/notify flags) -> live terminal: `settle_terminal_run`, and if the event turn is the foreground turn `finalize_foreground_turn` -> live and dispatch flag: `dispatch_stream` with timestamp only (timeline items already recorded+dispatched with seq by the handler; `turn_started` of the foreground turn, `mode_changed`, `model_changed`, `thinking_option_changed` are not dispatched). Foreground waiters are notified after all of this with the RAW provider event (pre-limit, pre-identity), matched on the raw turn id (`dispatchSessionEvent` :3968-4012).
- Turn identity (:519-549, `applyActiveTurnTerminal` :2786): events that already carry a turn id, and history events, are untouched. `turn_started` without id gets `activeForegroundTurnId ?? activeTurnId ?? autonomous-<uuid>`; terminal and timeline events get `activeForegroundTurnId ?? activeTurnId ?? absent`. `isForegroundEvent = activeForegroundTurnId === eventTurnId` where `null !== undefined`: model as `active_fg.is_some() && active_fg == event_turn`, never `Option == Option`. Terminal disposition: history -> stale; no `activeTurnId` -> untracked; differing turn id -> stale; else clear and closed_current. Handlers return early on stale but the event is still dispatched.
- Attention is edge-triggered inside `emit_state` (:4962, :4997-5035) via `previous_statuses: HashMap<AgentId, Lifecycle>` updated on every emit (also for internal agents, which then skip): running -> idle sets `finished`, entering `error` sets `error`; both call `on_attention`. The first pending permission (map empty -> non-empty, non-internal) calls `on_attention("permission")` without setting attention state (:4743). Delegated agents (`isDelegatedAgent`) never call the callback. Mutation happens before the state snapshot is cloned, so the emitted snapshot includes it.
- `AgentManager`: `DashMap<AgentId, ActorHandle>`, `LaneMap`, provider maps, `RetiredInternal`, shutdown flag, plus a `ManagerBus` task that owns the subscriber list and a mirror of the latest `agent_state` per live agent. Actors send `ManagerEvent` to the bus over one mpsc (per-agent order preserved); `subscribe(replay_state)` is a bus command, so replay plus registration is atomic like TS's synchronous `subscribe` (:980). Per-subscriber bounded mpsc; overflow closes/resyncs that subscriber, never silently drops. Subscribers: SessionActors, DirectorySync feeder, plugin-hook forwarder. Filtering as TS `dispatch` (:5187): agent-scoped subscribers match by agent id (provider_subagent by parent id); global subscribers skip internal agents unless `include_internal`.
- Reads (`get_agent`, `list_agents`, `has_in_flight_run`, `get_pending_permissions`): each actor publishes an `Arc<ManagedAgentView>` into an `ArcSwap` after every handled message, so reads need no mailbox round trip. TS reads live-mutated objects, so a read between two emits can see unemitted fields; publishing per message is the closest equivalent.
- Persistence is a bus subscriber for background snapshots, but `emit_state(persist=true)` semantics are kept: fire-and-forget `AgentStore::apply_snapshot` (:5037), errors logged; callers that persisted explicitly use `persist=false`. TS projects `toStoredAgentRecord` when the store lane runs, not at enqueue; Rust projects at enqueue and relies on per-agent FIFO, so the final file is identical and only intermediate writes differ. `apply_snapshot` preserves only `archivedAt` from the existing record (plus createdAt/title rules in section 4). Internal agents never persist. `flush()` = `coalescer.flush_all` then join background tasks.
- `SessionActor` per `(principal, clientId)`: owns delivery, agent-updates, subscription registries, client capability set, 90 s reconnect grace (cancelled on resume). One mpsc of `SessionCmd::{Inbound, SocketAttached, SocketDetached, AgentEvent, ProjectUpdate, Shutdown}`. Long handlers run as sub-tasks reporting back; `ReqCtx.active` flag gives `owner.active=false` semantics.
- `SocketWriter` per physical socket (edge-owned, section 9): bounded queue of `Outbound`, sole enforcement point of ownership `permits()`, stringifies once (`Arc<str>` shared), tracks queued bytes (`AtomicUsize`), closes socket at high-water.
- `JsonRegistry` is an actor owning an `IndexMap` plus a mailbox of mutation closures; reads via `ArcSwap` snapshot. `get_or_create_active_by_root` is a message to the same actor.
- `DirectorySync`: shared `Arc` with `std::sync::Mutex` (never across `.await`). Read-then-respond must be atomic w.r.t. seq (TS got this from synchronous execution).
- `HubRelationshipActor`, `SkillsActor`, `HubExecutionController`: actors with generation/epoch stale-event rejection (section 12).
- AuthZ hot path: one `ArcSwap` load per outbound frame. Never hold a lock across an await into `AgentManager`.
- Time: all timers use `tokio::time::Instant`; tests use `start_paused = true`. Timestamps are `Date.toISOString()` format (`YYYY-MM-DDTHH:MM:SS.mmmZ`, always ms, `Z`). Agent `updatedAt` is monotonic per agent (+1 ms if clock did not advance).

## 4. Persistence

Byte-compatible with existing `$PASEO_HOME` files. Single-writer: once `PASEO_CORE=rust`, TS must not construct `AgentStorage`/`FileBacked*` registries (guard `bootstrap.ts:956-963`); TS callers use `RemoteAgentStorage`/`RemoteWorkspaceRegistry` adapters over `internal.agents.*`, `internal.workspaces.*`, `internal.projects.*`.

Writer format (`paseo-fs-atomic`): `JSON.stringify(v, null, 2)`, no trailing newline, temp `.<base>.<pid>.<ms>.<uuid>.tmp` in same dir, rename, remove temp on error. fsync behavior identical to TS, default Off (decision Q5).

Files owned by P4:

| File | Notes |
|---|---|
| `agents/<projectDirNameFromCwd>/<id>.json` (+ root `agents/*.json` read) | `STORED_AGENT_SCHEMA` (`agent/agent-storage.ts:45-78`) is authoritative, not `docs/data-model.md`. Struct field order = `toStoredAgentRecord` order. `undefined` omitted, `null` kept (use `Nullable<T>{Missing,Null,Value}`). Every record carries `#[serde(flatten)] extra` so unknown keys survive rewrite (decision Q3: round-trip extra fields; TS strips them, both implementations read the same files during the strangler). Lenient load: invalid file logged once and skipped. Missing `lastStatus` -> `closed`, `labels` -> `{}`. |
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
- Internal agents (`config.internal`) never touch disk; keep retired internal agents FIFO, cap 500, TTL 10 min (not LRU).
- Registry mutation: stage copy -> updater -> write whole array if changed -> swap cache -> afterCommit. Failed write leaves cache unchanged. Post-commit events pushed to one ordered mpsc.
- Corrupt registry (invalid JSON or one bad record): same as TS (truncate to empty) plus `tracing::warn!` (decision Q1, `PARITY-QUIRK(corrupt-registry-truncate)`); quarantine is a post-cutover fix.
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
- Content limits (`agent-timeline-content.ts`), 64 KiB in UTF-16 code units: shell `detail.output` and failed-shell `error.content` are cut with `slice(0, 65536)` and NO notice; `plain_text` detail is cut to `65536 - 21` units (the notice `\n\n[Content truncated]` is 21 units, computed as `notice.length`) plus the notice. Plugin item data is not truncated: JSON over 64 KiB (UTF-8 bytes) throws. Applied on record, on dispatch, on hydration. A cut may leave a lone high surrogate; reproduce it (decision Q4, `PARITY-QUIRK(surrogate-truncation)`).
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
- Backpressure: no per-frame drop; socket closed when `buffered + frame > cap`. Cap is 64 MiB (`physical-socket.ts:4`, decision Q2).
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

- TS side does not exist: write `packages/server/src/provider-host/main.ts` implementing exactly `crates/paseo-provider-host/RPC.md` over `ProviderRegistry`. Must ship pi extensions and opencode plugins, and must not write `$PASEO_HOME`. `RPC.md` is at `crates/paseo-provider-host/RPC.md`.
- `wire.rs`: add `ModeChanged`, `ModelChanged`, `ThinkingOptionChanged`, `AttentionRequired`, `ProviderSubagent` (currently decode to Unknown); decode `Timeline.item` into `AgentTimelineItem` with raw fallback; add `import_session`, `try_handle_out_of_band`, `listCommands`/`listFeatures`/`getCapabilities`; make `stream_history` chunked (frame size); assert per-handle `seq == last+1`, gap = session failure; synthesize `turn_failed{error:"Provider host exited"}` per active handle on host exit; add host->core request frames (`tool.call`, `host.log`).
- `HostAgentClient/HostAgentSession` implement `paseo-provider-api`. `ProviderRegistry{by_id}` selects `native|host` per provider (`PASEO_PROVIDER_<ID>`).
- One `ProviderHostClient` per daemon owned by `paseo-core`.
- WP7 as built: `tool.call` is a host -> client request (`{agentId?, tool, arguments}` -> MCP `CallToolResult`), `host.log` a notification; `launchContext.paseoTools` is a `ToolSpec[]` (JSON Schema) the host turns into a proxy `PaseoToolCatalog`; `host.initialize.runtimeDir` is the only directory the host writes (OpenCode bridge plugin) and `host.setToolManifest` sets the OpenCode tool list. `HostAgentClient::new(host, provider)` is one view per provider over the shared host; its `shutdown` is a no-op (core calls `ProviderHostClient::shutdown`). The host client keeps an unbounded per-session queue; the adapter forwards into a bounded `EventStream` (cap 1024) with await.

## 12. Hub and skills

Hub (`paseo-hub`, moves only after session/websocket layer is Rust):
- `HubRelationshipActor` sole owner of `hub-relationship.json` (0600, atomic, symlink-following) and socket. Commands: Connect, UpdatePermissions, Disconnect, Status, Stop; internal events carry `generation` and `enrollment_generation`; stale events no-op. `trait HubRemote` (reqwest 15 s, tokio-tungstenite with Bearer + `x-paseo-daemon-id` + `x-paseo-session-protocol: 1`) with fake; injectable `Clock`/`RetryPolicy`.
- State machine v2: pending -> active -> revoked (401/403/close 4403); any -> disconnecting (persisted before remote revoke) -> file deleted. Startup: any record -> connecting; persisted `disconnecting` DELETED with warning (COMPAT `hubUnilateralDisconnect`, remove after 2027-02-13). Load failure: rename to `hub-relationship.invalid-<ms>-<uuid>.json` (0600), start empty. v1 -> v2 migration (only `hub.execution.*` -> `hub.execute`; COMPAT `semanticHubPermissions`).
- Enrollment: persisted `idempotencyKey`, `daemonId`, `credentialVerifier=base64url(sha256(secret))` sent on each retry; raw secret never sent; response must echo daemonId and same permission SET else retryable. Retry delay `max(1, round(min(30000, 500*2^attempt)*(0.75+rand*0.5)))`, one attempt counter shared by enroll/reconnect. `updatePermissions`: `(current - revoke) + grant` -> remote PATCH -> require response set == local -> persist -> propagate.
- `DaemonExecutions`: create idempotent by `daemonId\0executionId` (in-flight map + persisted `find_by_daemon_execution`, requires workspaceId); validation order as TS (authority -> existing -> authority -> reject `paseo` MCP name -> preapproved server must be in same request's mcpServers -> create -> authority -> require workspaceId); failed-create cleanup runs every step (closeAgent/deleteAgentState, cleanupFailedCreate(createdWorktree only if `created==true`), `AgentStore::remove`) and rethrows the original; control serialized per key, deduped per (key, action), waits for pending create; authority invalidate bumps generation and awaits settled. `AgentStore` needs atomic find-by-(daemon, execution) with create.
- Hub cwd must be absolute and non-blank.

Skills (`paseo-skills`): `SkillsActor` = the serialized queue (even `status` runs recovery first). Transaction format byte-identical (`transaction.json` owner `paseo-skills-transaction`, version 1, phase capturing|converging; per-skill `.paseo-managed-files.json`) so a TS crash is recoverable by Rust and vice versa; ownership proven only by readable `transaction.json`; delete ops need `confirmedRemovals`; save: max 3 attempts, selection write failure -> rollback. Config key `skills.selection` via `ConfigStore` trait (single writer: Rust calls TS over internal RPC or takes the key). Bundled `skills/` ships next to the binary in a resources dir mirroring the TS dist layout (decision Q6).

## 13. Authority matrix: who calls whom in the strangler phase

| Need | Mechanism |
|---|---|
| TS reads/writes agent/project/workspace records | `internal.agents.*`, `internal.workspaces.*`, `internal.projects.*` request/response; TS `Remote*` adapters implement existing interfaces |
| TS emits owned subscriptions | `internal.delivery.begin/emit` |
| Core -> TS plugin lifecycle hooks (`agent.archived`, `agent.closed`, `workspace.archived`, stream publish) | notification `internal.plugins.lifecycle.emit` (async, ordering audit required) |
| Core -> TS-resident domains (git, checkout, voice, browser) | `internal.<domain>.<op>.request` via `CoreApi` adapters; in-process once the domain is Rust |
| Core -> TS plugin `before` hooks (`agent.create`, `agent.session_open`: awaited, may rewrite config/env) | `internal.plugins.lifecycle.before.request/response`, blocking in the create/resume/reload path (Q16) |
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

WP8. `paseo-core` agent manager (P4b part 1). Source: `agent/agent-manager.ts` (5531 LOC), `agent-run-state.ts`, `agent-loading.ts`, `agent-archive.ts`, `provider-subagents/store.ts`, `docs/agent-lifecycle.md`. Port by area, modules under ~250 LOC.
- Files (`crates/paseo-core/src/`):
  - `manager/{mod,lanes,bus,providers,config_prep,importable,retired}.rs`: `AgentManager`, `ManagerOptions`, `ManagerHooks`, `LaneMap`, `ManagerBus`, provider registry (`update_provider_registry`, enabled flags, `require_available_client`), `normalize_config`/`prepare_session_config`/`build_launch_context`, `list_importable_sessions` (90 s per-provider timeout, results merged then ranked), retired internal agents.
  - `agent_actor/{mod,state,run_state,gate,events,turn_identity,permissions,steer,cancel,hydrate,timeline_record,emit}.rs`.
  - `lifecycle/{create,resume,reload,close,archive,detach_labels,loader}.rs`; `loader.rs` ports `agent-loading.ts` (`ensure_agent_loaded`, `ensure_unarchived_agent_loaded`).
  - `provider_subagents.rs` (port of `ProviderSubagentStore`: key `parent\0sub`, sticky fields, list sorted by `createdAt`), `persistence_sub.rs`, `mock.rs` (`MockSession`, `MockClient`, behind `testing` feature).
- Sig (`AgentId` is a GUID, validated on every entry, `validateAgentId`):
  ```
  AgentManager::new(ManagerOptions) -> Arc<Self>
  // lifecycle lane
  resume_agent_from_persistence(handle, overrides, id, ResumeOpts, Option<ResumePurpose>) -> Result<ManagedAgentView>
  reload_agent_session(id, overrides, rehydrate_from_disk) / close_agent(id) / archive_agent(id) -> ArchivedAt
  archive_snapshot(id, archived_at) / unarchive_snapshot(id, updates) -> bool / unarchive_snapshot_by_handle(handle)
  detach_agent(id) / set_labels / update_agent_metadata / wait_for_agent_close(id)
  // not on the lane (registration-tracked)
  create_agent(cfg, Option<id>, CreateAgentOptions) / import_provider_session(ImportInput)
  // runs
  async stream_agent(id, prompt, RunOpts) -> Result<RunStream>   // admission is atomic; turn starts on first poll; Drop of an unpolled stream settles the pending run
  run_agent / replace_agent_run / steer_agent_run -> SteerResult / steer_or_replace_active_turn -> ActiveTurnSteerDispatch
  cancel_agent_run(id) -> {NotRunning|Settled|Refused} / try_run_out_of_band(id, prompt, opts) -> bool
  wait_for_agent_run_start(id, opts) / wait_for_agent_event(id, opts) / respond_to_permission(id, req, resp)
  // state and timeline
  set_agent_mode|model|thinking_option|feature / set_title / notify_agent_state / clear_agent_attention / mark_agent_unread
  fetch_timeline / get_timeline_rows / append_timeline_item / emit_live_timeline_item / hydrate_timeline_from_provider(id, HydrateOpts{force, broadcast: Arc<AtomicBool>|bool, broadcast_timeline}) / rewind(id, message_id, mode)
  // reads and plumbing
  get_agent / list_agents(include_internal) / get_retired_internal_agent / list_retired_internal_agents / get_last_assistant_message / has_in_flight_run
  subscribe(SubscribeOpts{agent_id, include_internal, replay_state}) -> Subscription
  prepare_for_shutdown / flush / flush_for_shutdown
  trait ManagerHooks { async plugin_before_create, plugin_before_session_open, plugin_emit, publish_agent_stream, on_attention, async on_archived, on_workspace_state_may_have_changed, async before_steer_unavailable_fallback, async paseo_tool_catalog }
  ```
  Implements `ScheduleAgentHost`. `ManagerHooks` is the Rust form of `AgentManagerOptions` callbacks; in the strangler the plugin and on_archived hooks are `internal.*` RPCs to TS (`on_archived` -> `scheduleService.completeForAgent`, errors swallowed, bootstrap.ts:1444; `on_attention` -> push-notification broadcast, websocket-server.ts:752; `on_workspace_state_may_have_changed` -> git service refresh, bootstrap.ts:1026).
- Invariants (port exactly, assert in tests). Line refs are `agent-manager.ts` unless stated.
  1. Admission: `stream_agent` fails `Agent <id> already has an active run` if `activeForegroundTurnId` set or any run slot is occupied (:2631). Rewind's lock occupies the slot, so a concurrent prompt fails the same way.
  2. Accept block order (:2672-2712): `turn_started{turnId}` dispatched BEFORE the canonical `user_message` row; the staged provider echo of the same `clientMessageId` supplies `providerMessageId` and is not replayed; the staged `turn_started` with the accepted id is dropped; remaining staged events re-route to the back of the event queue; then `emit_state`. The provider's own start event for the foreground turn is suppressed in `on_stream_turn_started` (:4707).
  3. `start_turn` failure (:2548-2600): stale/closed session errors set `start=Failed`, `pendingReplacement=false`, lifecycle `idle` if no foreground turn, NO `turn_failed` event. Any other error records a rejected prompt row (if `clientMessageId`), feeds a synthetic `turn_failed` (no turn id) through the pipeline, then `finalize_foreground_turn` and settles the run.
  4. `finalize_foreground_turn` (:2737-2779): remember turn id; `activeForegroundTurnId=null`; close active turn; lifecycle = `running` if `pendingReplacement && !lastError`, else `error` if `lastError`, else `idle`; refresh `persistence` from `describe_persistence()` (fallback runtime sessionId) with cwd attached; touch + `emit_state` unless held for replacement.
  5. Lifecycle table: registration ends `initializing -> idle`; accept -> `running`; autonomous `turn_started` (no foreground turn) -> `running` and tracks an Autonomous run (:4725); non-foreground `turn_completed`/`turn_canceled` -> `idle` only when no foreground turn, not `pendingReplacement`; non-foreground `turn_failed` -> `error`. `turn_completed` merges usage into `lastUsage` (never clears) and clears `lastError`; `turn_failed` sets `lastError`.
  6. `turn_failed`/`turn_canceled` (:4630-4705): `turn_failed` appends assistant row `[System Error] <msg>` unless identical to the last row (:4893-4929; message format :4931 base, `code: <code>`, diagnostic if different, joined by blank line; empty base -> `Provider run failed`); both resolve every pending permission with a dispatched `permission_resolved{deny, "Turn failed"|"Interrupted"}`.
  7. Permissions (:3107-3143, :4756-4771): one in-flight response per request id (`A response to this permission request is already being submitted`); provider `permission_resolved` arriving while in flight is buffered and dispatched only after the refreshed `agent_state`; the pending entry is deleted only on success; buffered entry always cleared in `finally`.
  8. Cancel (:3149-3212): run = tracked run, or an Autonomous run created on the spot if lifecycle is `running`; none -> `not_running`. `interrupt` bounded by `interruptSessionMs` (2 s); not acked -> wait 2 s for settle, `settled` or `refused`. Acked -> wait 2 s for settle; on timeout force `turn_canceled{reason:"interrupted"}` through `dispatch_session_event` (bypasses the gate) for a known turn id, or clear a pending foreground run (and drop to `idle` unless replacing), or force an id-less cancel for Autonomous. Then resolve pending permissions as `Interrupted`.
  9. Steer (:2831-2991): `expected = activeForegroundTurnId ?? activeTurnId`; none or no `steer_active_turn` -> `unavailable`/`inactive`. Admission runs in the foreground lane: `drain_session_events`, `flush_for(agent)`, assert `activeTurnId == expected`, install the barrier, await the provider, always release (barrier events re-queued, then drain). Accepted steer records the submitted prompt with `turnId=expected`. `unavailable` falls back only if the same turn is still active (else error `Active turn changed before steering could be delivered`); `steer_or_replace` runs `before_steer_unavailable_fallback`, re-asserts ownership, then replaces.
  10. Replace (:2799-2829, :2953-2975): set `pendingReplacement=true`, lifecycle `running`, touch, emit BEFORE the cancel; cancel refusal -> `AgentRunCancellationError`; `pendingReplacement` cleared on start success, on start failure, and on any thrown error. `wait_for_agent_run_start` ignores `lifecycle==running` while `pendingReplacement` (:2993-3105).
  11. Reload (:1560-1670): cancel in-flight run first; prepare config and launch context and check MCP support BEFORE touching the old session; close old session once (memoized per session) bounded by 10 s (`Timed out closing previous session during refresh`); drain events; terminalize running provider subagents; prepare closure; persist the closed snapshot (failure aborts, no replacement is created); resume (or create when no handle); `register_session` with preserved labels, workspace, owner, timestamps, usage, transient-error-stripped `lastError`, attention, `historyPrimed` (false when `rehydrateFromDisk`, which also wipes the timeline and removes provider children with dispatched removes). Failure after closure emits the closed state with `persist=false`; failure before leaves the live agent in `error`.
  12. Register (:3568-3681): duplicate id error; initial title = existing stored title, else trimmed config title, else initial title; timeline seed = explicit (items/rows/nextSeq) else fresh; startup history is collected BEFORE the agent is published (failure closes the session, nothing registered); order after publish: previous status, history prime or imported rows, `refresh_runtime_info(emit=false)`, persist snapshot with title, emit state (skipped when `publishWhenReady`), `refresh_session_state`, lifecycle `idle`, touch unless `restoring`, persist, emit, and only then `session.subscribe()`. Call `subscribe()` at that point so adapters that create their channel lazily lose pre-subscribe events as in TS (PARITY-QUIRK(pre-subscribe-events-lost)).
  13. Hydration (:4119-4277, agent-loading.ts): resume/import paths set `historyPrimed` from the caller; `create` sets it true; `ensure_agent_loaded` runs `resume` then `hydrate_timeline_from_provider` as a separate step after the agent is already published `idle`. Non-force (`prime`): collect all history first, project each timeline item (`project_agent_message`, dropped when it returns none), delete durable rows, then APPEND rows to the existing `TimelineState` (same epoch, seq continues), apply provider-subagent events, set `historyPrimed=true`; a failed stream leaves `historyPrimed=false` and the timeline untouched. Force: same collection, then `flush_and_discard`, delete + re-initialize `TimelineState` (new epoch), replace provider children, touch, `emit_state`. `broadcast` may be a closure evaluated at the end so a second concurrent loader can upgrade it (`pendingOptions.broadcastTimeline ||=`); port as `Arc<AtomicBool>`. `ensure_agent_loaded` keeps its module-level in-flight map and the double `wait_for_agent_close` barrier (before and after the live lookup).
  14. Archive (:1837-1943, agent-archive.ts): internal agent: build archived record in memory, close runtime, `deleteAgentState`, retire (below), cascade; nothing touches disk. Public agent: `apply_snapshot`; read stored (missing -> error); `mark_record_archived` = upsert archived record (`lastStatus` running/initializing -> idle, attention cleared) + plugin `agent.archived` (only when previously unarchived and not internal) + `notify_agent_state` (touch + emit) + awaited `on_archived` (errors logged, swallowed); `agent.updatedAt = archivedAt`; `close_agent_runtime`; native archive hook (best-effort, errors logged); discard retained state (timeline, tool policy, provider children with dispatched removes); cascade. The final `persist_snapshot(closed)` inside `close_agent_runtime` overwrites `lastStatus=closed` and attention from the live agent and preserves only `archivedAt` (PARITY-QUIRK(archive-closed-snapshot-overwrite)); cover with a TS-vs-Rust file diff for a running agent with attention.
  15. Cascade (:1890-1943): scan stored records for non-archived children labeled `paseo.parent-agent-id == parent`; per child, re-read inside the CHILD's lifecycle lane and re-check archived/label; detach if cross-workspace (both workspace ids defined and different) or any open-tab label true, else live child -> `archive_agent_unlocked`, stored-only child -> `archive_snapshot_unlocked`. Then a second pass archives live internal children (never on disk). The first child failure propagates and later children are skipped (PARITY-QUIRK(cascade-abort-on-first-failure), test :9237). Lane order is parent then child; no cycle because the parent is already archived before the scan.
  16. Close (:1761-1821): concurrent `close_agent` calls share one in-flight future; sequence is drain events, `session.close()` (failure retains the runtime: no closed snapshot is published, the next attempt retries), cancel running provider subagents, `prepare_agent_for_closure` (flush_and_discard coalescer, remove from `agents` and `previous_statuses`, cancel waiters with `turn_canceled{reason}`, clear run), persist closed snapshot (error remembered), `emit_closed_agent` (state + plugin `agent.closed` unless internal), then rethrow the persist error.
  17. Internal agents (`config.internal`): never persisted (`persist_snapshot` returns), hidden from `list_agents` and global subscribers unless opted in, no plugin hooks, no attention callbacks, children archive with the parent. Retired entries: FIFO (insertion order, NOT LRU), 500 cap, 10 min TTL, expired entries pruned on read and on insert (:739-760, :1729-1759).
  18. `touch_updated_at`: `max(now_ms, prev_ms+1)`; applied on every live provider event, even coalescer-buffered ones (:4324). Stored-only updates use `next_stored_updated_at` (:960) with the same rule.
  19. `emit_state` always runs attention detection first, syncs `features` from the session, enqueues the persist unless `persist=false`, then dispatches `agent_state` (:4962). `hasInFlightRun` = lifecycle `running` or foreground turn or any run slot (:967).
  20. `record_timeline`: `limit_item_content`, append, then dispatch with `{seq, epoch, timestamp}` (:4947, :4792). Submitted prompts: canonical `user_message` once per `clientMessageId`; provider echo reconciles via `reconcile_submitted_prompt_echo` and only enriches `providerMessageId` (:4862). A completed shell tool call whose command matches `commandMayHaveChangedExternalState` (`gh pr merge|close|create|edit|comment|review`, `git push`, `git fetch`) calls `on_workspace_state_may_have_changed(cwd)` (:4813, :5516).
  21. Out-of-band prompts (:2473): handler emits straight to `dispatch_stream` (timeline rows recorded with seq, other events broadcast only); failure appends an `[Error] <msg>` assistant row.
  22. Config prep (:5236-5420): cwd resolved to absolute and must be an existing directory unless `purpose=history`; model `""`/`default` -> unset, then provider default (`fetch_catalog{scope:workspace, force:false}`, first default else first, errors ignored); tool-policy preapprovals must reference a server in the same request; `paseo` MCP entry is stripped before storing and injected only into the launch config (or replaced by native `paseoTools` in the launch context when the client supports them); `PASEO_AGENT_ID` and `PASEO_AGENT_CWD` env always set.
  23. Dead code, do not port: `agentsAwaitingInitialSnapshotPersist` guard in `setTitle` (:2125, the set is never filled), `durableTimelineStore` branches (non-goal), `getTimelineRows` durable path.
  24. Subagents (docs/agent-lifecycle.md): parentage is the label `paseo.parent-agent-id`; detach clears it plus every open-tab label via `detached_agent_label_patch`; archive of a parent never touches a child that was detached. Provider-managed children live in `ProviderSubagentStore` (volatile, same `TimelineState`); `child_session` handling and parent stamping for imported child handles are NOT in `agent-manager.ts` (see `import-sessions.ts` and the OMP provider), so WP8 only owns the store, `provider_subagent` dispatch, `deleteParent`, and cancel-running-on-close/reload.
- Acceptance tests to port from `agent-manager.test.ts` (202 cases; line = test start). Group, port the case bodies against `MockSession`, keep names:
  - Timeline/hydration: 671, 700, 759, 831, 4532, 4622, 4663, 4706, 4771, 5750, 5826, 5927, 6028, 6092, 6139, 6243, 10578, 10652, 10719, 10782, 10945, 11364, 11399; plus `agent-manager-stream-coalescing.test.ts` 398-1379 (limits 398-493, window/ordering 514-852, boundaries 898-967, lifecycle/timer safety 996-1273, foreground yield 1300, seq monotonic 1379).
  - Steer/replace/cancel: 882, 898, 980, 1036, 1112, 1182, 1224, 1246, 1261, 1301, 2667, 2694, 2725, 2758, 6643, 6761, 7174, 7269, 8388, 8468, 11029.
  - Run/turn/attention/permissions: 5415, 6455, 6525, 6561, 6925, 7031, 7132, 7324, 7374, 7584, 7630, 8205, 8241, 8274, 9284, 9360, 9437, 9534, 9689, 9801.
  - Lifecycle (reload/close/shutdown/resume): 1650, 1678, 1718, 1757, 1806, 2417, 2462, 2495, 2519, 2547, 3913, 3982, 4079, 5586, 9906, 10068, 10107, 10384, 10447, 10503, 10542, 11581, 11632, 11716.
  - Archive/cascade/detach/unarchive/labels: 5236, 5269, 5329, 5377, 5448, 5497, 5536, 8525, 8575, 8608, 8637, 8684, 8711, 8752, 8785, 8815, 8866, 8902, 8935, 8988, 9042, 9131, 9170, 9237, 10156, 10193, 10223, 10267, 10317.
  - Internal agents/subscribe: 7503, 7766, 7819, 7849, 7887, 7917, 7961, 7997, 8035, 8088, 8150, 8190.
  - Create/config/provider registry (port where the Rust seam owns the behavior): 1855-2291, 2788-3727, 3727-3850, 4153, 4187, 4309, 4452, 4815-5148, 6315-6425, 11183-11298 (importable sessions), 11475-11546 (workspace hook), 11758.
  - Provider-subagent store: `provider-subagents/store.test.ts`; loader: `agent-loading.test.ts`; archive record: `agent-archive.test.ts`; also `paseo-schedule` host tests.
- Acceptance beyond tests: `MockSession` drives every case with `start_paused=true` and a fake clock/uuid; agent record parity file diffs against TS (including the archive quirk in invariant 14); Rust vs TS daemon run on copies of the same home with a scripted client diffing timeline pages; every `// PARITY-QUIRK(<name>)` in this WP is listed in section 17.

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
7. Provider host spec unimplemented, no reverse channel in RPC.md, five event types decode as Unknown today.
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

## 16. Decisions and open questions

Decided by the lead. Principle: PARITY FIRST during the strangler. Rust reproduces TS behavior including quirks; each reproduced quirk is marked in code `// PARITY-QUIRK(<name>): <what TS does>` and listed in section 17.

- Q1 Corrupt registry: same as TS (truncate/start empty, no quarantine) plus a `tracing::warn!`. `PARITY-QUIRK(corrupt-registry-truncate)`. `quarantine_corrupt` stays in `paseo-fs-atomic` unused by registries until cutover fixes.
- Q2 Socket high-water: TS code is the source of truth. Verified `MAX_PHYSICAL_SOCKET_BUFFERED_BYTES = 64 * 1024 * 1024` at `packages/server/src/server/websocket/physical-socket.ts:4` (checked in `physical-socket.ts:94`, `encrypted-relay-socket.ts:62`). `docs/plan.md` said 8 MiB and is fixed. `wip/paseod/src/config.rs:15` already uses 64 MiB.
- Q3 Unknown keys: preserved on agent and registry records (`#[serde(flatten)] extra`), because both implementations read the same files during the strangler. Rolling back to TS strips them (risk 15).
- Q4 Surrogate after UTF-16 truncation: exactly as TS. `slice(0, 65536)` can split a pair and leave a lone high surrogate; `JSON.stringify` emits it as `\udXXX`. Implemented in `paseo-agent-types::js_str`: the lone high surrogate `0xD800+n` is carried inside the normal `String` as the Plane 16 private-use scalar `U+10F800+n` (counted as one UTF-16 unit by `utf16_len`/`utf16_prefix`/`hash_text`), so it also survives inside `serde_json::Value` (failed-shell `error.content`). `to_js_json_string` / `WireEvent::to_json_string` write it as the `\\udXXX` escape; every outbound JSON writer that can see a truncated item must use them instead of `serde_json::to_string`. Reading a lone `\\udXXX` escape back is not supported by `serde_json` (TS-written files never contain one because items are not persisted). `PARITY-QUIRK(surrogate-truncation)`.
- Q5 Fsync: same as TS (`FsyncPolicy::Off` everywhere).
- Q6 Assets (bundled `skills/`, pi extensions, opencode bridge plugin): shipped next to the binary in a resources dir that mirrors the TS dist layout. No `include_dir`.
- Q7 Provider snapshot/catalog caching lives in core; the host exposes raw list calls (`fetch_catalog`, model/mode listing) and does no caching. Internal `usage_updated`-style events stay manager-owned as in TS.
- Q8 OMP is host-only for now.
- Q9 No durable timeline.
- Q10 `skills.selection` stays TS-owned via internal RPC until WP12 flips ownership.
- Q11 `stream_history` stays a single-frame array. `PARITY-QUIRK(stream-history-single-frame)`; hydration already collects everything before applying (invariant 13), so chunking is a later optimization.
- Q12 Directory quirks reproduced: removals unfiltered, background-flip vanishes without a tombstone for non-background readers, duplicate observations re-emit the same seq. `PARITY-QUIRK(directory-removals-unfiltered)`, `(directory-background-flip-no-tombstone)`, `(directory-duplicate-observation-same-seq)`.
- Q13 Plugin lifecycle consumers audit: investigation task inside WP0. Output goes into this section (which hooks are `before` vs `emit`, which assume ordering relative to persisted state). Known so far: `agent.create` and `agent.session_open` are awaited `before` hooks that rewrite config/env (`agent-manager.ts:1296`, `:5351`), the rest are fire-and-forget emits.
- Q14 App dependence on cross-domain frame ordering beyond `docs/timeline-sync.md:218-223`: investigation task inside WP0. Output goes into this section.
- RPC.md now lives at `crates/paseo-provider-host/RPC.md` (verified present).

Still open:
- Q15. Duplicate files for one agent id on load (section 4): resolve to max `updatedAt` and unlink losers (deviation, TS is readdir-order nondeterministic) or reproduce TS. Not covered by the lead's decisions; the current text keeps the deviation.
- Q16. Plugin `before` hooks make create/resume/reload block on a core -> TS round trip (`internal.plugins.lifecycle.before.request/response`). Needs a failure rule (TS: a throwing hook aborts the operation) and a timeout.
- Q17. Reads (`get_agent`) come from a view published after each handled message, not from live mutated state. Confirm no TS caller depends on seeing unemitted mid-await mutations (checked: `wait_for_agent_run_start` and `wait_for_agent_event` use subscribe plus a state read).
- Q18. `stream_agent` is lazy in TS (an unpolled generator leaks its pending run). Rust settles the pending run on `Drop`; confirm this guard is acceptable as the only intentional deviation in WP8.
- Q19. `onAgentArchived` is awaited inside archive (schedule completion). Under TS-resident schedules this is a core -> TS RPC inside the archive path; confirm it stays awaited until `paseo-schedule` is the only caller.

## 17. Post-cutover fixes

Behavior deliberately reproduced during the strangler. Fix after the flag is default and TS no longer reads the same files. Each item corresponds to a `// PARITY-QUIRK(<name>)` marker.

| Marker | What TS does | Intended fix |
|---|---|---|
| `corrupt-registry-truncate` | Invalid JSON or one bad record in a registry file silently truncates to empty | `quarantine_corrupt` (rename `<name>.corrupt-<ts>`), start empty, log error |
| `lone-low-surrogate` | `JSON.parse` keeps a lone low surrogate escape (`\udc00`) in persisted strings; `paseo-store` reads it as U+FFFD (the carrier covers high surrogates only, the only kind an end-of-string slice produces) | Extend the carrier to low surrogates or normalize on write |
| `project-id-collation` | Oldest-active-project tie-break is `localeCompare` on `projectId` (ICU) | None needed: ids are `prj_<hex>` or `remote:...`; Rust compares bytewise, identical for generated ids |
| `surrogate-truncation` | UTF-16 truncation at 64 KiB can leave a lone high surrogate, serialized as `\udXXX` | Back off to a code-point boundary |
| `stream-history-single-frame` | `stream_history` collected as one array, no chunking | Chunked `stream_history` frames |
| `directory-removals-unfiltered` | Removal entries are not filtered by the reader's scope | Filter removals by scope |
| `directory-background-flip-no-tombstone` | An entity flipping to background vanishes for non-background readers without a tombstone | Emit a tombstone |
| `directory-duplicate-observation-same-seq` | A repeated identical observation re-emits the same seq | Suppress the re-emit |
| `waiter-raw-event` | Foreground waiters get the raw provider event (pre-limit, pre-identity), matched on the raw turn id | Deliver the recorded event |
| `pre-subscribe-events-lost` | Events a provider emits before `registerSession` subscribes are not seen | Buffer from session creation |
| `archive-closed-snapshot-overwrite` | The closed snapshot persisted after archive overwrites `lastStatus` and attention, keeping only `archivedAt` | Preserve archived normalization |
| `cascade-abort-on-first-failure` | First failing child aborts the cascade; later children are skipped | Continue and aggregate errors |
| `intersection-path-conflict` | Write/edit input carrying both `path`/`file_path` and a different `filePath` fails the zod intersection, so the call becomes `unknown` detail | Pick one path deterministically |
| `branch-match-undefined-detail` | A matched detail branch whose mapper returns `undefined` (empty command, empty speak text) still ends the union and yields `unknown` detail | None needed |
| `shell-exit-code-null-skipped` | `exitCode ?? exit_code ?? metadata...` skips `null`, so a null exit code is dropped, never emitted | Preserve explicit null |
| `stale-terminal-dispatch` | A stale terminal event is dispatched to clients even though handlers ignore it | Drop stale terminals |
| `retired-internal-fifo` | Retired internal agents evict in insertion order (not LRU) | None needed unless reuse patterns demand it |
| `rewind-staged-events-dropped` | Provider events staged under rewind's pending-run lock are discarded | Replay or explicit contract |
| `falsy-id-dropped` | `turnId`/`providerMessageId` (row build, store append) and an assistant chunk's `messageId` (on merge) use JS truthiness, so an empty string is treated as absent | Treat only `undefined` as absent |
| `run-canceled-unflagged` | `runProviderTurn` resolves a `turn_canceled` turn without marking the result canceled | Flag the result canceled |
| `skills-root-symlink-unchecked` | Sync checks path components below a skill dir for symlinks, never the skill dir itself, so a symlinked skill dir is written through | Reject or resolve the skill dir root |
| `skills-manifest-array-files` | `.paseo-managed-files.json` with `files: []` is accepted (`typeof [] === "object"`) | Require an object |
| `skills-manifest-plain-write` | `transaction.json` is written with a plain write, not atomically; a torn file reads as invalid and is ignored | Atomic write |
| `skills-undo-removes-symlinked-dir` | Rollback of an emptied, symlinked skill dir removes the link itself | Remove contents only |
| `skills-recovery-aborts-on-first-failure` | The first failing interrupted transaction aborts recovery of the rest | Continue and aggregate |
| `codex-deletion-diff-empty-lines` | Codex `contentToDeletionDiff` filters empty lines with `lines.indexOf(l) < lines.length - 1`, so one earlier empty line keeps every empty line, the trailing one included | Drop only the trailing empty element |
| `skills-rollback-error-masks-cause` | A failing rollback replaces the convergence error that triggered it | Report both |

WP4 (`paseo-store`) Rust-only deviations, all intentional: duplicate agent record files for one id resolve to max `updatedAt` and the losers are unlinked on load (Q15, TS is readdir-order nondeterministic; not yet in `docs/data-model.md` of the TS repo); unknown keys on agent/project/workspace/label records survive rewrite (Q3); key order is the stored-schema order, with `archivedAt` before `owner` (TS appends `archivedAt` after `owner` only when `applySnapshot` preserves it, and backfill appends `workspaceId` last); number formatting follows `serde_json`, not `JSON.stringify`, for floats in opaque blobs (`featureValues`, `providerOptions`).

Also queued: unknown-key preservation interacts with TS rollback (risk 15); Q15 duplicate-file rule; the `usage_updated` dispatch through `agent_stream` is stripped only by the wire allow-list.

Known Rust-only deviation: `surrogate-carrier-collision`. Lone surrogates produced by UTF-16 truncation travel as Plane 16 private-use scalars `U+10F800..U+10FBFF` (`paseo-agent-types/src/js_str.rs`). A genuine character in that range (for example from a private-use icon font) is counted as one UTF-16 unit and written as a `\ud8xx` escape, where TS keeps two units and the real character. Fix at cutover by tracking truncation explicitly instead of through a carrier.

Known Rust-only deviations of the Codex mapper (WP10, `paseo-provider-map/src/codex.rs`):
- `codex-turn-id-on-untyped-events`: TS tags every event, `thread_started` and `provider_subagent` included, with the foreground `codex-turn-N` id. The Rust `AgentStreamEvent` has no `turn_id` on those variants, so they carry none; the golden test strips the key from the expected side.
- `codex-deny-message-from-options`: the message of a deny answer is client state that never reaches the wire (only `decision: "decline"` does); `CodexMapper::with_deny_message` supplies it.
- `codex-image-materialize-injected`: TS writes `data` images to a content-hashed temp file inside the mapper; the Rust mapper takes an injected `ImageMaterializer` and renders the TS failure text without one.
- `codex-history-timestamp-iso-only`: `Date.parse` legacy formats are not accepted for history timestamps, only ISO-8601 strings and epoch numbers.

Known deviations and quirks of the Claude mapper (WP10, `paseo-provider-map/src/claude.rs`, goldens `fixtures/p4/provider-events/claude`, all 10 replay including the 4 `partial` ones):
- `claude-turn-id-on-untyped-events` (DEVIATION): TS tags every event it emits, `thread_started` and `mode_changed` included, with the active turn id. The Rust `AgentStreamEvent` has no `turn_id` on those two, so the tag is dropped; the golden test strips it from both sides. The manager does not read it (`mode_changed` is not dispatched, `thread_started` refreshes persistence).
- `claude-subagent-frames-unmapped` (DEVIATION): frames with a `parent_tool_use_id`, the task protocol (`task_started`/`task_progress`, synthetic Task cards, `provider_subagent` events) and the sidechain tracker are not ported; those frames emit nothing and sidechain transcripts are skipped in history. No golden covers them. Main-session frames match TS.
- `claude-rewind-command-unmapped` (DEVIATION): `/rewind` goes through the Agent SDK and is mapped like any other prompt.
- `claude-tool-result-image-files` (DEVIATION): tool-result images are written to a private temp dir by the mapper (as TS does), rendered as `file://` markdown; Windows path forms of `markdownImageSource` are not ported.
- `claude-session-key-uuid` (PARITY-QUIRK): `ClaudeAgentSession` draws a uuid for its usage session key at construction, so replay ids start at the second uuid.
- `claude-tool-input-shallow-equality` (PARITY-QUIRK): streamed tool input is skipped as unchanged only when every value is a primitive, so nested values re-emit the running call on each delta.
- `claude-answer-first-pending-permission` (PARITY-QUIRK): a permission `control_response` in a capture answers the first pending request (what the golden driver did); hosts answer by id through the typed API.
- `claude-partial-json-lone-surrogate` (PARITY-QUIRK): a lone high surrogate escape in streamed tool input travels as the `js_str` carrier char, a lone low surrogate becomes U+FFFD.

Known deviations and quirks of the Pi mapper (WP10, `paseo-provider-map/src/pi.rs`, goldens `fixtures/p4/provider-events/pi`: 6 `ok` and 2 `partial` replay; `08-catalog` and the OMP fixtures are `failed` in WP0 and have no events). `PiMapper` is the event half of `PiRpcAgentSession`: the RPC calls and process I/O stay in the session layer, which feeds runtime events and calls `start_turn` / `interrupt` / `respond_to_permission`.
- `pi-session-key-uuid` (PARITY-QUIRK): `PiRpcAgentSession` draws a uuid for its usage session key at construction, so replay ids start at the second uuid (turn id is `...0002`).
- `pi-answer-first-pending-permission` (PARITY-QUIRK): an `extension_ui_response` in a capture answers the first pending request when its id matches none (what the golden driver did); hosts answer by id through the typed API.
- `pi-custom-call-id-live-vs-history` (PARITY-QUIRK): a live custom message row is named `pi-custom-<uuid>`, a replayed one `pi-custom-<n>`.
- `pi-notice-line-terminators` (PARITY-QUIRK): the pi-subagents completion notice patterns use JS `.` and `$`, so a line ending in CR never matches.
- `pi-usage-poll` (DEVIATION): `usage_updated` comes from polling `get_session_stats`, which is an RPC concern; the mapper emits none (the goldens have none, the fake stats are all zero).
- `pi-no-turn-probe` (DEVIATION): the `get_state` probe after a slash-command prompt without an `agentInvoked` ack is not ported; an explicit ack (`prompt_acked`) or a `prompt_result` event completes a no-turn prompt, inline instead of one macrotask later.
- `pi-out-of-band-compaction` (DEVIATION): `/compact` out-of-band emission is the session layer's.
- `pi-extension-io` (DEVIATION): child-session file reads and following, and the pi-subagents `status.json` poll, are file I/O. The extension host records the child sessions a mapping reports (`take_child_sessions`) and `PiChildSessionMapper` maps the rows a reader gives it; the `poll()` hook is not ported.
- `pi-history-hooks` (DEVIATION): `PiHistoryMapperHooks` exist for the OMP wrapper (host-only, Q8) and are not ported.
- `pi-strip-ansi-approximate` (DEVIATION): `command_output` text goes through a hand-written ansi-regex 5 matcher, tested on CSI and BEL-terminated OSC sequences only.
