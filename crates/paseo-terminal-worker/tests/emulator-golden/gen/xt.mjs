import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
const WT = process.env.PASEO_TS_CHECKOUT ?? process.cwd();
const require = createRequire(WT + "/packages/server/package.json");
const { Terminal } = require("@xterm/headless");
export const E = "\x1b";
process.on("uncaughtException", () => {});
const withTimeout = (fn) => Promise.race([new Promise(fn), new Promise((_, rej) => setTimeout(() => rej(new Error("xterm hang")), 500))]);
function cellOf(cell) {
  const fgMode = cell.getFgColorMode() >> 24, bgMode = cell.getBgColorMode() >> 24;
  return { char: cell.getWidth() === 0 ? "" : cell.getChars() || " ", fg: fgMode !== 0 ? cell.getFgColor() : undefined, bg: bgMode !== 0 ? cell.getBgColor() : undefined, fgMode: fgMode !== 0 ? fgMode : undefined, bgMode: bgMode !== 0 ? bgMode : undefined, bold: cell.isBold() !== 0, italic: cell.isItalic() !== 0, underline: cell.isUnderline() !== 0, dim: cell.isDim() !== 0, inverse: cell.isInverse() !== 0, strikethrough: cell.isStrikethrough() !== 0 };
}
const ex = (t, row, col) => { const line = t.buffer.active.getLine(row); if (!line) return { char: " " }; const c = line.getCell(col); return c ? cellOf(c) : { char: " " }; };
function state(t, title, o) {
  const b = t.buffer.active, base = b.baseY, n = base;
  const start = typeof o.scrollbackLines === "number" ? Math.max(0, n - o.scrollbackLines) : 0;
  const grid = [], sb = [], gw = [], sw = [];
  for (let r = 0; r < t.rows; r++) { const row = []; for (let c = 0; c < t.cols; c++) row.push(ex(t, base + r, c)); grid.push(row); gw.push(b.getLine(base + r + 1)?.isWrapped === true); }
  for (let r = start; r < n; r++) { const row = []; for (let c = 0; c < t.cols; c++) row.push(ex(t, r, c)); sb.push(row); sw.push(b.getLine(r + 1)?.isWrapped === true); }
  const cs = t._core?.coreService; const st = cs?.decPrivateModes?.cursorStyle; const ns = st === "block" || st === "underline" || st === "bar" ? st : undefined;
  const bl = typeof cs?.decPrivateModes?.cursorBlink === "boolean" ? cs.decPrivateModes.cursorBlink : undefined; const hidden = Boolean(cs?.isCursorHidden);
  return JSON.parse(JSON.stringify({ rows: t.rows, cols: t.cols, grid, scrollback: sb, cursor: { row: b.cursorY, col: b.cursorX, ...(hidden ? { hidden: true } : {}), ...(ns ? { style: ns } : {}), ...(typeof bl === "boolean" ? { blink: bl } : {}) }, ...(title ? { title } : {}), ...(o.includeWrapFlags ? { gridWrapped: gw, scrollbackWrapped: sw } : {}) }));
}
export async function xtermSnapshots(scn) {
  try { return await xtermSnapshotsRaw(scn); } catch (e) { return null; }
}
async function xtermSnapshotsRaw(scn) {
  const t = new Terminal({ rows: scn.rows, cols: scn.cols, scrollback: scn.scrollback, allowProposedApi: true });
  let title; t.onTitleChange((n) => { title = n.trim().length > 0 ? n : undefined; });
  const out = [];
  for (const s of scn.steps) {
    if (s.feed !== undefined) await withTimeout((r) => t.write(new Uint8Array(Buffer.from(s.feed, "base64")), r));
    else if (s.resize) t.resize(s.resize[1], s.resize[0]);
    else if (s.snapshot) out.push(state(t, title, s.snapshot));
  }
  t.dispose?.();
  return out;
}
// Differential runner: `cargo build --release` in ./emudiff, or point EMUDIFF at the binary.
const EMUDIFF = process.env.EMUDIFF ?? new URL("./emudiff/target/release/emudiff", import.meta.url).pathname;
export const rustAvailable = () => fs.existsSync(EMUDIFF);
export function rustSnapshots(scns) {
  const bin = EMUDIFF;
  const out = execFileSync(bin, { input: scns.map((s) => JSON.stringify(s)).join("\n") + "\n", maxBuffer: 1 << 30 }).toString().trim().split("\n");
  return out.map((l) => JSON.parse(l));
}
export function firstDiff(a, b, path = "$") {
  if (typeof a !== typeof b || (a === null) !== (b === null)) return `${path}: ${JSON.stringify(a)} vs ${JSON.stringify(b)}`;
  if (Array.isArray(a)) { if (a.length !== b.length) return `${path}: len ${a.length} vs ${b.length}`; for (let i = 0; i < a.length; i++) { const d = firstDiff(a[i], b[i], `${path}[${i}]`); if (d) return d; } return null; }
  if (a && typeof a === "object") { const keys = new Set([...Object.keys(a), ...Object.keys(b)]); for (const k of [...keys].sort()) { const d = firstDiff(a[k], b[k], `${path}.${k}`); if (d) return d; } return null; }
  return a === b ? null : `${path}: ${JSON.stringify(a)} vs ${JSON.stringify(b)}`;
}
// A scenario here: {rows, cols, scrollback, steps:[{feed:b64}|{resize:[r,c]}|{snapshot:opts}]}
export async function compare(scns) {
  const rust = rustSnapshots(scns);
  const res = [];
  for (let i = 0; i < scns.length; i++) { const x = await xtermSnapshots(scns[i]); res.push(x === null ? null : firstDiff(rust[i], x)); }
  return res;
}
export const feed = (str) => ({ feed: Buffer.from(str, "utf8").toString("base64") });
