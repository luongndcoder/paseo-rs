// TS side. TS_SERVER_DIR=$PWD npx tsx <this file> <repo>
import path from "node:path";
const dir = process.env.TS_SERVER_DIR!;
const { getCheckoutStatus } = await import(path.join(dir, "src/utils/checkout-git.ts"));
const repo = process.argv[2];
for (let i = 0; i < 3; i++) await getCheckoutStatus(repo);
const s: number[] = [];
for (let i = 0; i < 30; i++) { const t = performance.now(); await getCheckoutStatus(repo); s.push(performance.now() - t); }
s.sort((a, b) => a - b);
console.log(JSON.stringify({ impl: "ts", ms_p50: +s[15].toFixed(1), ms_p90: +s[27].toFixed(1) }));
