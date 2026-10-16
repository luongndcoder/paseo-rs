// TS side of the projection microbench. Run from packages/server of the integration worktree:
//   TS_SERVER_DIR=$PWD npx tsx <this file>
import { readFileSync } from "node:fs";
import path from "node:path";
const dir = process.env.TS_SERVER_DIR!;
const { projectTimelineRows } = await import(path.join(dir, "src/server/agent/timeline-projection.ts"));
const rows = JSON.parse(readFileSync(path.join(import.meta.dirname, "timeline-300.json"), "utf8"));
const n = 20000;
for (let i = 0; i < 2000; i++) projectTimelineRows({ rows, mode: "projected" });
const samples: number[] = [];
let projected = 0;
for (let k = 0; k < 5; k++) {
  const t = performance.now();
  for (let i = 0; i < n; i++) projected = projectTimelineRows({ rows, mode: "projected" }).length;
  samples.push(((performance.now() - t) * 1000) / n);
}
samples.sort((a, b) => a - b);
console.log(JSON.stringify({ impl: "ts", rows: rows.length, projected, us_per_call_median: +samples[2].toFixed(2) }));
