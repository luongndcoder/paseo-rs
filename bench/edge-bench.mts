// Edge benchmark: in-process TS test daemon, direct vs through paseod (PASEO_EDGE=rust).
// Run from packages/server of the integration worktree:
//   TS_SERVER_DIR=$PWD [PASEO_EDGE=rust PASEO_EDGE_BIN=...] [PASEO_TERMINAL_WORKER=rust PASEO_TERMINAL_WORKER_BIN=...] npx tsx <this file>
import { execSync } from "node:child_process";
import { mkdtempSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { performance } from "node:perf_hooks";

const dir = process.env.TS_SERVER_DIR!;
const { createTestPaseoDaemon } = await import(path.join(dir, "src/server/test-utils/paseo-daemon.ts"));
const { DaemonClient } = await import(path.join(dir, "src/server/test-utils/daemon-client.ts"));
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const pct = (a: number[], p: number) => {
  const s = [...a].sort((x, y) => x - y);
  return s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))];
};
const rssMb = (pid: number) => Number(execSync(`ps -o rss= -p ${pid}`).toString().trim() || 0) / 1024;
const children = () =>
  execSync(`pgrep -P ${process.pid} || true`).toString().trim().split("\n").filter(Boolean).map((p) => ({
    pid: Number(p),
    cmd: execSync(`ps -o command= -p ${p}`).toString(),
  }));

const edge = process.env.PASEO_EDGE === "rust";
const t0 = performance.now();
const daemon = await createTestPaseoDaemon({});
const daemonStartMs = performance.now() - t0;
await sleep(1500);
const url = `ws://127.0.0.1:${daemon.port}/ws`;

// connection setup
const setup: number[] = [];
for (let i = 0; i < 40; i++) {
  const c = new DaemonClient({ url, reconnect: { enabled: false } });
  const s = performance.now();
  await c.connect();
  setup.push(performance.now() - s);
  await c.close();
}
const client = new DaemonClient({ url });
await client.connect();
await sleep(500);
const edgeChild = children().find((c) => /paseod/.test(c.cmd));
const workerChild = children().find((c) => /paseo-terminal-worker|terminal-worker-process/.test(c.cmd));
const rssIdle = {
  tsProcess: rssMb(process.pid),
  paseod: edgeChild ? rssMb(edgeChild.pid) : null,
  terminalWorker: workerChild ? rssMb(workerChild.pid) : null,
};

// ping rtt
const pings: number[] = [];
for (let i = 0; i < 600; i++) {
  const s = performance.now();
  await client.ping();
  if (i >= 100) pings.push(performance.now() - s);
}

// large JSON frame (client -> daemon, validated + handled; response is small)
const cwd = mkdtempSync(path.join(os.tmpdir(), "ebench-"));
async function frames(bytes: number, n: number) {
  const content = "x".repeat(bytes);
  const out: number[] = [];
  for (let i = 0; i < n; i++) {
    const s = performance.now();
    await client.writeFile({ cwd, path: "f.txt", content, expectedModifiedAt: new Date(0).toISOString() }).catch(() => {});
    if (i >= 20) out.push(performance.now() - s);
  }
  return out;
}
const res: Record<string, { p50: number; p99: number }> = {};
for (const [label, bytes] of [["1KiB", 1024], ["16KiB", 16384], ["256KiB", 262144]] as const) {
  const a = await frames(bytes, label === "256KiB" ? 220 : 520);
  res[label] = { p50: pct(a, 50), p99: pct(a, 99) };
}
res.ping = { p50: pct(pings, 50), p99: pct(pings, 99) };

// under load: 8 concurrent clients hammering 256KiB frames for 5s
const loaders = await Promise.all(Array.from({ length: 8 }, async () => { const c = new DaemonClient({ url }); await c.connect(); return c; }));
let stop = false, done = 0;
const content = "y".repeat(262144);
const loop = (c: any) => (async () => { while (!stop) { await c.writeFile({ cwd, path: "g.txt", content, expectedModifiedAt: new Date(0).toISOString() }).catch(() => {}); done++; } })();
const loads = loaders.map(loop);
let peakEdge = 0;
const t1 = performance.now();
const loadPings: number[] = [];
while (performance.now() - t1 < 5000) {
  if (edgeChild) peakEdge = Math.max(peakEdge, rssMb(edgeChild.pid));
  const s = performance.now();
  await client.ping();
  loadPings.push(performance.now() - s);
  await sleep(20);
}
stop = true;
await Promise.all(loads);
const loadSec = (performance.now() - t1) / 1000;
const rssLoad = { tsProcess: rssMb(process.pid), paseod: edgeChild ? rssMb(edgeChild.pid) : null, paseodPeak: edgeChild ? peakEdge : null };
console.log(JSON.stringify({
  mode: edge ? "edge" : "direct", terminalWorker: process.env.PASEO_TERMINAL_WORKER === "rust" ? "rust" : "node",
  daemonStartMs, connectP50: pct(setup, 50), connectP99: pct(setup, 99), frames: res,
  loadFramesPerSec: done / loadSec, pingUnderLoadP50: pct(loadPings, 50), pingUnderLoadP99: pct(loadPings, 99),
  rssIdle, rssLoad,
}));
for (const c of loaders) await c.close().catch(() => {});
await client.close().catch(() => {});
await daemon.close().catch(() => {});
process.exit(0);
