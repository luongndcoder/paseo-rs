/**
 * P4 WP13: `hub-relationship.json` goldens. Drives the real TS `HubRelationshipController` with a
 * scripted remote and a manual clock, reads back the bytes it wrote for every state, and replays
 * a table of persisted files (v1, v2, invalid) through its constructor to record what it does
 * with each. `crates/paseo-hub/tests/record_parity.rs` replays both against the Rust actor.
 *
 *   cd <ts>/packages/server
 *   node --import tsx <paseo-rs>/fixtures/p4/hub/capture-hub-relationship.mts \
 *     --out <paseo-rs>/fixtures/p4/hub
 *
 * Env: P4_TS_SERVER_DIR (default: cwd) = the packages/server dir whose sources are captured.
 */
import { createRequire, syncBuiltinESMExports } from "node:module";
import { mkdirSync, mkdtempSync, readFileSync, readdirSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";

const serverDir = path.resolve(process.env.P4_TS_SERVER_DIR ?? process.cwd());
const outDir = process.argv[process.argv.indexOf("--out") + 1];
if (!outDir) throw new Error("pass --out <dir>");

// ---- deterministic time and randomness -------------------------------------------------
const NOW = "2026-07-13T00:00:00.000Z";
const nodeRequire = createRequire(import.meta.url);
const nodeCrypto = nodeRequire("node:crypto");
let uuidCounter = 0;
let bytesCounter = 0;
nodeCrypto.randomUUID = () => {
  uuidCounter += 1;
  return `00000000-0000-4000-8000-${String(uuidCounter).padStart(12, "0")}`;
};
nodeCrypto.randomBytes = (size: number) => {
  bytesCounter += 1;
  return Buffer.from(Array.from({ length: size }, (_, i) => (i * 7 + bytesCounter * 31) & 255));
};
syncBuiltinESMExports();

const load = (rel: string) => import(pathToFileURL(path.join(serverDir, "src/server", rel)).href);
const serverRequire = createRequire(path.join(serverDir, "package.json"));
const pino = serverRequire("pino");
const { HubRelationshipController } = await load("hub/relationship-controller.ts");
const { HubEnrollmentRejectedError } = await load("hub/relationship-remote.ts");

// ---- scripted remote and clock ----------------------------------------------------------
type Deferred = { promise: Promise<unknown>; resolve(v?: unknown): void; reject(e: Error): void };
function deferred(): Deferred {
  let resolve!: (v?: unknown) => void;
  let reject!: (e: Error) => void;
  const promise = new Promise((a, b) => {
    resolve = a;
    reject = b;
  });
  return { promise, resolve, reject };
}

class ScriptedRemote {
  enrollMode: "hold" | "ok" | "offline" | "reject403" = "ok";
  revokeMode: "hold" | "ok" = "ok";
  enrollments: any[] = [];
  revocations: any[] = [];
  sockets: { input: any; events: any }[] = [];
  held = deferred();
  async enroll(input: any) {
    this.enrollments.push(input);
    if (this.enrollMode === "hold") return this.held.promise;
    if (this.enrollMode === "offline") throw new Error("offline");
    if (this.enrollMode === "reject403") throw new HubEnrollmentRejectedError(403);
    return {
      daemonId: input.daemonId,
      permissions: input.permissions,
      webSocketUrl: "wss://hub.test/daemon",
    };
  }
  async updatePermissions(input: any) {
    return { permissions: input.permissions };
  }
  async revoke(input: any) {
    this.revocations.push(input);
    if (this.revokeMode === "hold") return new Promise(() => undefined);
  }
  openSocket(input: any, events: any) {
    this.sockets.push({ input, events });
    return { close() {} };
  }
}

class ManualClock {
  tasks: { cancelled: boolean; task: () => void }[] = [];
  now() {
    return new Date(NOW);
  }
  delay() {
    return 0;
  }
  schedule(_delay: number, task: () => void) {
    const t = { cancelled: false, task };
    this.tasks.push(t);
    return { cancel: () => (t.cancelled = true) };
  }
  pendingTasks() {
    return this.tasks.filter((t) => !t.cancelled).length;
  }
}

const silent = pino({ level: "silent" });
const attachCalls: unknown[] = [];

function newHome(): string {
  return mkdtempSync(path.join(tmpdir(), "p4-hub-"));
}

function controllerFor(home: string, remote: ScriptedRemote, clock = new ManualClock()) {
  return new HubRelationshipController({
    paseoHome: home,
    hostname: "fixture-host",
    serverId: "server-fixture",
    daemonPublicKey: "public-key-fixture",
    logger: silent,
    remote,
    clock,
    retryPolicy: clock,
    createDaemonId: () => "daemon-fixture",
    attachSocket: async (_socket: unknown, options: unknown) => void attachCalls.push(options),
    updateAttachedPermissions: () => undefined,
    createExecutionAgents: () => ({
      create: async () => {
        throw new Error("unused");
      },
      control: async () => undefined,
      subscribe: () => () => undefined,
      invalidateAuthority: async () => undefined,
    }),
  });
}

const file = (home: string) => path.join(home, "hub-relationship.json");
const read = (home: string): string | null =>
  existsSync(file(home)) ? readFileSync(file(home), "utf8") : null;
const tick = () => new Promise((r) => setTimeout(r, 20));

// ---- state goldens ----------------------------------------------------------------------
const states: Record<string, { text: string | null; status?: unknown }> = {};
let ceremony: { daemonId: string; idempotencyKey: string; secret: string } | null = null;

async function connectedActive(remote = new ScriptedRemote()) {
  const home = newHome();
  const controller = controllerFor(home, remote);
  await controller.connect({
    hubUrl: "https://hub.test",
    token: "token-1",
    permissions: ["hub.execute"],
  });
  return { home, controller, remote };
}

{
  const remote = new ScriptedRemote();
  remote.enrollMode = "hold";
  const home = newHome();
  const controller = controllerFor(home, remote);
  const connecting = controller.connect({
    hubUrl: "https://hub.test/",
    token: "token-1",
    permissions: ["hub.execute"],
  });
  await tick();
  states.pending = { text: read(home), status: controller.status() };
  const pending = JSON.parse(states.pending.text!);
  ceremony = {
    daemonId: pending.relationship.daemonId,
    idempotencyKey: pending.relationship.idempotencyKey,
    secret: pending.credential.secret,
  };
  const second = controller.connect({
    hubUrl: "https://hub.test",
    token: "fresh-token",
    permissions: ["hub.execute"],
  });
  await tick();
  states.pending_fresh_token = { text: read(home), status: controller.status() };
  remote.held.resolve({
    daemonId: "daemon-fixture",
    permissions: ["hub.execute"],
    webSocketUrl: "wss://hub.test/daemon",
  });
  await Promise.all([connecting, second]);
  states.active = { text: read(home), status: controller.status() };
  await controller.updatePermissions({ grant: ["daemon.read"], revoke: [] });
  states.active_updated_permissions = { text: read(home), status: controller.status() };
  await controller.updatePermissions({ grant: [], revoke: ["hub.execute"] });
  states.active_revoked_execute = { text: read(home), status: controller.status() };
}
{
  const { home, controller, remote } = await connectedActive();
  remote.revokeMode = "hold";
  void controller.disconnect({ force: false });
  await tick();
  states.disconnecting = { text: read(home), status: controller.status() };
}
{
  const { home, controller, remote } = await connectedActive();
  remote.sockets[0].events.rejected(401);
  states.revoked_401 = { text: read(home), status: controller.status() };
}
{
  const { home, controller, remote } = await connectedActive();
  remote.sockets[0].events.closed(4403);
  states.revoked_4403 = { text: read(home), status: controller.status() };
}
{
  // Reconnect after a transient close: the record is unchanged.
  const { home, controller, remote } = await connectedActive();
  remote.sockets[0].events.closed(1006);
  states.reconnecting_after_close = { text: read(home), status: controller.status() };
}

// ---- load table -------------------------------------------------------------------------
const active = JSON.parse(states.active.text!);
const pendingRecord = JSON.parse(states.pending.text!);
const revoked = JSON.parse(states.revoked_401.text!);
const v1Relationship = (extra: Record<string, unknown> = {}) => ({
  daemonId: "daemon-legacy",
  idempotencyKey: "ceremony-legacy",
  hubOrigin: "https://hub.test",
  createdAt: "2026-07-13T00:00:00.000Z",
  scopes: ["hub.execution.*"],
  ...extra,
});
const v1Active = (relationship: unknown) => ({
  version: 1,
  state: "active",
  relationship,
  credential: { secret: "credential" },
  transport: { kind: "direct_websocket", webSocketUrl: "wss://hub.test/daemon" },
});
const v1Pending = (hubOrigin: string) => ({
  version: 1,
  state: "pending",
  relationship: v1Relationship({ hubOrigin }),
  credential: { secret: "credential" },
  enrollment: { token: "enrollment-token" },
  identity: { serverId: "server-1", daemonPublicKey: "public-key" },
});
const withRel = (record: any, patch: Record<string, unknown>) => ({
  ...record,
  relationship: { ...record.relationship, ...patch },
});
const stringify = (v: unknown) => JSON.stringify(v, null, 2) + "\n";

const loadCases: { name: string; text: string }[] = [
  ...Object.entries(states).map(([name, s]) => ({ name: `golden-${name}`, text: s.text! })),
  { name: "v1-active-execution-scope", text: stringify(v1Active(v1Relationship())) },
  { name: "v1-active-other-scope", text: stringify(v1Active(v1Relationship({ scopes: ["other"] }))) },
  { name: "v1-active-no-scopes", text: stringify(v1Active(v1Relationship({ scopes: undefined }))) },
  { name: "v1-active-bad-scope-type", text: stringify(v1Active(v1Relationship({ scopes: [1] }))) },
  { name: "v1-pending", text: stringify(v1Pending("https://hub.test")) },
  { name: "v1-unknown-state", text: JSON.stringify({ version: 1, state: "unknown" }) },
  { name: "not-json", text: "{not-json" },
  { name: "empty-file", text: "" },
  { name: "version-3", text: stringify({ ...active, version: 3 }) },
  { name: "version-string", text: stringify({ ...active, version: "2" }) },
  { name: "unknown-state", text: stringify({ ...active, state: "paused" }) },
  { name: "unknown-keys-stripped", text: stringify({ ...active, extra: 1, credential: { secret: "s", more: true } }) },
  { name: "permissions-wildcard", text: stringify(withRel(active, { permissions: ["*"] })) },
  { name: "permissions-duplicates", text: stringify(withRel(active, { permissions: ["hub.execute", "hub.execute"] })) },
  { name: "permissions-not-array", text: stringify(withRel(active, { permissions: "hub.execute" })) },
  { name: "permissions-all", text: stringify(withRel(active, { permissions: ["daemon.read", "daemon.manage", "tunnel.manage", "access.manage", "workspace.read", "workspace.write", "workspace.manage", "automation.manage", "hub.execute"] })) },
  { name: "origin-trailing-slash", text: stringify(withRel(active, { hubOrigin: "https://hub.test/" })) },
  { name: "origin-ftp", text: stringify(v1Pending("ftp://hub.test")) },
  { name: "origin-credentials", text: stringify(v1Pending("https://user:password@hub.test")) },
  { name: "origin-query", text: stringify(v1Pending("https://hub.test?token=secret")) },
  { name: "origin-empty-query", text: stringify(v1Pending("https://hub.test?")) },
  { name: "origin-fragment", text: stringify(v1Pending("https://hub.test#secret")) },
  { name: "origin-not-url", text: stringify(withRel(active, { hubOrigin: "hub.test" })) },
  { name: "daemon-id-empty", text: stringify(withRel(active, { daemonId: "" })) },
  { name: "idempotency-key-missing", text: stringify(withRel(active, { idempotencyKey: undefined })) },
  { name: "created-at-any-string", text: stringify(withRel(active, { createdAt: "yesterday" })) },
  { name: "secret-empty", text: stringify({ ...active, credential: { secret: "" } }) },
  { name: "credential-missing", text: stringify({ ...active, credential: undefined }) },
  { name: "transport-ftp", text: stringify({ ...active, transport: { kind: "direct_websocket", webSocketUrl: "ftp://hub.test/daemon" } }) },
  { name: "transport-fragment", text: stringify({ ...active, transport: { kind: "direct_websocket", webSocketUrl: "wss://hub.test/daemon#fragment" } }) },
  { name: "transport-empty-fragment", text: stringify({ ...active, transport: { kind: "direct_websocket", webSocketUrl: "wss://hub.test/daemon#" } }) },
  { name: "transport-other-host", text: stringify({ ...active, transport: { kind: "direct_websocket", webSocketUrl: "wss://other.test/daemon" } }) },
  { name: "transport-ws-for-https-hub", text: stringify({ ...active, transport: { kind: "direct_websocket", webSocketUrl: "ws://hub.test/daemon" } }) },
  { name: "transport-explicit-default-port", text: stringify({ ...active, transport: { kind: "direct_websocket", webSocketUrl: "wss://hub.test:443/daemon" } }) },
  { name: "transport-other-port", text: stringify({ ...active, transport: { kind: "direct_websocket", webSocketUrl: "wss://hub.test:8443/daemon" } }) },
  { name: "transport-wrong-kind", text: stringify({ ...active, transport: { kind: "tcp", webSocketUrl: "wss://hub.test/daemon" } }) },
  { name: "transport-missing-on-active", text: stringify({ ...active, transport: undefined }) },
  { name: "pending-missing-identity", text: stringify({ ...pendingRecord, identity: undefined }) },
  { name: "pending-empty-token", text: stringify({ ...pendingRecord, enrollment: { token: "" } }) },
  { name: "revoked-without-transport", text: stringify({ ...revoked, transport: undefined }) },
  { name: "revoked-null-transport", text: stringify({ ...revoked, transport: null }) },
  { name: "revoked-without-reason", text: stringify({ ...revoked, reason: undefined }) },
  { name: "revoked-null-reason", text: stringify({ ...revoked, reason: null }) },
  { name: "revoked-with-credential-stripped", text: stringify({ ...revoked, credential: { secret: "leak" } }) },
  { name: "revoked-with-idempotency-key-stripped", text: stringify(withRel(revoked, { idempotencyKey: "kept?" })) },
  { name: "disconnecting-with-transport", text: states.disconnecting.text! },
  { name: "disconnecting-without-transport", text: stringify({ ...JSON.parse(states.disconnecting.text!), transport: undefined }) },
];

const loadResults: unknown[] = [];
for (const testCase of loadCases) {
  const home = newHome();
  writeFileSync(file(home), testCase.text);
  const remote = new ScriptedRemote();
  remote.enrollMode = "offline";
  const controller = controllerFor(home, remote);
  const statusAtConstruction = controller.status();
  await controller.start();
  const quarantined = readdirSync(home).filter((n) => n.startsWith("hub-relationship.invalid-"));
  loadResults.push({
    name: testCase.name,
    text: testCase.text,
    fileAfterLoad: read(home),
    quarantined: quarantined.length,
    quarantinedText: quarantined.length ? readFileSync(path.join(home, quarantined[0]), "utf8") : null,
    statusAtConstruction,
    statusAfterStart: controller.status(),
    enrollAttempts: remote.enrollments.length,
    socketAttempts: remote.sockets.length,
  });
}

mkdirSync(outDir, { recursive: true });
writeFileSync(
  path.join(outDir, "hub-relationship-states.json"),
  JSON.stringify({ now: NOW, hubUrl: "https://hub.test/", hostname: "fixture-host", serverId: "server-fixture", daemonPublicKey: "public-key-fixture", ceremony, states }, null, 2) + "\n",
);
writeFileSync(path.join(outDir, "hub-relationship-load.json"), JSON.stringify(loadResults, null, 2) + "\n");
console.log(`wrote ${Object.keys(states).length} states and ${loadResults.length} load cases to ${outDir}`);
process.exit(0);
