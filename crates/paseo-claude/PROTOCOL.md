# Claude Code stream-json control protocol

What `@anthropic-ai/claude-agent-sdk` 0.3.246 does when it drives the `claude` CLI, reverse-engineered
from `sdk.mjs` and `sdk.d.ts` and checked against live traffic from CLI 2.1.301 (golden captures in
`fixtures/`, plus ad-hoc probes). Rust types live in `src/protocol/`. Anything not confirmed by one of
those sources is marked **UNVERIFIED**.

Sources: `ProcessTransport` (spawn, argv, env, line framing, exit), `Query` (control requests, hooks,
MCP tunnel, input streaming), `sD()` (option -> transport/initialize split), `SDKControl*Request` types.

## 1. Process

```
claude --output-format stream-json --verbose --input-format stream-json [flags...]
```

- Native binary is spawned directly (`pathToClaudeCodeExecutable`). If the path ends in `.js .mjs .ts .tsx .jsx`
  the SDK runs `node|bun <path> <args>`. Paseo overrides spawn through `spawnClaudeCodeProcess` (custom
  command prefix / append args); the argv below is what the SDK hands to that hook.
- stdio is three pipes. `cwd` = `Options.cwd`. `windowsHide: true`.
- stderr is read as UTF-8; the last ~2 KiB is kept for error messages and forwarded to `Options.stderr`.
- The prompt is never an argv entry: all input is stream-json on stdin, even for a single prompt.

### 1.1 argv mapping (order as the SDK emits it)

| Option | argv |
| --- | --- |
| (always) | `--output-format stream-json --verbose --input-format stream-json` |
| `thinking: {type:'adaptive'}` / `{type:'enabled'}` without budget | `--thinking adaptive` |
| `thinking: {type:'enabled', budgetTokens:n}` (also legacy `maxThinkingTokens:n`; `0` = disabled) | `--max-thinking-tokens n` |
| `thinking: {type:'disabled'}` | `--thinking disabled` |
| `thinking.display` (not for disabled) | `--thinking-display <summarized\|omitted>` |
| `effort` | `--effort <low\|medium\|high\|xhigh\|max>` |
| `maxTurns` (truthy) | `--max-turns n` |
| `maxBudgetUsd` | `--max-budget-usd n` |
| `taskBudget` | `--task-budget <total>` |
| `model` | `--model <id/alias>` |
| `agent` | `--agent <name>` |
| `betas` | `--betas a,b` |
| `outputFormat {type:'json_schema'}` | `--json-schema <json>` (schema is also sent in `initialize.jsonSchema`) |
| `debugFile` / `debug` | `--debug-file <p>` / `--debug` |
| `canUseTool` set | `--permission-prompt-tool stdio` (mutually exclusive with `permissionPromptToolName`) |
| `permissionPromptToolName` | `--permission-prompt-tool <name>` |
| `continue` | `--continue` |
| `resume` | `--resume=<session-id>` (`=` form) |
| `allowedTools` (+ `Skill(name)` rules from `skills`) | `--allowedTools a,b` |
| `disallowedTools` | `--disallowedTools a,b` |
| `tools` | `--tools a,b` / `--tools ""` (empty list) / `--tools default` |
| `mcpServers` (everything except `{type:'sdk', instance}`) | `--mcp-config '{"mcpServers":{...}}'` |
| `settingSources` | `--setting-sources=user,project,local` (`=` form; empty list gives `--setting-sources=`) |
| `strictMcpConfig` | `--strict-mcp-config` |
| `permissionMode` (SDK defaults to `default`) | `--permission-mode <mode>` |
| `allowDangerouslySkipPermissions` | `--allow-dangerously-skip-permissions` |
| `fallbackModel` (must differ from `model`) | `--fallback-model <id>` |
| `includeHookEvents` | `--include-hook-events` |
| `includePartialMessages` | `--include-partial-messages` |
| `sessionStore` | `--session-mirror` |
| `additionalDirectories` | `--add-dir <d>` per entry |
| `plugins` (`type:'local'`) | `--plugin-dir <p>` / `--plugin-dir-no-mcp <p>` |
| `forkSession` | `--fork-session` (used together with `--resume=`) |
| `resumeSessionAt` / `resumeDropsTurn` | `--resume-session-at=<uuid>` / `--resume-drops-turn=<bool>` |
| `sessionId` | `--session-id=<uuid>` (Paseo uses it for a fresh session with a known id) |
| `persistSession:false` | `--no-session-persistence` |
| `managedSettings` | `--managed-settings <json>` |
| `settings` (object -> JSON, or file path) and `extraArgs` | `--<key> <value>`; a `null` value gives bare `--<key>`; a value starting with `-` uses `--key=value` |
| `sandbox` | merged into the `--settings` JSON as `{sandbox: ...}` (`failIfUnavailable: true` added when `enabled` and unset) |

Not on argv, sent in `initialize` instead (section 3.1): `systemPrompt`, `appendSystemPrompt`, `agents`, `hooks`,
SDK-MCP server names, `title`, `skills` (array), `jsonSchema`, `forwardSubagentText`, `perTaskStopAffordance`,
`promptSuggestions`, `agentProgressSummaries`, `supportedDialogKinds`, `planModeInstructions`,
`appendSubagentSystemPrompt`, `toolAliases`, `excludeDynamicSections`, `webSearchIsolationExemptMcpServers`.

`ArgvOptions` / `build_argv` in `src/protocol/argv.rs` reproduces this table; the golden header argv of
fixtures 02, 04, 08, 09 are asserted in tests.

### 1.2 Environment

SDK defaults when the key is not already set in `Options.env` (or the inherited env):

| Variable | Value / effect |
| --- | --- |
| `CLAUDE_CODE_ENTRYPOINT` | `sdk-ts` |
| `CLAUDE_AGENT_SDK_VERSION` | `0.3.246` |
| `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING` | `true` iff `enableFileCheckpointing` (required for `rewind_files`) |
| `CLAUDE_CODE_SDK_HAS_OAUTH_REFRESH` / `CLAUDE_CODE_SDK_HAS_HOST_AUTH_REFRESH` | `1` iff the matching callback exists |
| `CLAUDE_CODE_QUESTION_PREVIEW_FORMAT` | `toolConfig.askUserQuestion.previewFormat` |
| `CLAUDE_CONFIG_DIR` | set only with `sessionStore` resume (temp materialised dir) |
| `TRACEPARENT`, `TRACESTATE` | OpenTelemetry propagation if an active context exists |
| `DEBUG` | removed, or `1` when `DEBUG_CLAUDE_AGENT_SDK` is truthy; `NODE_OPTIONS` is always removed |

Paseo additionally sets `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS=1` so the CLI emits `system/session_state_changed`.
Everything else is inherited (`Options.env` replaces `process.env`, it does not overlay it).

## 2. Framing

- UTF-8, one JSON object per line, `\n` terminated, both directions. No length prefix, no handshake bytes.
- Writer: `JSON.stringify(obj) + "\n"`. Reader (SDK): readline over stdout; blank lines skipped; a line that is not JSON
  is logged and **skipped** (never an error).
- Every line has a string `type`. Classification on the reader side:
  1. `control_response` -> resolve the pending request with that `response.request_id` (unmatched ones are buffered, cap 1024,
     in case the response beats the `await`).
  2. `control_request` -> handle (section 4), answer with a `control_response`.
  3. `control_cancel_request` -> abort the matching in-flight handler (section 6).
  4. `keep_alive` -> ignore.
  5. `transcript_mirror` -> session store mirroring only.
  6. anything else is a conversation message, delivered to the consumer in order.
- Unknown `type` values must be passed through and never fail parsing (`command_lifecycle` was not in `sdk.d.ts` yet is emitted
  by 2.1.301).

## 3. Host -> CLI (stdin)

### 3.1 Startup sequence

1. Spawn.
2. Write `initialize` control request immediately (before any user message). Fixture order: `initialize`, then `user`.
3. Write user messages (section 3.2). The SDK does not wait for the initialize response before writing the first user message.
4. CLI answers `initialize` with a `control_response` (payload in 4.2); the first `system/init` message is emitted after
   the CLI has received the first user message, not after `initialize` (probe: initialize + mcp_status + set_permission_mode
   without a user message produced no `init`).

```json
{"type":"control_request","request_id":"yld39picjy","request":{
  "subtype":"initialize",
  "hooks":{"PreToolUse":[{"matcher":"Bash","hookCallbackIds":["hook_0"],"timeout":5}],"PostToolUse":[{"hookCallbackIds":["hook_1"]}]},
  "sdkMcpServers":["paseo"],
  "systemPrompt":[""],
  "appendSystemPrompt":"...",
  "agents":{"reviewer":{"description":"...","prompt":"...","tools":["Read"],"model":"haiku"}},
  "forwardSubagentText":true,"perTaskStopAffordance":true}}
```

`initialize` fields (all optional; the SDK always includes the key, `JSON.stringify` drops `undefined`):

| Field | Source | Notes |
| --- | --- | --- |
| `hooks` | `Options.hooks` | `{Event: [{matcher?, hookCallbackIds[], timeout?}]}`. The SDK allocates ids `hook_<n>` (counter per Query, across all events and matchers, in registration order) and keeps `id -> callback`. Events with an empty matcher list are omitted. Callback ids are opaque to the CLI. |
| `sdkMcpServers` | names of in-process MCP servers | CLI creates a server of `type:'sdk'` per name and tunnels it through `mcp_message` (section 4.4). |
| `jsonSchema` | `outputFormat.schema` | |
| `systemPrompt` | `Options.systemPrompt` | string -> `[string]`; **unset -> `[""]`** (the SDK default is an empty prompt, not Claude Code's); array passes through (`SYSTEM_PROMPT_DYNAMIC_BOUNDARY` marks the cache boundary); preset object -> key omitted. |
| `appendSystemPrompt` | preset `append` | Paseo: `{type:'preset', preset:'claude_code', append}` -> only `appendSystemPrompt` is sent (may be `""`). |
| `excludeDynamicSections` | preset | |
| `agents` | `Options.agents` | `AgentDefinition`: `description, prompt, tools?, disallowedTools?, model?, mcpServers?, skills?, initialPrompt?, maxTurns?, background?, memory?, effort?, permissionMode?, criticalSystemReminder_EXPERIMENTAL?`. |
| `title`, `skills[]`, `planModeInstructions`, `appendSubagentSystemPrompt`, `toolAliases`, `promptSuggestions`, `agentProgressSummaries`, `forwardSubagentText`, `supportedDialogKinds`, `perTaskStopAffordance`, `webSearchIsolationExemptMcpServers` | same-named options | |

`perTaskStopAffordance:true` (Paseo sets it) makes `interrupt` stop only the main turn and leave background subagents running.
`initialize` may be sent again (`reinitialize`, hooks replaced); the response flag `hooks_applied` says whether they were taken.

### 3.2 User message

```json
{"type":"user","session_id":"","parent_tool_use_id":null,
 "uuid":"6ecca4d2-...",
 "message":{"role":"user","content":[
   {"type":"text","text":"..."},
   {"type":"image","source":{"type":"base64","media_type":"image/png","data":"..."}}]}}
```

- `message` is an Anthropic `MessageParam`; `content` is a string or blocks (`text`, `image` base64/url, `document`, `tool_result`).
- `session_id` is `""` for new input; the CLI fills in the real one on echoes.
- Optional: `uuid` (client id; **required to rewind to this message**, echoed in `command_lifecycle.command_uuid` and
  `assistant.user_message_uuid` / `result.user_message_uuid`), `priority: now|next|later`, `origin`, `isSynthetic`,
  `shouldQuery: false` (append to transcript, no turn), `timestamp`.
- Multi-turn: write another `user` line at any time. While a turn runs it is queued; `command_lifecycle` reports
  `queued -> started -> completed` per `uuid`. A slash command is expanded only if it is the last content block (Paseo comment in
  `toSdkUserMessage`).
- End of input: close stdin. Single-prompt SDK usage closes stdin after the first `result`; with callbacks (canUseTool, hooks,
  SDK MCP) it waits for the first `result` before closing, otherwise the CLI would exit while a callback response is pending.

### 3.3 Control requests (host -> CLI)

Envelope: `{"type":"control_request","request_id":"<id>","request":{"subtype":...}}`. The response is
`{"type":"control_response","response":{"subtype":"success","request_id":"<id>","response":{...}}}` (payload optional) or
`{"subtype":"error","request_id":"<id>","error":"<message>"}`.

| subtype | Fields | Success payload | Used by Paseo |
| --- | --- | --- | --- |
| `initialize` | section 3.1 | `commands[] {name, description, argumentHint, aliases?, builtin?}`, `agents[] {name, description, model?}`, `output_style`, `available_output_styles`, `models[] ModelInfo`, `account`, `pid`, `claude_code_version`, `current_permission_mode`, `session_state`, `capabilities[]`, `fast_mode_state`, `hooks_applied?`, assorted flags; **beside** `response`: `pending_permission_requests[]`, `pending_user_dialog_requests[]` | yes (`supportedCommands`, `supportedModels`, `supportedAgents`, `accountInfo`) |
| `interrupt` | `cancel_queued?: bool` | `{still_queued: uuid[], cancelled?: uuid[]}` | yes |
| `set_permission_mode` | `mode: default\|acceptEdits\|bypassPermissions\|plan\|dontAsk\|auto` | `{mode}` | yes |
| `set_model` | `model?: string\|null` (omitted/null = default) | none | yes |
| `set_max_thinking_tokens` | `max_thinking_tokens?: n\|null`, `thinking_display?: summarized\|omitted\|null` | none | no (thinking via argv) |
| `rewind_files` | `user_message_id`, `dry_run?` | `{canRewind, error?, filesChanged?, insertions?, deletions?, skippedLinks?}`; no checkpoint gives `{canRewind:false, error:"No file checkpoint found for this message."}` as a *success* | yes |
| `apply_flag_settings` | `settings: {...}` (e.g. `{fastMode:true}`) | none | yes (fast mode) |
| `mcp_status` | | `{mcpServers:[{name, status:connected\|failed\|needs-auth\|pending\|disabled, serverInfo?, error?, config?, scope?, source?, tools?[]}]}` | maybe |
| `mcp_set_servers` | `servers: {name: config}` (SDK servers as `{type:'sdk', name}`) | `McpSetServersResult` (UNVERIFIED shape) | no |
| `mcp_reconnect` / `mcp_toggle` | `serverName` (camelCase) [+ `enabled`] | none | no |
| `get_context_usage` | | large usage report (`categories, totalTokens, maxTokens, percentage, ...`) | possibly |
| `get_settings`, `get_usage`, `get_session_cost`, `list_models` | | settings / usage objects (UNVERIFIED) | no |
| `stop_task` | `task_id` | none | no |
| `background_tasks` | `tool_use_id?` | `{backgrounded?: bool}` (absent = true) | no |
| `cancel_async_message` | `message_uuid` | `{cancelled: bool}` | no |
| `read_file` | `path, max_bytes?, encoding?: utf-8\|base64` | `{contents, absPath, truncated?, encoding?}` | no |
| `seed_read_state` | `path, mtime` | none | no |
| `reload_plugins`, `reload_skills` | | `{commands, agents, plugins, mcpServers, error_count}` / `{skills}` | no |
| `rename_session`, `set_color` | `title` / `color` | none | no |
| `set_cwd` | `path, trust_accepted?, trusted_directory?` | UNVERIFIED (in sdk.mjs, not in d.ts) | no |
| `mcp_message` (host -> CLI) | server -> CLI JSON-RPC (notifications such as `tools/list_changed`) | UNVERIFIED | only with SDK MCP |
| other (`mcp_authenticate`, `mcp_clear_auth`, `mcp_oauth_callback_url`, `claude_authenticate`, `claude_oauth_*`, `channel_enable`, `submit_feedback`, `generate_session_title`, `side_question`, `ultrareview_launch`, `message_rated`, `remote_control`, `register_repo_root`, `file_suggestions`, `mcp_call`, `get_binary_version`) | | | no (carried as `Unknown`) |

An unsupported subtype gets `{"subtype":"error","request_id":..,"error":"Unsupported control request subtype: <x>"}` (verified).

### 3.4 Control responses (host -> CLI), see section 4.

## 4. CLI -> host (stdout)

### 4.1 Control requests from the CLI

The CLI's `request_id` is a UUID v4. The host answers each exactly once with a `control_response` carrying the same id.
A handler error is reported as `{"subtype":"error","request_id":..,"error":"<message>"}`. A handler may also
choose to send no response (see `canUseTool` returning `null`, `onElicitation` returning `null`).

| subtype | Fields | Host response payload |
| --- | --- | --- |
| `can_use_tool` | `tool_name, input, tool_use_id, permission_suggestions?[PermissionUpdate], blocked_path?, decision_reason?, decision_reason_type?, classifier_approvable?, suppress_always_allow_rule?, default_to_no?, matched_ask_rule? {source, tool_name, rule_content?}, title?, display_name?, description?, agent_id?, requires_user_interaction?` | `{behavior:"allow", updatedInput?, updatedPermissions?, toolUseID, decisionClassification?}` or `{behavior:"deny", message, interrupt?, toolUseID, ...}`. `toolUseID` is added by the SDK from `tool_use_id`. In every captured response `updatedInput` was present for allows (TS provider passes it); whether the CLI accepts allow without it is **UNVERIFIED**, so always echo the input. |
| `hook_callback` | `callback_id, input: HookInput, tool_use_id?` | `HookJSONOutput`: `{}` is a valid no-op; see 4.3 |
| `mcp_message` | `server_name, message: JSON-RPC` | request (`method` + non-null `id`): `{"mcp_response": <JSON-RPC response>}`; notification: `{"mcp_response":{"jsonrpc":"2.0","result":{},"id":0}}`; unknown server: error `SDK MCP server not found: <name>` |
| `elicitation` | `mcp_server_name, message, mode?: form\|url, url?, elicitation_id?, requested_schema?, title?, display_name?, description?` | `{action:"accept"\|"decline"\|"cancel", content?}` (MCP ElicitResult, UNVERIFIED on the wire); SDK default without handler: `{action:"decline"}` |
| `request_user_dialog` | `dialog_kind, payload, tool_use_id?` | dialog result; with no handler the SDK stays silent (no response) |
| `oauth_token_refresh` | | `{accessToken: string\|null, reason?}` (error if no callback) |
| `host_auth_token_refresh` | | `{authToken: string\|null}` |

The first three plus `elicitation` are the ones Paseo can hit. `can_use_tool` and `hook_callback` requests can be concurrent;
answer them out of order using `request_id`. A repeated delivery of an in-flight `request_id` is ignored.
`pending_permission_requests` / `pending_user_dialog_requests` inside a control *response* are redelivered requests
(each element is a full `control_request` envelope) that the host must process as if they had arrived on stdout
(honoured on `initialize`; ignored on other responses).

#### PermissionUpdate

`{type: addRules|replaceRules|removeRules, rules:[{toolName, ruleContent?}], behavior: allow|deny|ask, destination}`,
`{type: setMode, mode, destination}`, `{type: addDirectories|removeDirectories, directories[], destination}`;
`destination` is `userSettings|projectSettings|localSettings|session|cliArg`.

### 4.2 Control responses from the CLI

Same envelope as 3.3. `success.response` is absent for no-result requests (`set_model` verified; `apply_flag_settings` UNVERIFIED).
Response order is not guaranteed relative to conversation messages: on interrupt, `control_response` precedes the
`[Request interrupted by user]` user echo and the error `result`.

### 4.3 Hooks

Registered per event in `initialize.hooks`; the CLI sends a `hook_callback` request when a matching hook fires
(matcher is matched against the tool name for tool events). `input` is the hook payload with
`hook_event_name`, `session_id`, `transcript_path`, `cwd`, optional `prompt_id`, `permission_mode`, `agent_id`, `agent_type`,
`effort:{level}`, plus per-event fields:

| Event | Extra input fields |
| --- | --- |
| `PreToolUse` | `tool_name, tool_input, tool_use_id` |
| `PostToolUse` | `tool_name, tool_input, tool_response, tool_use_id, duration_ms?` |
| `PostToolUseFailure` | `tool_name, tool_input, tool_use_id, error, is_interrupt?, duration_ms?` |
| `UserPromptSubmit` | `prompt, source?, session_title?` |
| `SessionStart` | `source: startup\|resume\|clear\|compact\|fork, model?, session_title?` |
| `SessionEnd` | `reason` |
| `Stop` | `stop_hook_active, last_assistant_message?, background_tasks?, session_crons?` |
| `SubagentStart` | (`agent_id`, `agent_type` in base) |
| `SubagentStop` | `stop_hook_active, agent_transcript_path, last_assistant_message?, background_tasks?, session_crons?` |
| `PreCompact` | `trigger: manual\|auto, custom_instructions: string\|null` |
| `Notification` | `message, title?, notification_type` |
| `PermissionRequest` | `tool_name, tool_input, permission_suggestions?` |
| others (`PostToolBatch, StopFailure, PostCompact, PermissionDenied, Setup, TeammateIdle, TaskCreated, TaskCompleted, Elicitation, ElicitationResult, ConfigChange, WorktreeCreate, WorktreeRemove, InstructionsLoaded, CwdChanged, FileChanged, DirectoryAdded, MessageDisplay, UserPromptExpansion`) | `HookInput::Unknown(Value)` |

Output (`HookJSONOutput`): `{continue?, suppressOutput?, stopReason?, decision?: approve|block, systemMessage?, reason?,
terminalSequence?, hookSpecificOutput?: {hookEventName, ...}}`, or `{async:true, asyncTimeout?}`.
`PreToolUse` specific: `permissionDecision: allow|deny|ask|defer`, `permissionDecisionReason`, `updatedInput`, `additionalContext`.
`PermissionRequest` specific: `decision: {behavior:allow, updatedInput?, updatedPermissions?} | {behavior:deny, message?, interrupt?}`.
Unknown callback id: the SDK throws `No hook callback found for ID: <id>` (-> error response).
Separately, the CLI emits `system/hook_started|hook_progress|hook_response` for configured (non-SDK) hooks; with
`--include-hook-events` for all hook events.

### 4.4 In-process (SDK) MCP servers

- Host declares the server name in `initialize.sdkMcpServers`. The CLI then speaks MCP JSON-RPC through `mcp_message` control
  requests: `initialize` (protocolVersion `2025-11-25`, clientInfo `claude-code`), `notifications/initialized`, `tools/list`,
  `tools/call` (`params._meta` carries `claudecode/toolUseId` and `progressToken`).
- Host replies `{"mcp_response": <JSON-RPC response>}` inside `success.response` with the **JSON-RPC id of the request**
  (not the control `request_id`). Verified shape of `initialize` result: `{protocolVersion, capabilities:{tools:{listChanged:true}}, serverInfo}`.
- Notifications (no `id`) get the fixed ack `{"mcp_response":{"jsonrpc":"2.0","result":{},"id":0}}`.
- Server-initiated messages (e.g. `notifications/tools/list_changed`, progress) go host -> CLI as a new
  `control_request` with `subtype:"mcp_message"`, `server_name`, `message` and a fresh `request_id`. The CLI's reply is
  ignored by the SDK. **UNVERIFIED** whether the CLI replies at all.
- Tool name seen by the model: `mcp__<server>__<tool>`; Paseo pre-approves by adding those to `allowedTools`.
- Alternative Paseo may use instead of the tunnel: a plain `http` MCP server in `--mcp-config` (`/mcp/agents`).

### 4.5 Conversation messages

All carry `session_id`; most carry `uuid`. Typed in `src/protocol/messages.rs` and `stream.rs`.

| `type` / `subtype` | Meaning |
| --- | --- |
| `system/init` | session start: `cwd, session_id, tools[], mcp_servers[{name,status,source?}], model, permissionMode, slash_commands[], apiKeySource, claude_code_version, output_style, agents[], skills[], plugins[], capabilities[], fast_mode_state, ...` (extra keys: `memory_paths, original_cwd, additional_directories, messaging_socket_path, view_mode, ...`). One per session start, emitted after the first user message. |
| `system/status` | `status: "compacting"\|"requesting"\|null`, optional `permissionMode` (also the confirmation after `set_permission_mode`), `compact_result`, `compact_error` |
| `system/session_state_changed` | `state: idle\|running\|requires_action`; only with `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS=1` |
| `system/compact_boundary` | `compact_metadata {trigger, pre_tokens, post_tokens?, duration_ms?, preserved_*}` |
| `system/api_retry` | `attempt, max_retries, retry_delay_ms, error_status, error` |
| `system/task_started`, `task_updated`, `task_progress`, `task_notification` | background task / subagent lifecycle (`task_id`, `tool_use_id`, `status`, `usage`, `output_file`, `summary`) |
| `system/hook_started`, `hook_progress`, `hook_response` | configured hook execution |
| `system/permission_denied`, `thinking_tokens`, `commands_changed`, `notification`, `informational`, `local_command_output`, `files_persisted`, `memory_recall`, `plugin_install`, `elicitation_complete`, `mirror_error`, `worker_shutting_down`, `model_refusal_fallback`, `model_refusal_no_fallback`, `control_request_progress`, `background_tasks_changed`, `post_turn_summary` | see sdk.d.ts; first four typed in Rust, the rest `SystemMessage::Unknown` |
| `assistant` | `message` (Messages API Message: `id, model, content[], stop_reason, usage`), `parent_tool_use_id` (non-null for subagent output), `error?`, `uuid`, `user_message_uuid?`, `aborted?`, `supersedes?`. One per content block batch; the same `message.id` repeats. |
| `user` | tool results and replays: `message {role:user, content[...tool_result...]}`, `tool_use_result`, `timestamp`, `isReplay?`, `isSynthetic?`; also `"[Request interrupted by user]"` after an interrupt |
| `stream_event` | only with `--include-partial-messages`: `event` is a raw Messages stream event (`message_start`, `content_block_start/delta/stop`, `message_delta`, `message_stop`), plus `parent_tool_use_id`, `uuid`, `api_message_id`, `ttft_ms?`. Deltas: `text_delta`, `thinking_delta`, `input_json_delta`, `signature_delta`. The complete `assistant` message for a block arrives after its `content_block_delta`s and before its `content_block_stop`. |
| `result` | exactly one per turn: `subtype` `success` or `error_during_execution\|error_max_turns\|error_max_budget_usd\|error_max_structured_output_retries`, `is_error`, `result` (success text, or the error text when `is_error`), `errors[]` (error subtypes), `duration_ms, duration_api_ms, num_turns, stop_reason, total_cost_usd, usage, modelUsage, permission_denials[], terminal_reason, queued_turn_count, structured_output?, deferred_tool_use?, fast_mode_state, uuid`. Interrupted turn: `error_during_execution`, `terminal_reason:"aborted_streaming"`, `is_error:true`. Cost and usage are cumulative per process, `modelUsage` too. |
| `command_lifecycle` | `command_uuid, state: queued\|started\|completed` (not in d.ts) |
| `rate_limit_event` | `rate_limit_info {status, resetsAt, rateLimitType, utilization, overage*, unifiedWindows}` |
| `tool_progress` | `tool_use_id, tool_name, parent_tool_use_id, elapsed_time_seconds, task_id?, heartbeat?` |
| `auth_status`, `prompt_suggestion`, `tool_use_summary`, `active_goal`, `autocompact_state`, `conversation_reset` | see sdk.d.ts (carried as `Line::Unknown`) |
| `keep_alive`, `transcript_mirror` | internal |

Turn end rule: the `result` message ends a turn. Informational system messages (`task_notification`, `session_state_changed`,
`prompt_suggestion`) may follow it. With `queued_turn_count > 0` another turn follows without further input.

## 5. Session identity, resume, fork

- `session_id` is assigned by the CLI (`system/init`, all later lines) unless `--session-id` was given.
- Resume: spawn with `--resume=<id>` (history is loaded from `~/.claude/projects/<project>/<id>.jsonl`; `settingSources` still apply).
  `--fork-session` with resume starts a new session id that inherits the history.
  `--resume-session-at=<assistant-uuid>` truncates; `--resume-drops-turn=true` drops the last turn. **UNVERIFIED** exact semantics beyond the names.
- Paseo restarts the query (new process) on model/thinking/rewind changes and resumes by `claudeSessionId`.
- `rewind_files` needs `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING=true` and a user message that was sent with a client `uuid`
  after checkpointing started.

## 6. Cancellation

| What | How |
| --- | --- |
| Stop the running turn | `interrupt` control request (graceful; CLI replies, aborts the stream, emits the interrupt `user` echo and an `error_during_execution` result with `terminal_reason: aborted_streaming`). The process stays alive and usable for the next user message. |
| Cancel queued input | `interrupt` with `cancel_queued:true`, or `cancel_async_message {message_uuid}` |
| Abort a pending host control request | host writes `{"type":"control_cancel_request","request_id":"<id>"}` and drops the pending entry (SDK does this when the caller's `AbortSignal` fires) |
| CLI aborts a request it sent to the host | CLI writes `{"type":"control_cancel_request","request_id":"<id>"}`; the SDK aborts that handler's `AbortSignal` and drops the in-flight entry. A late handler result may still be written and is expected to be ignored by the CLI (**UNVERIFIED**) |
| Hard stop | close stdin, wait ~2 s, `SIGTERM`, then `SIGKILL` after 5 s (Windows: close stdin, `SIGKILL` after 5 s). All pending control requests are rejected locally with "Query closed before response received". |

## 7. Errors and exit semantics

- A control request that cannot be satisfied returns `{"subtype":"error","request_id","error":"<text>"}`. The SDK turns it into an exception
  with that message. Missing optional state yields a *success* with a payload that says so (`rewind_files` -> `canRewind:false`).
- API/turn failures are **not** protocol errors: they come as `assistant.error` (`authentication_failed`, `oauth_org_not_allowed`,
  `account_on_hold`, `billing_error`, `rate_limit`, `overloaded`, `invalid_request`, `model_not_found`, `server_error`, `max_output_tokens`, `unknown`)
  and/or a `result` with `is_error:true`.
- Process exit: exit code 0 after stdin EOF once the queue drains. A non-zero exit or signal makes the SDK throw
  `Claude Code process exited with code N. stderr: <tail>` / `... terminated by signal S` unless an error `result` was just seen,
  in which case the thrown error is `Claude Code returned an error result: <result text or errors joined by "; ">`.
  Observed exit codes in the golden captures: 0 normal, 1 after an interrupted turn closed by the SDK (06), 143 when the SDK
  terminated the process (07). Spawn failure (ENOENT) is reported as a missing binary; a spawn error with an existing path means a libc/arch mismatch.
- After exit/abort, writing to stdin is an error ("Cannot write to terminated process"); writes made before the deferred spawn completes are buffered.
- The CLI does not write protocol text to stderr; stderr is diagnostics only (the SDK exposes it via `Options.stderr`).

## 8. Version drift

- Pin the supported CLI range (this document: 2.1.301) and the SDK version used to derive the types (0.3.246). The CLI advertises feature flags in
  `system/init.capabilities` and `initialize` response `capabilities` (e.g. `interrupt_receipt_v1`, `interrupt_cancel_queued_v1`, `msg_lifecycle_v1`,
  `sdk_mcp_tools_list_changed`); gate optional behaviour on those rather than on version numbers.
- New fields and message types appear regularly: every Rust struct keeps unknown keys, every enum keeps unknown variants, and
  `Line::parse` re-serializes to the exact input JSON, so a newer CLI never breaks parsing.
- Known gaps against the SDK (not used by Paseo): session store mirroring, OAuth refresh flows, remote control, ultrareview, side questions.
