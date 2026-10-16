import { compare, feed, E } from "./xt.mjs";
import fs from "node:fs";
function rng(seed) { let a = seed >>> 0; return () => { a = (a + 0x6d2b79f5) >>> 0; let t = a; t = Math.imul(t ^ (t >>> 15), t | 1); t ^= t + Math.imul(t ^ (t >>> 7), t | 61); return ((t ^ (t >>> 14)) >>> 0) / 4294967296; }; }
const SETS = {
  text: (r, rows, cols, pick, int) => [() => pick(["abc", "hello", "x", "0123456789", "zz", " ", "  "]), () => "x".repeat(int(cols * 2)), () => "\r\n", () => "\r\n", () => "\n", () => "\r", () => "\t", () => "\b"],
  cursor: (r, rows, cols, pick, int) => [() => "abc\r\n", () => `${E}[${1 + int(rows)};${1 + int(cols)}H`, () => `${E}[${int(4)}${pick(["A", "B", "C", "D"])}`, () => `${E}M`, () => `${E}7`, () => `${E}8`, () => `${E}[${int(cols)}G`, () => `${E}[${1 + int(rows)}d`],
  erase: (r, rows, cols, pick, int) => [() => "abcdefghij\r\n", () => `${E}[${pick(["K", "1K", "2K", "J", "1J", "2J", "L", "M", "P", "@", "X", "3X", "2P", "2@", "2L", "2M"])}`, () => `${E}[${1 + int(rows)};${1 + int(cols)}H`],
  sgr: (r, rows, cols, pick, int) => [() => "abc ", () => `${E}[${pick(["0", "1", "2", "3", "4", "7", "8", "9", "21", "22", "23", "24", "27", "29", "31", "42", "91", "100", "38;5;" + int(256), "48;5;" + int(256), "38;2;1;2;3", "48;2;" + int(256) + ";1;2", "39", "49", "1;31;44"])}m`, () => "\r\n"],
  wide: (r, rows, cols, pick, int) => [() => pick(["日本", "é", "😀", "wide日", "á", "한국", "👍🏽", "🇻🇳", "✅", "❌", "你好", "‍", "️"]), () => "ab", () => "\r\n", () => `${E}[${1 + int(rows)};${1 + int(cols)}H`, () => `${E}[2C`, () => `${E}[2D`],
  modes: (r, rows, cols, pick, int) => [() => "abcabcabcabcabc", () => `${E}[?7${pick(["h", "l"])}`, () => `${E}[4${pick(["h", "l"])}`, () => `${E}[?6${pick(["h", "l"])}`, () => `${E}[?25${pick(["h", "l"])}`, () => "\r\n", () => `${E}[${1 + int(rows)};${1 + int(cols)}H`, () => `${E}[${int(5)} q`],
  region: (r, rows, cols, pick, int) => [() => "line\r\n", () => `${E}[${1 + int(rows)};${1 + int(rows)}r`, () => `${E}[r`, () => `${E}M`, () => `${E}D`, () => `${E}E`, () => `${E}[${1 + int(3)}S`, () => `${E}[${1 + int(3)}T`, () => "\r\n"],
  alt: (r, rows, cols, pick, int) => [() => "main text\r\nmore\r\n", () => `${E}[?1049${pick(["h", "l"])}`, () => `${E}[?47${pick(["h", "l"])}`, () => `${E}[?1047${pick(["h", "l"])}`, () => `${E}[${1 + int(rows)};${1 + int(cols)}H`, () => "alt\r\n", () => `${E}7`, () => `${E}8`],
  resize: (r, rows, cols, pick, int) => [() => pick(["abc", "hello world", "x".repeat(int(cols * 3)), "日本語"]), () => "\r\n", () => "\r\n", () => `${E}[${1 + int(rows)};${1 + int(cols)}H`],
};
export function gen(seed, setNames, withResize = false) {
  const r = rng(seed); const pick = (a) => a[Math.floor(r() * a.length)]; const int = (n) => Math.floor(r() * n);
  const rows = 3 + int(5), cols = 6 + int(12), scrollback = pick([5, 20, 1000]);
  const pieces = setNames.flatMap((n) => SETS[n](r, rows, cols, pick, int));
  const steps = []; const n = 3 + int(8);
  for (let i = 0; i < n; i++) {
    if (withResize && r() < 0.25) steps.push({ resize: [Math.max(2, rows + int(7) - 3), Math.max(3, cols + int(11) - 5)] });
    else { let s = ""; const m = 1 + int(6); for (let j = 0; j < m; j++) s += pieces[int(pieces.length)](); steps.push(feed(s)); }
  }
  steps.push({ snapshot: { includeWrapFlags: true } });
  return { rows, cols, scrollback, steps };
}
// token-level minimizer
function tokens(s) { return s.match(/\x1b\[[0-9;?]*[ -\/]*[@-~]|\x1b\][^\x07]*\x07|\x1b[0-9A-Za-z]|[\s\S]/gu) ?? []; }
async function stillFails(scn) { const [d] = await compare([scn]); return d; }
export async function minimize(scn) {
  let cur = JSON.parse(JSON.stringify(scn));
  let diff = await stillFails(cur); if (!diff) return null;
  // drop whole steps
  for (let i = cur.steps.length - 2; i >= 0; i--) { const t = JSON.parse(JSON.stringify(cur)); t.steps.splice(i, 1); const d = await stillFails(t); if (d) { cur = t; diff = d; } }
  // merge feeds
  // drop tokens
  for (let i = 0; i < cur.steps.length; i++) {
    if (cur.steps[i].feed === undefined) continue;
    let toks = tokens(Buffer.from(cur.steps[i].feed, "base64").toString());
    for (let j = toks.length - 1; j >= 0; j--) { const t2 = toks.slice(0, j).concat(toks.slice(j + 1)); const t = JSON.parse(JSON.stringify(cur)); t.steps[i] = feed(t2.join("")); const d = await stillFails(t); if (d) { toks = t2; cur = t; diff = d; } }
  }
  // shrink dims
  for (const dim of ["rows", "cols"]) { while (cur[dim] > 3) { const t = JSON.parse(JSON.stringify(cur)); t[dim]--; const d = await stillFails(t); if (d) { cur = t; diff = d; } else break; } }
  return { cur, diff };
}
const esc = (t) => JSON.stringify(t).replace(/[^\x20-\x7e]/gu, (c) => c.codePointAt(0) > 0xffff ? `\\u{${c.codePointAt(0).toString(16)}}` : `\\u${c.charCodeAt(0).toString(16).padStart(4, "0")}`);
export function pretty(scn) { return `[${scn.rows}x${scn.cols} sb${scn.scrollback}] ` + scn.steps.map((s) => s.feed ? esc(Buffer.from(s.feed, "base64").toString()) : s.resize ? "RESIZE" + JSON.stringify(s.resize) : "SNAP").join(" ; "); }
if (process.argv[1].endsWith("fuzz.mjs")) {
  const sets = process.argv[2].split(","); const count = Number(process.argv[3] ?? 200); const withResize = process.argv[4] === "resize";
  const scns = []; for (let s = 1; s <= count; s++) scns.push(gen(s * 7919, sets, withResize));
  const res = await compare(scns);
  const bad = scns.map((s, i) => [s, res[i]]).filter(([, d]) => d);
  console.log(`${sets} resize=${withResize}: ${bad.length}/${count} diverge`);
  const seen = new Map();
  for (const [s, d] of bad.slice(0, Number(process.argv[5] ?? 6))) { const m = await minimize(s); if (m) console.log("  ", pretty(m.cur), "\n      =>", m.diff); }
}
