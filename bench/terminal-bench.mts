// Terminal worker benchmark: Node worker vs Rust worker.
// Run (from packages/server of the integration worktree):
//   TS_SERVER_DIR=$PWD [PASEO_TERMINAL_WORKER=rust PASEO_TERMINAL_WORKER_BIN=...] npx tsx <this file>
// Prints one JSON object.
import { execSync } from "node:child_process";
import { mkdtempSync, writeFileSync, existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { performance } from "node:perf_hooks";

const dir = process.env.TS_SERVER_DIR!;
const { createWorkerTerminalManager, terminateWorkerTerminalManager } = await import(
  path.join(dir, "src/terminal/worker-terminal-manager.ts")
);
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const pct = (a: number[], p: number) => {
  const s = [...a].sort((x, y) => x - y);
  return s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))];
};
const rssKb = (pid: number) => Number(execSync(`ps -o rss= -p ${pid}`).toString().trim() || 0);
function workerPid(): number {
  const out = execSync(`pgrep -P ${process.pid} || true`).toString().trim().split("\n").filter(Boolean);
  for (const p of out) {
    const cmd = execSync(`ps -o command= -p ${p}`).toString();
    if (/paseo-terminal-worker|terminal-worker-process/.test(cmd)) return Number(p);
  }
  throw new Error("worker pid not found: " + out.join(","));
}
const tmp = mkdtempSync(path.join(os.tmpdir(), "tbench-"));
const home = mkdtempSync(path.join(os.tmpdir(), "tbench-home-"));
process.env.PASEO_HOME = home;
const bigFile = path.join(tmp, "big.txt");
const BIG_MB = Number(process.env.BIG_MB ?? 64);
execSync(`base64 -i /dev/urandom | head -c ${BIG_MB * 1024 * 1024} > ${bigFile}`, { shell: "/bin/bash" });

const t0 = performance.now();
const manager = createWorkerTerminalManager({});
const echo = await manager.createTerminal({ cwd: tmp, workspaceId: "w", command: "/bin/cat", args: [] });
const coldStartMs = performance.now() - t0;
await sleep(500);
const wpid = workerPid();
const rssIdleKb = rssKb(wpid);

// echo latency
let resolveNext: (() => void) | null = null;
echo.subscribe((m: any) => {
  if (m.type === "output" && resolveNext) {
    const r = resolveNext;
    resolveNext = null;
    r();
  }
});
const lat: number[] = [];
for (let i = 0; i < 700; i++) {
  const p = new Promise<void>((r) => (resolveNext = r));
  const s = performance.now();
  echo.send({ type: "input", data: "a" });
  await p;
  if (i >= 100) lat.push(performance.now() - s);
  await sleep(10);
}

// throughput
const thr = await manager.createTerminal({
  cwd: tmp, workspaceId: "w", command: "/bin/sh", args: ["-c", `sleep 0.4; cat ${bigFile}; exec cat`],
});
let bytes = 0, tFirst = 0, tLast = 0;
thr.subscribe((m: any) => {
  if (m.type === "output") {
    if (!tFirst) tFirst = performance.now();
    bytes += Buffer.byteLength(m.data);
    tLast = performance.now();
  }
});
while (!tLast || performance.now() - tLast < 800) await sleep(100);
const throughputMBps = bytes / 1048576 / ((tLast - tFirst) / 1000);

// snapshot time (terminal holds the big output in scrollback)
const snaps: number[] = [];
for (let i = 0; i < 7; i++) {
  const s = performance.now();
  await manager.getTerminalState(thr.id);
  snaps.push(performance.now() - s);
}
// snapshot of the small interactive terminal
const snapsSmall: number[] = [];
for (let i = 0; i < 7; i++) {
  const s = performance.now();
  await manager.getTerminalState(echo.id);
  snapsSmall.push(performance.now() - s);
}
const rssAfterBigKb = rssKb(wpid);

// 10 terminals
const many = [];
for (let i = 0; i < 10; i++) many.push(await manager.createTerminal({ cwd: tmp, workspaceId: "w", command: "/bin/cat", args: [] }));
await sleep(500);
const rss10Kb = rssKb(wpid);
const med = (a: number[]) => pct(a, 50);
console.log(JSON.stringify({
  worker: process.env.PASEO_TERMINAL_WORKER === "rust" ? "rust" : "node",
  coldStartMs, echoP50: pct(lat, 50), echoP99: pct(lat, 99), echoMax: Math.max(...lat),
  throughputMBps, bytes, snapshotBigMs: med(snaps), snapshotSmallMs: med(snapsSmall),
  rssIdleMB: rssIdleKb / 1024, rssAfterBigMB: rssAfterBigKb / 1024, rss11TermsMB: rss10Kb / 1024,
}));
try { terminateWorkerTerminalManager(manager); } catch {}
manager.killAll();
await sleep(300);
process.exit(0);
