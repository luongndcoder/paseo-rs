/**
 * Generates crates/paseo-tools/tests/data/behavior-vectors.json: tool calls through a real
 * McpServer/Client pair over the TS catalog, against stubs described by JSON profiles (the Rust
 * test builds the same stubs from the same profiles). Records the response and the stub calls.
 *
 *   node <repo>/node_modules/tsx/dist/cli.mjs gen-behavior-vectors.mts <repo> <paseo-rs> > out.json
 */
const [repo, rsRoot] = process.argv.slice(2);
void rsRoot;
const serverDir = `${repo}/packages/server/src/server`;
const { createAgentMcpServer } = await import(`${serverDir}/agent/mcp-server.ts`);
const { Client } = await import(`${repo}/node_modules/@modelcontextprotocol/sdk/dist/esm/client/index.js`);
const { InMemoryTransport } = await import(`${repo}/node_modules/@modelcontextprotocol/sdk/dist/esm/inMemory.js`);
const pinoMod = (await import(`${repo}/node_modules/pino/pino.js`)).default;
const logger = pinoMod({ level: "silent" });

const FIXED_NOW = Date.parse("2026-02-01T12:00:00.000Z");
const RealDate = Date;
class FakeDate extends RealDate {
  constructor(...args: unknown[]) {
    if (args.length === 0) super(FIXED_NOW);
    else super(...(args as [number]));
  }
  static override now(): number {
    return FIXED_NOW;
  }
}
globalThis.Date = FakeDate as unknown as DateConstructor;
process.cwd = () => "/proc/cwd";

const T0 = "2026-01-15T10:00:00.000Z";
const BID = "00000000-0000-4000-8000-000000000000";

// ---- profiles (shared with the Rust test) ------------------------------------------------------

const richConfig = {
  model: "opus",
  thinkingOptionId: "high",
  providerOptions: { effort: "max" },
  featureValues: { fast: true },
  systemPrompt: "be brief",
  mcpServers: { x: { type: "http", url: "http://x" } },
};
const schedules = {
  "sch-1": {
    id: "sch-1", name: "nightly", prompt: "run", cadence: { type: "cron", expression: "0 2 * * *" },
    target: { type: "new-agent", config: { provider: "claude", cwd: "/work/repo" } }, status: "active",
    createdAt: T0, updatedAt: T0, nextRunAt: T0, lastRunAt: null, pausedAt: null, expiresAt: null, maxRuns: null, runs: [],
  },
  "hb-mine": {
    id: "hb-mine", name: null, prompt: "ping", cadence: { type: "cron", expression: "* * * * *" },
    target: { type: "agent", agentId: "caller-1" }, status: "active",
    createdAt: T0, updatedAt: T0, nextRunAt: T0, lastRunAt: null, pausedAt: null, expiresAt: null, maxRuns: null, runs: [],
  },
  "hb-other": {
    id: "hb-other", name: null, prompt: "ping", cadence: { type: "cron", expression: "* * * * *" },
    target: { type: "agent", agentId: "someone-else" }, status: "active",
    createdAt: T0, updatedAt: T0, nextRunAt: T0, lastRunAt: null, pausedAt: null, expiresAt: null, maxRuns: null, runs: [],
  },
};
const workspaces = [
  { workspaceId: "wks_a", cwd: "/work/repo", title: null, archived: false },
  { workspaceId: "wks_old", cwd: "/work/old", title: "Old", archived: true },
];
const providers = [
  { provider: "claude", status: "ready", enabled: true, label: "Claude", description: "Anthropic", modes: [{ id: "default" }] },
  { provider: "off", status: "ready", enabled: false },
  { provider: "down", status: "unavailable", enabled: true, error: "no binary" },
  { provider: "down2", status: "unavailable", enabled: true },
];
const caller = (extra: object = {}) => ({
  id: "caller-1", provider: "claude", cwd: "/work/repo", workspaceId: "wks_a", currentModeId: "default", config: {}, ...extra,
});
const PROFILES: Record<string, any> = {
  top: { caller: null, caps: ["schedules", "terminals", "workspaceRegistry", "workspaceUpdates", "directoryWorkspaces", "browser"], schedules, workspaces, providers, terminals: [{ id: "t1", name: "main", cwd: "/work/repo" }] },
  caller: { caller: caller(), caps: ["schedules", "terminals", "workspaceRegistry", "workspaceUpdates", "directoryWorkspaces", "browser"], schedules, workspaces, providers, terminals: [{ id: "t1", name: "main", cwd: "/work/repo" }] },
  rich: { caller: caller({ config: richConfig }), caps: ["schedules", "terminals", "workspaceRegistry", "workspaceUpdates", "browser"], schedules, workspaces, providers, terminals: [] },
  nocaps: { caller: null, caps: [], schedules: {}, workspaces: [], providers, terminals: [] },
  noworkspace: { caller: caller({ workspaceId: null }), caps: ["terminals", "browser", "workspaceMinting"], schedules: {}, workspaces: [], providers, terminals: [] },
};

// ---- stubs from a profile ------------------------------------------------------------------------

function makeDeps(profile: any, calls: any[], brokerPayload: unknown) {
  const rec =
    (target: string, method: string, fn: (...a: any[]) => any) =>
    (...args: unknown[]) => {
      calls.push({ target, method, args: JSON.parse(JSON.stringify(args)) });
      return fn(...args);
    };
  const has = (c: string) => profile.caps.includes(c);
  const callerAgent = profile.caller
    ? { ...profile.caller, workspaceId: profile.caller.workspaceId ?? undefined, lifecycle: "idle", availableModes: [] }
    : null;
  const agentManager = {
    getAgent: (id: string) => (callerAgent && id === callerAgent.id ? callerAgent : null),
    listAgents: () => [],
    getTimeline: () => [],
    setAgentMode: rec("agentManager", "setAgentMode", async () => null),
    setAgentModel: rec("agentManager", "setAgentModel", async () => undefined),
    setAgentThinkingOption: rec("agentManager", "setAgentThinkingOption", async () => undefined),
    setAgentFeature: rec("agentManager", "setAgentFeature", async () => undefined),
    updateAgentMetadata: rec("agentManager", "updateAgentMetadata", async () => undefined),
    listDraftFeatures: rec("agentManager", "listDraftFeatures", async () => []),
    hasInFlightRun: () => false,
    cancelAgentRun: rec("agentManager", "cancelAgentRun", async () => ({ status: "settled" })),
  };
  const sched = (id: string) => profile.schedules[id];
  const scheduleService = {
    list: async () => Object.values(profile.schedules),
    get: async (id: string) => sched(id) ?? null,
    inspect: async (id: string) => {
      if (!sched(id)) throw new Error(`Schedule not found: ${id}`);
      return sched(id);
    },
    createOrReplace: rec("scheduleService", "createOrReplace", async (input: unknown) => ({ id: "sch-new", runs: [], echo: input })),
    pause: rec("scheduleService", "pause", async () => undefined),
    resume: rec("scheduleService", "resume", async () => undefined),
    delete: rec("scheduleService", "delete", async () => undefined),
    runOnce: rec("scheduleService", "runOnce", async (id: string) => ({ id })),
    update: rec("scheduleService", "update", async (input: any) => ({ id: input.id, updated: true })),
    logs: rec("scheduleService", "logs", async () => [{ id: "run-1" }]),
  };
  const terminals = profile.terminals;
  const terminalManager = {
    getTerminals: async (cwd: string) => terminals.filter((t: any) => t.cwd === cwd),
    listDirectories: () => [...new Set(terminals.map((t: any) => t.cwd))],
    getTerminal: (id: string) => {
      const t = terminals.find((x: any) => x.id === id);
      return t ? { ...t, kill: rec("terminal", "kill", () => undefined), send: rec("terminal", "send", () => undefined) } : undefined;
    },
    createTerminal: rec("terminalManager", "createTerminal", async (input: any) => ({ id: "term-new", name: input.name ?? "Terminal", cwd: input.cwd })),
    captureTerminal: rec("terminalManager", "captureTerminal", async () => ({ lines: ["a"], totalLines: 1 })),
  };
  const mkWorkspace = (w: any) => ({
    workspaceId: w.workspaceId, projectId: "prj_1", cwd: w.cwd, kind: "directory", displayName: w.workspaceId,
    title: w.title, createdAt: T0, updatedAt: T0, archivedAt: w.archived ? T0 : null, background: false,
  });
  const workspaceRegistry = {
    get: async (id: string) => (profile.workspaces.find((w: any) => w.workspaceId === id) ? mkWorkspace(profile.workspaces.find((w: any) => w.workspaceId === id)) : null),
    list: async () => profile.workspaces.map(mkWorkspace),
    upsert: async (r: any) => {
      calls.push({ target: "workspaceRegistry", method: "upsert", args: [{ workspaceId: r.workspaceId, title: r.title, updatedAt: r.updatedAt }] });
    },
  };
  const providerEntries = profile.providers;
  const providerSnapshotManager = {
    listRegisteredProviderIds: () => providerEntries.map((p: any) => p.provider),
    listProviders: async () => providerEntries,
    getProvider: async (input: any) => {
      calls.push({ target: "providerSnapshotManager", method: "getProvider", args: [JSON.parse(JSON.stringify(input))] });
      const e = providerEntries.find((p: any) => p.provider === input.provider);
      if (!e) throw new Error(`Provider ${input.provider} is not configured`);
      return e;
    },
    listModels: async () => [],
  };
  const deps: any = {
    agentManager,
    agentStorage: { get: async () => null, list: async () => [] },
    terminalManager: has("terminals") ? terminalManager : null,
    scheduleService: has("schedules") ? scheduleService : null,
    providerSnapshotManager,
    daemonConfigStore: { get: () => ({ agentProfiles: [] }) },
    workspaceRegistry: has("workspaceRegistry") ? workspaceRegistry : undefined,
    logger,
  };
  if (has("workspaceUpdates")) deps.emitWorkspaceUpdatesForWorkspaceIds = rec("daemon", "emitWorkspaceUpdates", async () => undefined);
  if (has("directoryWorkspaces")) {
    deps.createDirectoryWorkspace = rec("daemon", "createDirectoryWorkspace", async (cwd: string, title: unknown, projectId: unknown, ctx: any) =>
      mkWorkspace({ workspaceId: "wks_created", cwd, title: title ?? null, archived: false }));
  }
  if (has("workspaceMinting")) deps.ensureWorkspaceForCreate = rec("daemon", "ensureWorkspaceForCreate", async () => "wks_minted");
  if (has("browser")) {
    deps.browserToolsEnabled = true;
    deps.browserToolsBroker = { execute: rec("browserBroker", "execute", async () => brokerPayload) };
  }
  if (profile.caller) deps.callerAgentId = profile.caller.id;
  return deps;
}

// ---- cases ---------------------------------------------------------------------------------------

interface Case { name: string; profile: string; tool: string; args: unknown; brokerPayload?: unknown; }
const cases: Case[] = [];
const add = (profile: string, tool: string, args: unknown, extra: Partial<Case> = {}) =>
  cases.push({ name: `${profile}/${tool}/${cases.length}`, profile, tool, args, ...extra });

// schedules: create_schedule
const base = { prompt: " run it ", cron: " 0 2 * * * " };
for (const profile of ["top", "caller", "rich"]) {
  const providerChoices: unknown[] = profile === "top" ? ["claude", "codex/gpt-5", " claude/opus ", "claude/", "/x", "  "] : [undefined, "claude", "codex", "claude/haiku", "codex/gpt-5", "claude/"];
  for (const provider of providerChoices) {
    const args: any = { ...base };
    if (provider !== undefined) args.provider = provider;
    add(profile, "create_schedule", args);
  }
  add(profile, "create_schedule", { ...base, provider: "claude", timezone: " UTC ", name: " nightly ", maxRuns: 3, expiresIn: "1h", isolation: "worktree" });
  add(profile, "create_schedule", { ...base, provider: "claude", cwd: "/abs/dir", isolation: "local", expiresIn: "2h30m" });
  add(profile, "create_schedule", { ...base, provider: "claude", expiresIn: "bogus" });
  add(profile, "create_schedule", { ...base, provider: "claude", expiresIn: "45" });
  add(profile, "create_schedule", { ...base, provider: "claude", expiresIn: "99999999999999999999h" });
  add(profile, "create_schedule", { prompt: "p", cron: "   ", provider: "claude" });
  add(profile, "create_schedule", { ...base, provider: "claude", name: "   " });
}
add("caller", "create_schedule", { ...base, cwd: "sub/dir" });
add("caller", "create_schedule", { ...base, cwd: "../up" });
add("rich", "create_schedule", { ...base, cwd: "/elsewhere", provider: "claude/sonnet" });
add("nocaps", "create_schedule", { ...base, provider: "claude" });

// heartbeats
for (const profile of ["caller", "rich", "top", "nocaps"]) {
  add(profile, "create_heartbeat", { prompt: " ping ", cron: "*/5 * * * *" });
  add(profile, "create_heartbeat", { prompt: "ping", cron: "*/5 * * * *", timezone: "Europe/Paris", name: " hb ", maxRuns: 2, expiresIn: "10m" });
  add(profile, "create_heartbeat", { prompt: "ping", cron: "*/5 * * * *", expiresIn: "nope" });
  for (const id of ["hb-mine", "hb-other", "sch-1", "missing"]) add(profile, "delete_heartbeat", { id });
}

// schedule reads and simple operations
for (const profile of ["top", "nocaps"]) {
  add(profile, "list_schedules", {});
  for (const tool of ["inspect_schedule", "pause_schedule", "resume_schedule", "delete_schedule", "schedule_logs", "run_schedule_once"]) {
    for (const id of ["sch-1", "hb-mine", "missing"]) add(profile, tool, { id });
  }
}

// update_schedule
const updates: any[] = [
  {},
  { name: null },
  { name: "n" },
  { prompt: "np" },
  { cron: "0 3 * * *" },
  { cron: "0 3 * * *", timezone: "UTC" },
  { timezone: "UTC" },
  { cron: "  ", timezone: "UTC" },
  { every: "5m" }, { every: " 30m " }, { every: "1h" }, { every: "2h" }, { every: "24h" }, { every: "90m" }, { every: "7m" }, { every: "30s" },
  { every: "0m" }, { every: "bogus" }, { every: "" }, { every: 5 }, { every: true },
  { every: "5m", cron: "* * * * *" }, { every: "5m", timezone: "UTC" }, { every: "5m", cron: "  " },
  { maxRuns: 4 }, { maxRuns: null },
  { provider: "codex" }, { provider: "codex/gpt-5" }, { provider: "codex/gpt-5", model: "gpt-5" }, { provider: "codex/gpt-5", model: "other" },
  { provider: "codex/gpt-5", model: null }, { provider: "codex/" }, { provider: "/gpt" }, { provider: "codex", model: "m" }, { provider: "codex", model: null },
  { model: "m" }, { model: null }, { model: " m " }, { model: "  " },
  { mode: "plan" }, { mode: null }, { cwd: "/new/cwd" }, { cwd: "rel" },
  { expiresIn: "1h" }, { expiresIn: "bogus" }, { clearExpires: true }, { clearExpires: false }, { expiresIn: "1h", clearExpires: true }, { expiresIn: "1h", clearExpires: false },
  { cron: "0 3 * * *", prompt: "np", maxRuns: null, provider: "codex/x", mode: null, cwd: "/c", expiresIn: "30m", name: "z" },
  { name: "n", unknownKey: 1 },
];
for (const args of updates) add("top", "update_schedule", { id: "sch-1", ...args });
add("top", "update_schedule", { id: "hb-mine", cron: "* * * * *" });
add("top", "update_schedule", { id: "missing", cron: "* * * * *" });
add("nocaps", "update_schedule", { id: "sch-1" });

// update_agent
for (const args of [
  { name: " New " }, { labels: { a: "b" } }, { name: "  ", labels: {} }, {}, { name: "n", labels: { k: "v" } },
  { settings: { modeId: "plan" } }, { settings: { model: null } }, { settings: { model: "m" } }, { settings: { thinkingOptionId: null } },
  { settings: { thinkingOptionId: "high", features: { f1: true, f2: 2 } } }, { settings: { features: {} } },
  { settings: { modeId: "plan", model: "m", thinkingOptionId: "low", features: { a: 1 } }, name: "x", labels: { l: "m" } },
]) add("top", "update_agent", { agentId: "agent-9", ...args });

// terminals
for (const profile of ["top", "caller", "noworkspace"]) {
  add(profile, "list_terminals", {});
  add(profile, "list_terminals", { cwd: "/work/repo" });
  add(profile, "list_terminals", { cwd: "sub" });
  add(profile, "list_terminals", { all: true });
  add(profile, "create_terminal", {});
  add(profile, "create_terminal", { cwd: "/work/repo", name: " build " });
  add(profile, "create_terminal", { cwd: "sub", name: "   " });
  add(profile, "kill_terminal", { terminalId: "t1" });
  add(profile, "kill_terminal", { terminalId: "nope" });
  add(profile, "capture_terminal", { terminalId: "t1" });
  add(profile, "capture_terminal", { terminalId: "t1", start: 2, end: 5, stripAnsi: false });
  add(profile, "capture_terminal", { terminalId: "t1", scrollback: true, start: 9 });
  add(profile, "send_terminal_keys", { terminalId: "t1", keys: "Enter" });
  for (const keys of ["Tab", "Escape", "Space", "BSpace", "C-c", "C-d", "C-z", "C-l", "C-a", "C-e", "hello", "C-x"]) add(profile, "send_terminal_keys", { terminalId: "t1", keys });
  add(profile, "send_terminal_keys", { terminalId: "t1", keys: "Enter", literal: true });
}
add("nocaps", "list_terminals", {});

// workspaces
for (const profile of ["top", "caller", "nocaps"]) {
  add(profile, "rename_workspace", { title: " Fresh " });
  add(profile, "rename_workspace", { workspaceId: "wks_a", title: "Explicit" });
  add(profile, "rename_workspace", { workspaceId: "wks_old", title: "x" });
  add(profile, "rename_workspace", { workspaceId: "nope", title: "x" });
  add(profile, "rename_workspace", { workspaceId: "  wks_a ", title: "x" });
  add(profile, "list_workspaces", {});
}
add("rich", "rename_workspace", { title: "x" });
add("noworkspace", "rename_workspace", { title: "x" });
add("top", "create_workspace", { isolation: "local", path: "/tmp", title: " T ", projectId: "prj_9", background: true });
add("caller", "create_workspace", { isolation: "local", path: "/tmp" });
add("rich", "create_workspace", { isolation: "local", path: "/tmp" });
add("top", "create_workspace", { isolation: "local", path: "/definitely/not/here" });
add("top", "create_workspace", { isolation: "local", path: "/tmp", mode: "branch-off" });
add("top", "create_workspace", { isolation: "local" });
add("caller", "create_workspace", { isolation: "local" });
add("top", "create_workspace", { isolation: "worktree", path: "/tmp", mode: "checkout-branch" });
add("top", "create_workspace", { isolation: "worktree", path: "/tmp", mode: "checkout-pr" });
add("top", "create_workspace", { isolation: "worktree", path: "/tmp", branch: "x" });
add("top", "create_workspace", { isolation: "worktree", path: "/tmp", mode: "checkout-branch", branch: "x", baseBranch: "y" });
add("top", "create_workspace", { isolation: "worktree", path: "/tmp", mode: "checkout-pr", prNumber: 1, branch: "y" });
add("top", "create_workspace", { isolation: "worktree", projectId: "p" });

// providers
for (const args of [
  { provider: "claude", cwd: "/w" }, { provider: " claude/opus ", cwd: "/w" }, { provider: "off", cwd: "/w" }, { provider: "down", cwd: "/w" }, { provider: "down2", cwd: "/w" },
  { provider: "ghost", cwd: "/w" }, { provider: "claude" }, { provider: "claude", cwd: "sub" }, { provider: "a/", cwd: "/w" }, { provider: "", cwd: "/w" }, { provider: "claude/opus/x", cwd: "/w" },
  { provider: "claude", cwd: "/w", settings: { modeId: "plan", model: "haiku", thinkingOptionId: "low", features: { f: 1 } } },
  { provider: "claude/opus", cwd: "/w", settings: { model: "" } },
  { provider: "claude", cwd: "/w", settings: { modeId: "", thinkingOptionId: "", features: {} } },
  { provider: "claude", cwd: "/w", settings: { bogus: 1 } },
]) for (const profile of ["top", "caller"]) add(profile, "inspect_provider", args);
add("top", "list_providers", {});

// browser: one payload per command and a few errors
const tab = (i: number) => ({ browserId: i === 0 ? BID : `t${i}`, title: i === 2 ? "" : `Tab ${i}`, url: `http://site${i}`, ...(i === 0 ? { isActive: true } : {}) });
const results: Record<string, any> = {
  list_tabs: { command: "list_tabs", tabs: [tab(0), tab(1), tab(2)] },
  new_tab: { command: "new_tab", browserId: BID, url: "http://example.com" },
  snapshot: { command: "snapshot", browserId: BID, url: "http://a", title: "Hello", snapshot: "- root\n  - @e1 button", stats: { nodeCount: 2, refCount: 1 } },
  click: { command: "click", ref: "@e1" }, fill: { command: "fill", ref: "@e2" }, wait: { command: "wait", matched: "text" },
  type: { command: "type", ref: "@e3" }, keypress: { command: "keypress", key: "Enter", ref: "@e4" }, navigate: { command: "navigate", url: "http://x" },
  back: { command: "back" }, forward: { command: "forward" }, reload: { command: "reload" },
  screenshot: { command: "screenshot", dataBase64: "AAAA", mimeType: "image/png", width: 800, height: 600 },
  upload: { command: "upload", ref: "@e5", filePaths: ["/a", "/b"] }, hover: { command: "hover", ref: "@e6" }, select: { command: "select", ref: "@e7", value: "v" },
  drag: { command: "drag", sourceRef: "@e1", targetRef: "@e2" }, logs: { command: "logs", console: [{}, {}], network: [{}] },
  evaluate: { command: "evaluate", resultJson: "{\"a\":1}", truncated: false }, scroll: { command: "scroll", deltaX: 0, deltaY: 100, ref: "@e8" },
  resize: { command: "resize", width: 100, height: 50 }, close_tab: { command: "close_tab", browserId: BID },
};
const argsByTool: Record<string, any> = {
  browser_list_tabs: {}, browser_new_tab: { url: "example.com" }, browser_snapshot: { browserId: BID },
  browser_click: { browserId: BID, ref: "@e1", button: "right", doubleClick: true, modifiers: ["Shift"] }, browser_fill: { browserId: BID, ref: "@e2", value: "x" },
  browser_wait: { browserId: BID, url: "/p", timeoutMs: 1500 }, browser_type: { browserId: BID, text: "hi", ref: "@e3" }, browser_keypress: { browserId: BID, key: "Enter" },
  browser_navigate: { browserId: BID, url: "https://x.y/z" }, browser_back: { browserId: BID }, browser_forward: { browserId: BID }, browser_reload: { browserId: BID },
  browser_screenshot: { browserId: BID, fullPage: true }, browser_upload: { browserId: BID, ref: "@e5", filePaths: ["/a"] }, browser_hover: { browserId: BID, ref: "@e6" },
  browser_select: { browserId: BID, ref: "@e7", value: "v" }, browser_drag: { browserId: BID, sourceRef: "@e1", targetRef: "@e2" }, browser_logs: { browserId: BID, maxEntries: 10 },
  browser_evaluate: { browserId: BID, function: "() => 1", ref: "@e1" }, browser_scroll: { browserId: BID, deltaX: 1, deltaY: 2 },
  browser_resize: { browserId: BID, width: 10, height: 20 }, browser_close_tab: { browserId: BID },
};
const commandOf = (tool: string) => tool.replace("browser_", "");
for (const profile of ["caller", "top", "noworkspace"]) {
  for (const [tool, args] of Object.entries(argsByTool)) {
    const command = commandOf(tool);
    add(profile, tool, args, { brokerPayload: { requestId: "r1", ok: true, result: results[command] } });
  }
}
const dialogs = [{ action: "accept", type: "confirm", message: "Sure?" }, { action: "dismiss", type: "alert", message: "Hi \"there\"" }];
for (const tool of ["browser_click", "browser_list_tabs", "browser_snapshot", "browser_screenshot", "browser_evaluate", "browser_logs", "browser_wait"]) {
  add("caller", tool, argsByTool[tool], { brokerPayload: { requestId: "r", ok: true, result: results[commandOf(tool)], dialogs } });
  add("caller", tool, argsByTool[tool], { brokerPayload: { requestId: "r", ok: true, result: results[commandOf(tool)], dialogs: [] } });
}
add("caller", "browser_list_tabs", {}, { brokerPayload: { requestId: "r", ok: true, result: { command: "list_tabs", tabs: [] }, dialogs } });
add("caller", "browser_list_tabs", {}, { brokerPayload: { requestId: "r", ok: true, result: { command: "list_tabs", tabs: [tab(0)] } } });
add("caller", "browser_evaluate", { browserId: BID, function: "() => 1" }, { brokerPayload: { requestId: "r", ok: true, result: { command: "evaluate", resultJson: "1", truncated: true } } });
add("caller", "browser_scroll", { browserId: BID, deltaX: 3, deltaY: 4 }, { brokerPayload: { requestId: "r", ok: true, result: { command: "scroll", deltaX: 3, deltaY: 4 } } });
add("caller", "browser_upload", { browserId: BID, ref: "@e1", filePaths: ["/a"] }, { brokerPayload: { requestId: "r", ok: true, result: { command: "upload", ref: "@e1", filePaths: ["/a"] } } });
add("caller", "browser_snapshot", { browserId: BID }, { brokerPayload: { requestId: "r", ok: true, result: { command: "snapshot", url: "u", title: "", snapshot: "s", stats: { nodeCount: 1, refCount: 1 } } } });
for (const error of [
  { code: "browser_disabled", message: "m", retryable: false }, { code: "browser_no_host", message: "No host", retryable: true }, { code: "browser_timeout", message: "m", retryable: true },
  { code: "screenshot_no_frame", message: "No frame", retryable: false }, { code: "browser_unsupported", message: "Nope", retryable: false }, { code: "browser_stale_ref", message: "m", retryable: false },
  { code: "browser_denied", message: "Denied", retryable: false }, { code: "other", message: "custom", retryable: false },
]) {
  add("caller", "browser_click", argsByTool.browser_click, { brokerPayload: { requestId: "r", ok: false, error } });
  add("caller", "browser_back", argsByTool.browser_back, { brokerPayload: { requestId: "r", ok: false, error, dialogs } });
}
add("noworkspace", "browser_list_tabs", {});
add("noworkspace", "browser_new_tab", {});

// ---- run ------------------------------------------------------------------------------------------

const out: any[] = [];
for (const c of cases) {
  const calls: any[] = [];
  const profile = PROFILES[c.profile];
  const deps = makeDeps(profile, calls, c.brokerPayload ?? { ok: true, result: { command: "back" } });
  const server = await createAgentMcpServer(deps);
  const client = new Client({ name: "gen", version: "1.0.0" });
  const [ct, st] = InMemoryTransport.createLinkedPair();
  await Promise.all([server.connect(st), client.connect(ct)]);
  let response: unknown;
  try {
    response = await client.callTool({ name: c.tool, arguments: c.args as never });
  } catch (e: any) {
    response = { threw: String(e?.message ?? e) };
  }
  await client.close();
  await server.close();
  out.push({ ...c, response, sideEffects: calls });
}
process.stdout.write(JSON.stringify({ profiles: PROFILES, cases: out }));
