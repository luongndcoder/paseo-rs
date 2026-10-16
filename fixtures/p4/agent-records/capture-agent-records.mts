/**
 * P4 WP8 step C: agent record parity goldens. Drives the real TS AgentManager + AgentStorage
 * with scripted sessions and a manual clock, then reads back the JSON files the TS storage wrote.
 * `crates/paseo-core/tests/record_parity.rs` replays the same scenarios against the Rust manager
 * and `paseo-store` and compares the parsed files.
 *
 *   cd <ts>/packages/server
 *   node --import tsx <paseo-rs>/fixtures/p4/agent-records/capture-agent-records.mts \
 *     --out <paseo-rs>/fixtures/p4/agent-records/records.json
 *
 * Env: P4_TS_SERVER_DIR (default: cwd) = the packages/server dir whose sources are captured.
 */
import { mkdirSync, mkdtempSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";

const serverDir = path.resolve(process.env.P4_TS_SERVER_DIR ?? process.cwd());
const out = process.argv[process.argv.indexOf("--out") + 1];
if (!out) throw new Error("pass --out <file>");

// ---- manual clock ------------------------------------------------------------------------
const RealDate = Date;
const T0 = RealDate.parse("2026-01-01T00:00:00.000Z");
let nowMs = T0;
class ManualDate extends RealDate {
  constructor(...args: unknown[]) {
    if (args.length === 0) super(nowMs);
    else super(...(args as [number]));
  }
  static override now(): number {
    return nowMs;
  }
}
globalThis.Date = ManualDate as unknown as DateConstructor;
const step = (n: number): void => {
  nowMs = T0 + n * 1000;
};

const load = (rel: string) => import(pathToFileURL(path.join(serverDir, "src/server/agent", rel)).href);
const { AgentManager } = await load("agent-manager.ts");
const { AgentStorage } = await load("agent-storage.ts");
const noop: any = new Proxy(() => noop, { get: () => noop, apply: () => noop });
const logger = noop;

const CAPS = {
  supportsStreaming: false,
  supportsSessionPersistence: false,
  supportsSessionListing: true,
  supportsDynamicModes: false,
  supportsMcpServers: false,
  supportsReasoningStream: false,
  supportsToolInvocations: false,
};

type Mode = "complete" | "fail" | "hang" | "permission" | "complete-then-hang";

class ScriptSession {
  readonly provider = "codex";
  readonly capabilities = CAPS;
  readonly id: string;
  private runtimeModel: string | null = null;
  private subscribers = new Set<(e: unknown) => void>();
  private turns = 0;
  constructor(
    private readonly config: Record<string, unknown>,
    private readonly mode: Mode,
    n: number,
  ) {
    this.id = `session-${n}`;
  }
  async run() {
    return { sessionId: this.id, finalText: "", timeline: [] };
  }
  async startTurn() {
    const turnId = `turn-${++this.turns}`;
    setTimeout(() => {
      this.push({ type: "turn_started", provider: this.provider, turnId });
      if (this.mode === "complete" || (this.mode === "complete-then-hang" && this.turns === 1)) {
        this.push({ type: "turn_completed", provider: this.provider, turnId });
        this.runtimeModel = "gpt-5.2-codex";
      } else if (this.mode === "fail") {
        this.push({ type: "turn_failed", provider: this.provider, error: "scripted failure", turnId });
      } else if (this.mode === "permission") {
        this.push({
          type: "permission_requested",
          provider: this.provider,
          request: { id: "perm-1", provider: this.provider, kind: "tool", name: "Read file" },
          turnId,
        });
      }
    }, 0);
    return { turnId };
  }
  subscribe(cb: (e: unknown) => void) {
    this.subscribers.add(cb);
    return () => this.subscribers.delete(cb);
  }
  push(event: unknown) {
    for (const cb of this.subscribers) cb(event);
  }
  async *streamHistory() {}
  async getRuntimeInfo() {
    return {
      provider: this.provider,
      sessionId: this.id,
      model: this.runtimeModel ?? (this.config.model as string | undefined) ?? null,
      modeId: (this.config.modeId as string | undefined) ?? null,
    };
  }
  async getAvailableModes() {
    return [];
  }
  async getCurrentMode() {
    return null;
  }
  async setMode() {}
  getPendingPermissions() {
    return [];
  }
  async respondToPermission() {}
  describePersistence() {
    return { provider: this.provider, sessionId: this.id };
  }
  async interrupt() {}
  async close() {}
}

let sessionCounter = 0;
const clientFor = (mode: Mode) => ({
  provider: "codex",
  capabilities: CAPS,
  async isAvailable() {
    return true;
  },
  async createSession(config: Record<string, unknown>) {
    return new ScriptSession(config, mode, ++sessionCounter);
  },
  async fetchCatalog() {
    return { models: [{ provider: "codex", id: "gpt-5.4", label: "GPT-5.4", isDefault: true }], modes: [] };
  },
  async resumeSession(_h: unknown, config: Record<string, unknown> | undefined) {
    return new ScriptSession({ ...config }, mode, ++sessionCounter);
  },
});

const tick = () => new Promise<void>((r) => setImmediate(r));
const settle = async () => {
  for (let i = 0; i < 20; i += 1) await tick();
};

function readRecords(dir: string): Record<string, unknown> {
  const records: Record<string, unknown> = {};
  const walk = (d: string, rel: string) => {
    for (const entry of readdirSync(d, { withFileTypes: true })) {
      if (entry.isDirectory()) walk(path.join(d, entry.name), path.join(rel, entry.name));
      else if (entry.name.endsWith(".json")) {
        const record = JSON.parse(readFileSync(path.join(d, entry.name), "utf8"));
        records[record.id] = { file: path.join(rel, entry.name), record };
      }
    }
  };
  walk(dir, "");
  return records;
}

const ID = (n: number) => `00000000-0000-4000-8000-${String(n).padStart(12, "0")}`;
const CWD = "/tmp/paseo-record-parity";
mkdirSync(CWD, { recursive: true });
const PARENT = "paseo.parent-agent-id";

async function scenario(
  name: string,
  mode: Mode,
  body: (m: any, storage: any) => Promise<void>,
  results: Record<string, unknown>,
) {
  sessionCounter = 0;
  step(0);
  const dir = mkdtempSync(path.join(tmpdir(), "p4-records-"));
  const storage = new AgentStorage(path.join(dir, "agents"), logger);
  const manager = new AgentManager({ clients: { codex: clientFor(mode) }, registry: storage, logger });
  await body(manager, storage);
  await manager.flush();
  await settle();
  results[name] = readRecords(path.join(dir, "agents"));
}

const base = (extra: Record<string, unknown> = {}) => ({ provider: "codex", cwd: CWD, ...extra });
const results: Record<string, unknown> = {};

await scenario("create", "complete", async (m) => {
  step(1);
  await m.createAgent(
    base({ title: "Parity", modeId: "plan", model: "gpt-5.4" }),
    ID(1),
    { labels: { team: "core" }, workspaceId: "wks_1" },
  );
}, results);

await scenario("run-finished", "complete", async (m) => {
  step(1);
  await m.createAgent(base({ title: "Finished" }), ID(1), {});
  step(2);
  await m.runAgent(ID(1), "hello");
  await settle();
}, results);

await scenario("run-failed", "fail", async (m) => {
  step(1);
  await m.createAgent(base({ title: "Failed" }), ID(1), {});
  step(2);
  await m.runAgent(ID(1), "hello").catch(() => undefined);
  await settle();
}, results);

await scenario("archive-running-with-attention", "permission", async (m) => {
  step(1);
  await m.createAgent(base({ title: "Busy" }), ID(1), {});
  step(2);
  const it = m.streamAgent(ID(1), "hello");
  void (async () => {
    for await (const _e of it) {
      /* drain */
    }
  })().catch(() => undefined);
  await settle();
  step(3);
  await m.archiveAgent(ID(1));
}, results);

await scenario("close", "complete", async (m) => {
  step(1);
  await m.createAgent(base({ title: "To close" }), ID(1), {});
  step(2);
  await m.runAgent(ID(1), "hello");
  await settle();
  step(3);
  await m.closeAgent(ID(1));
}, results);

await scenario("labels-title-detach", "complete", async (m) => {
  step(1);
  await m.createAgent(base({ title: "Parent" }), ID(1), {});
  step(2);
  await m.createAgent(base({ title: "Child" }), ID(2), {
    labels: { [PARENT_LABEL()]: ID(1), "paseo.open-tab.desktop-client": "true" },
  });
  step(3);
  await m.setLabels(ID(2), { team: "core" });
  step(4);
  await m.setTitle(ID(2), "Renamed child");
  step(5);
  await m.updateAgentMetadata(ID(2), { title: "Metadata title", labels: { extra: "1" } });
  step(6);
  await m.detachAgent(ID(2));
}, results);

await scenario("cascade-archive", "complete", async (m) => {
  step(1);
  await m.createAgent(base({ title: "Parent" }), ID(1), {});
  step(2);
  await m.createAgent(base({ title: "Child A" }), ID(2), { labels: { [PARENT_LABEL()]: ID(1) } });
  step(3);
  await m.createAgent(base({ title: "Child B" }), ID(3), { labels: { [PARENT_LABEL()]: ID(1) } });
  step(4);
  await m.archiveAgent(ID(1));
}, results);

await scenario("archive-snapshot-normalizes-running", "complete", async (m, storage) => {
  await storage.upsert({
    id: ID(1),
    provider: "codex",
    cwd: CWD,
    createdAt: "2026-01-01T00:00:00.000Z",
    updatedAt: "2026-01-01T00:00:01.000Z",
    title: "Stored running",
    labels: {},
    lastStatus: "running",
    config: null,
    requiresAttention: true,
    attentionReason: "permission",
    attentionTimestamp: "2026-01-01T00:00:01.000Z",
  });
  step(5);
  await m.archiveSnapshot(ID(1), "2026-01-01T00:00:05.000Z");
}, results);

await scenario("archive-running-after-finished", "complete-then-hang", async (m) => {
  step(1);
  await m.createAgent(base({ title: "Second turn" }), ID(1), {});
  step(2);
  await m.runAgent(ID(1), "first");
  await settle();
  step(3);
  const it = m.streamAgent(ID(1), "second");
  void (async () => {
    for await (const _e of it) {
      /* drain */
    }
  })().catch(() => undefined);
  await settle();
  step(4);
  await m.archiveAgent(ID(1));
}, results);

function PARENT_LABEL() {
  return PARENT;
}

mkdirSync(path.dirname(out), { recursive: true });
writeFileSync(out, `${JSON.stringify({ clock: { t0: "2026-01-01T00:00:00.000Z", stepMs: 1000 }, scenarios: results }, null, 2)}\n`);
console.log(`wrote ${out}`);
process.exit(0);
