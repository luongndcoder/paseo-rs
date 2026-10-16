/**
 * P4 WP9: golden for the `fetch_agents` sequenced sync flow, captured from the TS code that owns
 * it: the real `DirectorySyncService` and the real stored-agent projection
 * (`buildStoredAgentPayload`), wrapped the way `session.ts` `handleFetchAgents` /
 * `readAgentDirectorySync` wrap them. `crates/paseo-core/tests/fetch_agents_sync.rs` replays the
 * same inputs through the Rust `SessionActor` and compares the frames.
 *
 *   cd <ts>/packages/server
 *   node --import tsx <paseo-rs>/fixtures/p4/session/capture-fetch-agents-sync.mts \
 *     --out <paseo-rs>/fixtures/p4/session
 *
 * Env: P4_TS_SERVER_DIR (default: cwd) = the packages/server dir whose sources are captured.
 * Writes `fetch-agents-sync.inputs.json` and `fetch-agents-sync.jsonl`.
 */
import { writeFileSync } from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

const serverDir = path.resolve(process.env.P4_TS_SERVER_DIR ?? process.cwd());
const outDir = process.argv[process.argv.indexOf("--out") + 1];
if (!outDir) throw new Error("pass --out <dir>");

const load = (rel: string) => import(pathToFileURL(path.join(serverDir, "src/server", rel)).href);
const { DirectorySyncService } = await load("directory-sync/index.ts");
const { buildStoredAgentPayload } = await load("agent/agent-projections.ts");
const { checkoutFromPersistedWorkspacePlacement } = await load("workspace-registry-model.ts");
const { resolveProjectDisplayName, resolveWorkspaceDisplayName } = await load("workspace-registry.ts");
const { SortablePager } = await load("pagination/sortable-pager.ts");

const GENERATION = "golden-generation";
const PROVIDERS = ["claude", "codex", "opencode"];

const project = {
  projectId: "prj_0000000000000001",
  rootPath: "/work/repo",
  kind: "git",
  displayName: "repo",
  projectKey: null,
  customName: null,
  customIconRevision: null,
  createdAt: "2026-03-01T00:00:00.000Z",
  updatedAt: "2026-03-01T00:00:00.000Z",
  archivedAt: null,
};
const workspace = {
  workspaceId: "ws_1",
  projectId: project.projectId,
  cwd: "/work/repo",
  kind: "local_checkout",
  displayName: "main",
  title: null,
  branch: "main",
  worktreeRoot: "/work/repo",
  baseBranch: null,
  isPaseoOwnedWorktree: false,
  mainRepoRoot: null,
  createdAt: "2026-03-01T00:00:00.000Z",
  updatedAt: "2026-03-01T00:00:00.000Z",
  archivedAt: null,
  autoArchivedChangeRequestUrl: null,
  pinnedAt: null,
  background: false,
};

const record = (id: string, updatedAt: string, extra: Record<string, unknown> = {}) => ({
  id,
  provider: "codex",
  cwd: "/work/repo",
  workspaceId: "ws_1",
  createdAt: "2026-03-01T10:00:00.000Z",
  updatedAt,
  lastActivityAt: updatedAt,
  lastUserMessageAt: null,
  title: `Agent ${id}`,
  labels: {},
  lastStatus: "idle",
  lastModeId: null,
  config: { model: "gpt-5" },
  features: [],
  persistence: null,
  requiresAttention: false,
  attentionReason: null,
  attentionTimestamp: null,
  ...extra,
});

const records = new Map<string, any>([
  ["agent-a", record("agent-a", "2026-03-01T12:00:00.000Z")],
  ["agent-b", record("agent-b", "2026-03-01T11:00:00.000Z", { lastStatus: "running" })],
]);

const placement = () => ({
  projectKey: project.projectId,
  projectName: resolveProjectDisplayName(project),
  workspaceName: resolveWorkspaceDisplayName(workspace),
  checkout: checkoutFromPersistedWorkspacePlacement({ workspace }),
});

const entryOf = (rec: any) => ({
  agent: buildStoredAgentPayload(rec, new Set(PROVIDERS)),
  project: placement(),
});

const pager = new SortablePager({
  validKeys: ["status_priority", "created_at", "updated_at", "title"],
  defaultSort: [{ key: "updated_at", direction: "desc" }],
  label: "fetch_agents",
  getId: (agent: any) => agent.id,
  getSortValue: (agent: any, key: string) =>
    key === "updated_at" ? Date.parse(agent.updatedAt) : key === "created_at" ? Date.parse(agent.createdAt) : "",
});

const directory = new DirectorySyncService(GENERATION);
const lines: unknown[] = [];

const snapshotEntries = () => {
  const sort = pager.normalizeSort(undefined);
  return [...records.values()]
    .filter((rec) => !rec.archivedAt)
    .map(entryOf)
    .sort((l, r) => pager.compare(l.agent, r.agent, sort));
};

type Step =
  | { op: "fetch"; name: string; request: Record<string, unknown> }
  | { op: "set_agent"; name: string; record: Record<string, unknown> }
  | { op: "archive_agent"; name: string; id: string; archivedAt: string }
  | { op: "live_update"; name: string; id: string };

const steps: Step[] = [
  {
    op: "fetch",
    name: "snapshot",
    request: { type: "fetch_agents_request", requestId: "req-1", scope: "active", sync: {} },
  },
  {
    op: "set_agent",
    name: "agent-a-changes",
    record: record("agent-a", "2026-03-01T13:00:00.000Z", { title: "Agent a renamed", lastStatus: "running" }),
  },
  { op: "live_update", name: "agent-a-live", id: "agent-a" },
  {
    op: "fetch",
    name: "changes",
    request: {
      type: "fetch_agents_request",
      requestId: "req-2",
      scope: "active",
      sync: { generation: GENERATION, afterSeq: 2 },
    },
  },
  { op: "archive_agent", name: "agent-b-archived", id: "agent-b", archivedAt: "2026-03-01T14:00:00.000Z" },
  {
    op: "fetch",
    name: "removal",
    request: {
      type: "fetch_agents_request",
      requestId: "req-3",
      scope: "active",
      sync: { generation: GENERATION, afterSeq: 3 },
    },
  },
  {
    op: "fetch",
    name: "cursor_expired",
    request: {
      type: "fetch_agents_request",
      requestId: "req-4",
      scope: "active",
      sync: { generation: GENERATION, afterSeq: 999 },
    },
  },
  {
    op: "fetch",
    name: "generation_changed",
    request: {
      type: "fetch_agents_request",
      requestId: "req-5",
      scope: "active",
      sync: { generation: "previous-boot", afterSeq: 4 },
    },
  },
  {
    op: "fetch",
    name: "no_afterSeq",
    request: {
      type: "fetch_agents_request",
      requestId: "req-6",
      scope: "active",
      sync: { generation: GENERATION },
    },
  },
  {
    op: "fetch",
    name: "includeBackground_generation_suffix",
    request: {
      type: "fetch_agents_request",
      requestId: "req-7",
      scope: "active",
      includeBackground: true,
      sync: { generation: `${GENERATION}:background`, afterSeq: 0 },
    },
  },
  {
    op: "fetch",
    name: "sync_with_filter_is_rejected",
    request: {
      type: "fetch_agents_request",
      requestId: "req-8",
      scope: "active",
      filter: { labels: { team: "a" } },
      sync: {},
    },
  },
];

for (const step of steps) {
  if (step.op === "set_agent") {
    records.set(step.record.id as string, step.record);
    lines.push({ step: step.name, op: step.op });
  } else if (step.op === "archive_agent") {
    records.set(step.id, { ...records.get(step.id), archivedAt: step.archivedAt });
    lines.push({ step: step.name, op: step.op });
  } else if (step.op === "live_update") {
    const entry = entryOf(records.get(step.id));
    const payload = directory.sequenceAgentUpdate(
      { kind: "upsert", agent: entry.agent, project: entry.project },
      entry,
      step.id,
      true,
      false,
    );
    lines.push({ step: step.name, op: step.op, message: { type: "agent_update", payload } });
  } else {
    const request = step.request as any;
    let message: unknown;
    if (request.scope !== "active" || request.filter) {
      message = {
        type: "rpc_error",
        payload: {
          requestId: request.requestId,
          requestType: request.type,
          error: "Sequenced agent directory reads require scope=active and no filter.",
          code: "invalid_request",
        },
      };
    } else {
      const result = directory.synchronizeAgents(snapshotEntries(), request.sync ?? {}, request.includeBackground);
      message = {
        type: "fetch_agents_response",
        payload: { requestId: request.requestId, ...result, entries: result.entries },
      };
    }
    lines.push({ step: step.name, op: "fetch", request, message });
  }
}

writeFileSync(
  path.join(outDir, "fetch-agents-sync.inputs.json"),
  `${JSON.stringify({ generation: GENERATION, providers: PROVIDERS, project, workspace, steps: steps.map((s) => (s.op === "set_agent" ? { ...s } : s)), initialRecords: [...new Map([["agent-a", record("agent-a", "2026-03-01T12:00:00.000Z")], ["agent-b", record("agent-b", "2026-03-01T11:00:00.000Z", { lastStatus: "running" })]]).values()] }, null, 2)}\n`,
);
writeFileSync(path.join(outDir, "fetch-agents-sync.jsonl"), `${lines.map((l) => JSON.stringify(l)).join("\n")}\n`);
console.log(`wrote ${lines.length} steps`);
