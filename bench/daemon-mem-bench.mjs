// Whole-daemon cold start + idle RSS. Usage (from packages/server of the integration worktree):
//   [PASEO_TERMINAL_WORKER=rust PASEO_TERMINAL_WORKER_BIN=..] [PASEO_EDGE=rust PASEO_EDGE_BIN=..] node bench/daemon-mem-bench.mjs
import { spawn, execSync } from "node:child_process";
import { mkdtempSync } from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const port = await new Promise((res) => { const s = net.createServer().listen(0, "127.0.0.1", () => { const p = s.address().port; s.close(() => res(p)); }); });
const home = mkdtempSync(path.join(os.tmpdir(), "dbench-"));
const t0 = performance.now();
const child = spawn(process.execPath, ["--import", "tsx", "scripts/supervisor-entrypoint.ts", "--dev"], {
  cwd: process.cwd(),
  env: { ...process.env, PASEO_HOME: home, PASEO_LISTEN: `127.0.0.1:${port}`, PASEO_RELAY_ENABLED: "false", PASEO_NODE_ENV: "development", PASEO_LOG_FORMAT: "json" },
  stdio: ["ignore", "ignore", "ignore"],
});
async function ready() {
  for (;;) {
    const ok = await new Promise((r) => { const c = net.connect(port, "127.0.0.1"); c.once("connect", () => { c.destroy(); r(true); }); c.once("error", () => r(false)); });
    if (ok) return performance.now() - t0;
    await sleep(10);
  }
}
const coldStartMs = await Promise.race([ready(), sleep(60000).then(() => -1)]);
await sleep(20000);
const ps = execSync("ps -A -o pid=,ppid=,rss=,command=").toString().trim().split("\n").map((l) => {
  const m = l.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s+(.*)$/); return { pid: +m[1], ppid: +m[2], rss: +m[3] / 1024, cmd: m[4] };
});
const tree = []; const q = [child.pid];
while (q.length) { const p = q.shift(); const me = ps.find((x) => x.pid === p); if (me) tree.push(me); for (const c of ps) if (c.ppid === p) q.push(c.pid); }
const label = (c) => /paseod/.test(c.cmd) ? "paseod" : /paseo-terminal-worker/.test(c.cmd) ? "rust-terminal-worker" : /terminal-worker-process/.test(c.cmd) ? "node-terminal-worker" : /plugin-process/.test(c.cmd) ? "plugin" : /Paseo Supervisor|supervisor-entrypoint/.test(c.cmd) ? "supervisor" : /Paseo Daemon/.test(c.cmd) ? "ts-daemon" : /esbuild/.test(c.cmd) ? "esbuild" : /packag/.test(c.cmd) ? "other-pkg" : /(worker|daemon)/.test(c.cmd) ? "daemon-worker" : "other";
console.log(JSON.stringify({
  variant: `${process.env.PASEO_EDGE === "rust" ? "edge+" : ""}${process.env.PASEO_TERMINAL_WORKER === "rust" ? "rust-term" : "node-term"}`,
  coldStartMs, totalRssMB: tree.reduce((a, c) => a + c.rss, 0),
  procs: tree.map((c) => ({ role: label(c), rssMB: +c.rss.toFixed(1), cmd: c.cmd.slice(0, 60) })),
}));
child.kill("SIGTERM"); await sleep(1500); try { child.kill("SIGKILL"); } catch {}
process.exit(0);
