// Generates golden JSON for the Rust emulator from @xterm/headless + terminal.ts snapshot logic (reimplemented identically).
import { createRequire } from "node:module";
import fs from "node:fs";
import path from "node:path";
// Checkout that has `@xterm/headless` and `strip-ansi` in node_modules (the TS reference).
const WT = process.env.PASEO_TS_CHECKOUT ?? process.cwd();
const require = createRequire(WT + "/packages/server/package.json");
const xterm = require("@xterm/headless");
const stripAnsi = (await import(WT + "/packages/server/node_modules/strip-ansi/index.js")).default;
const { Terminal } = xterm;
const OUT = new URL("../", import.meta.url).pathname;
fs.mkdirSync(OUT, { recursive: true });

// ---- verbatim from terminal.ts ----
function cellOf(cell) {
  const fgMode = cell.getFgColorMode() >> 24;
  const bgMode = cell.getBgColorMode() >> 24;
  return {
    char: cell.getWidth() === 0 ? "" : cell.getChars() || " ",
    fg: fgMode !== 0 ? cell.getFgColor() : undefined,
    bg: bgMode !== 0 ? cell.getBgColor() : undefined,
    fgMode: fgMode !== 0 ? fgMode : undefined,
    bgMode: bgMode !== 0 ? bgMode : undefined,
    bold: cell.isBold() !== 0, italic: cell.isItalic() !== 0, underline: cell.isUnderline() !== 0,
    dim: cell.isDim() !== 0, inverse: cell.isInverse() !== 0, strikethrough: cell.isStrikethrough() !== 0,
  };
}
function extractCell(t, row, col) {
  const line = t.buffer.active.getLine(row);
  if (!line) return { char: " ", fg: undefined, bg: undefined };
  const cell = line.getCell(col);
  if (!cell) return { char: " ", fg: undefined, bg: undefined };
  return cellOf(cell);
}
const extractGrid = (t) => {
  const g = []; const baseY = t.buffer.active.baseY;
  for (let r = 0; r < t.rows; r++) { const row = []; for (let c = 0; c < t.cols; c++) row.push(extractCell(t, baseY + r, c)); g.push(row); }
  return g;
};
const extractScrollback = (t, o) => {
  const sb = []; const n = t.buffer.active.baseY;
  const start = typeof o?.scrollbackLines === "number" ? Math.max(0, n - o.scrollbackLines) : 0;
  for (let r = start; r < n; r++) { const row = []; for (let c = 0; c < t.cols; c++) row.push(extractCell(t, r, c)); sb.push(row); }
  return sb;
};
const cont = (t, r) => t.buffer.active.getLine(r + 1)?.isWrapped === true;
const gridWrapped = (t) => { const b = t.buffer.active.baseY; const w = []; for (let r = 0; r < t.rows; r++) w.push(cont(t, b + r)); return w; };
const sbWrapped = (t, o) => {
  const n = t.buffer.active.baseY;
  const start = typeof o?.scrollbackLines === "number" ? Math.max(0, n - o.scrollbackLines) : 0;
  const w = []; for (let r = start; r < n; r++) w.push(cont(t, r)); return w;
};
function cursorState(t) {
  const cs = t._core?.coreService;
  const style = cs?.decPrivateModes?.cursorStyle;
  const ns = style === "block" || style === "underline" || style === "bar" ? style : undefined;
  const blink = typeof cs?.decPrivateModes?.cursorBlink === "boolean" ? cs.decPrivateModes.cursorBlink : undefined;
  const hidden = Boolean(cs?.isCursorHidden);
  return { row: t.buffer.active.cursorY, col: t.buffer.active.cursorX, ...(hidden ? { hidden: true } : {}), ...(ns ? { style: ns } : {}), ...(typeof blink === "boolean" ? { blink } : {}) };
}
function getState(t, title, o) {
  return {
    rows: t.rows, cols: t.cols, grid: extractGrid(t),
    scrollback: extractScrollback(t, { scrollbackLines: o?.scrollbackLines }),
    cursor: cursorState(t),
    ...(title ? { title } : {}),
    ...(o?.includeWrapFlags ? { gridWrapped: gridWrapped(t), scrollbackWrapped: sbWrapped(t, { scrollbackLines: o?.scrollbackLines }) } : {}),
  };
}
// ---- verbatim from terminal-capture.ts ----
const cellsToPlain = (cells, so) => { const text = cells.map((c) => c.char).join("").trimEnd(); return so ? stripAnsi(text) : text; };
function resolveIdx(n, total, fb) {
  if (total === 0) return fb === "start" ? 0 : -1;
  const d = fb === "start" ? 0 : total - 1;
  if (typeof n !== "number") return d;
  const i = n < 0 ? total + n : n;
  if (i < 0) return 0; if (i >= total) return total - 1; return i;
}
function capture(state, o = {}) {
  const all = [...state.scrollback, ...state.grid].map((c) => cellsToPlain(c, o.stripAnsi ?? true));
  const total = all.length; const s = resolveIdx(o.start, total, "start"); const e = resolveIdx(o.end, total, "end");
  if (total === 0 || s > e) return { lines: [], totalLines: total };
  return { lines: all.slice(s, e + 1), totalLines: total };
}

// Cells omit boolean attributes that are false (tests normalize both sides the same way).
function stripFalse(v) { if (Array.isArray(v)) return v.map(stripFalse); if (v && typeof v === "object") { const o = {}; for (const [k, x] of Object.entries(v)) { if (x === false && ["bold", "italic", "underline", "dim", "inverse", "strikethrough"].includes(k)) continue; o[k] = stripFalse(x); } return o; } return v; }

async function run(name, { rows = 6, cols = 20, scrollback = 1000, steps }) {
  const t = new Terminal({ rows, cols, scrollback, allowProposedApi: true });
  let title;
  t.onTitleChange((n) => { title = n.trim().length > 0 ? n : undefined; });
  const out = [];
  for (const s of steps) {
    if (typeof s === "string" || s.feed !== undefined) {
      const str = typeof s === "string" ? s : s.feed;
      const buf = Buffer.from(str, "utf8");
      await new Promise((r) => t.write(new Uint8Array(buf), r));
      out.push({ feed: buf.toString("base64") });
    } else if (s.feedHex) {
      const buf = Buffer.from(s.feedHex, "hex");
      await new Promise((r) => t.write(new Uint8Array(buf), r));
      out.push({ feed: buf.toString("base64") });
    } else if (s.resize) {
      t.resize(s.resize[1], s.resize[0]);
      out.push({ resize: s.resize });
    } else if (s.snapshot) {
      const o = s.snapshot === true ? {} : s.snapshot;
      const st = getState(t, title, o);
      out.push({ snapshot: o, expected: JSON.parse(JSON.stringify(st)) });
    } else if (s.capture) {
      const o = s.capture === true ? {} : s.capture;
      out.push({ capture: o, expected: capture(getState(t, title, {}), o) });
    }
  }
  fs.writeFileSync(path.join(OUT, name + ".json"), JSON.stringify(stripFalse({ name, rows, cols, scrollback, steps: out })) + "\n");
  console.log("wrote", name, out.length);
}
const E = "\x1b";
const S = { snapshot: true }, SW = { snapshot: { includeWrapFlags: true } };
const scenarios = {
  plain_text: { steps: ["hello world\r\nsecond line", S, { capture: true }, { capture: { start: -2, end: -1 } }, { capture: { start: 1, end: 0 } }] },
  empty: { steps: [S, { capture: true }] },
  colors_16: { steps: [`${E}[31mred${E}[0m ${E}[91mbrred${E}[0m ${E}[42mgbg${E}[0m ${E}[107mwbg${E}[0m${E}[30;47mblk/wht${E}[0m`, S] },
  colors_256: { steps: [`${E}[38;5;1mlow${E}[0m ${E}[38;5;200mpink${E}[0m ${E}[48;5;17mnavybg${E}[0m ${E}[38;5;255;48;5;0mgrey${E}[0m`, S] },
  colors_truecolor: { steps: [`${E}[38;2;1;2;3mrgb${E}[0m ${E}[48;2;255;128;0mobg${E}[0m ${E}[38;2;0;0;0;48;2;255;255;255mbw${E}[0m ${E}[38:2::10:20:30mcolon${E}[0m`, S] },
  attributes: { steps: [`${E}[1mbold${E}[22m ${E}[2mdim${E}[22m ${E}[3mital${E}[23m ${E}[4mund${E}[24m ${E}[7minv${E}[27m ${E}[9mstr${E}[29m\r\n${E}[1;3;4;7;9;31mall${E}[0m plain ${E}[1;2mbd${E}[22mx${E}[4:3mcurly${E}[0m ${E}[21mdbl${E}[0m`, S] },
  color_bg_erase: { steps: [`${E}[44mabc${E}[K\r\n${E}[41m${E}[2J${E}[Hx`, S] },
  cursor_movement: { steps: [`line1\r\nline2\r\nline3${E}[2A${E}[3CX${E}[1B${E}[2DY${E}[H${E}[2;5HZ${E}7${E}[5;1Hbottom${E}8Q${E}[3;1H${E}[1Kcleared\tTab\tTab2${E}[4;3H${E}[0J`, S] },
  insert_delete: { steps: [`abcdefghij\r${E}[3C${E}[2@XY${E}[2P${E}[3X\r\n${E}[2L${E}[M`, S] },
  scroll_region: { rows: 6, steps: [`${E}[2;4r${E}[2;1H1\r\n2\r\n3\r\n4\r\n5${E}[r`, S] },
  wrap_and_crlf: { steps: ["a".repeat(45) + "\r\nshort\r\n" + "b".repeat(20) + "c\nlf-only", S, SW, { capture: true }] },
  wrap_scrolling: { rows: 4, cols: 10, steps: ["x".repeat(65) + "\r\nend", SW] },
  alt_screen: { steps: ["main1\r\nmain2", `${E}[?1049h${E}[Halt-content${E}[31mred${E}[0m`, S, { capture: true }, `${E}[?1049l`, S, SW] },
  alt_screen_1047: { steps: ["keep", `${E}[?47hjunk\r\nmore`, S, `${E}[?47l`, S] },
  wide_cjk: { steps: ["日本語 text 中\r\n", `${E}[1m한국어${E}[0m`, S, SW] },
  wide_emoji: { steps: ["😀 smile 👍🏽 flag🇻🇳 end\r\né combining\r\n", "ab😀".padEnd(3), S] },
  wide_at_edge: { rows: 4, cols: 10, steps: ["123456789日本", S, SW] },
  wide_overwrite: { steps: ["日本語\r后\x1b[1C\x1b[1Dx", S] },
  scrollback_overflow: { rows: 5, cols: 20, scrollback: 10, steps: [Array.from({ length: 40 }, (_, i) => `line ${i}`).join("\r\n"), S, { snapshot: { scrollbackLines: 3 } }, { snapshot: { scrollbackLines: 0 } }, SW, { capture: true }, { capture: { start: 0, end: 2 } }, { capture: { start: -3 } }, { capture: { start: 100, end: 200 } }, { capture: { start: -100, end: 1 } }] },
  scrollback_default_1000: { rows: 5, cols: 12, steps: [Array.from({ length: 1100 }, (_, i) => `l${i}`).join("\r\n"), { capture: { start: 0, end: 2 } }, { capture: { start: -2 } }] },
  resize_grow_cols: { rows: 5, cols: 10, steps: ["a".repeat(25) + "\r\nshort", SW, { resize: [5, 30] }, SW] },
  resize_shrink_cols: { rows: 5, cols: 20, steps: ["abcdefghijklmnopqrst\r\n0123456789", { resize: [5, 8] }, SW] },
  resize_shrink_rows: { rows: 6, cols: 10, steps: ["1\r\n2\r\n3\r\n4\r\n5\r\n6", { resize: [3, 10] }, SW] },
  resize_grow_rows: { rows: 3, cols: 10, steps: ["1\r\n2\r\n3\r\n4\r\n5", { resize: [6, 10] }, SW] },
  resize_cursor_mid: { rows: 5, cols: 10, steps: ["one\r\ntwo", `${E}[1;3H`, { resize: [4, 6] }, S] },
  cursor_styles: { steps: [`${E}[5 q`, S, `${E}[2 q`, S, `${E}[3 q`, S, `${E}[6 q`, S, `${E}[0 q`, S, `${E}[4 q${E}[?25l`, S, `${E}[?25h${E}[1 q`, S, `${E}[?12h`, S, `${E}[?12l`, S] },
  title: { steps: [`${E}]0;my title\x07hi`, S, `${E}]2;second${E}\\`, S, `${E}]0;\x07`, S, `${E}]1;icon\x07`, S] },
  tabs_and_ctl: { steps: ["a\tb\x08c\x07d\r\n\x0bvt\x0cff\x00nul", S] },
  utf8_split: { steps: [{ feedHex: "e697" }, { feedHex: "a5e69c" }, { feedHex: "ac" }, S] },
  capture_strip_ansi: { steps: ["plain  \r\n\x1b[31mred\x1b[0m\r\n", { capture: { stripAnsi: false } }, { capture: { stripAnsi: true } }] },
  reverse_index_scroll: { rows: 4, steps: ["1\r\n2\r\n3\r\n4", `${E}[H${E}M${E}M`, S] },
  erase_and_decaln: { rows: 3, cols: 5, steps: [`${E}#8`, S, `${E}c`, S] },
  autowrap_off: { cols: 10, steps: [`${E}[?7lAAAAAAAAAAAAAAAA${E}[?7hB`, S, SW] },
  insert_mode_origin: { steps: [`abc${E}[4h${E}[1GXY${E}[4l${E}[?6h${E}[2;4r${E}[1;1Ho`, S] },
  sgr_bright_bold: { steps: [`${E}[1;31mboldred${E}[0m ${E}[1;91mboldbright${E}[0m ${E}[2;38;5;9mdim9${E}[0m ${E}[38;5;7m${E}[7minvc${E}[0m`, S] },
  pending_wrap_cursor: { rows: 4, cols: 10, steps: ["0123456789", S, SW, "a", S] },
  resize_shrink_cols_cursor_below: { rows: 5, cols: 20, steps: ["abcdefghijklmnopqrst\r\n0123456789\r\n", { resize: [5, 8] }, SW] },
  resize_cursor_in_wrapped_chain: { rows: 6, cols: 10, steps: ["x".repeat(25) + "\r\nrow2\r\n" + "y".repeat(23), `${E}[2;3H`, { resize: [6, 6] }, SW] },
  resize_cursor_on_long_line_mid: { rows: 6, cols: 12, steps: ["line1\r\n" + "z".repeat(30), `${E}[1;1H`, { resize: [6, 7] }, SW] },
  resize_shrink_with_history: { rows: 4, cols: 12, scrollback: 50, steps: [Array.from({ length: 10 }, (_, i) => `row${i}-` + "w".repeat(i)).join("\r\n") + "\r\nend", { resize: [4, 5] }, SW] },
  resize_grow_with_history: { rows: 4, cols: 5, scrollback: 50, steps: [Array.from({ length: 10 }, (_, i) => `row${i}-` + "w".repeat(i)).join("\r\n") + "\r\nend", { resize: [4, 14] }, SW] },
  resize_both: { rows: 5, cols: 10, steps: ["aaaaaaaaaaaaaaa\r\nbb\r\ncc", { resize: [3, 6] }, SW, { resize: [8, 12] }, SW] },
  resize_alt_screen: { rows: 4, cols: 10, steps: ["main\r\nline", `${E}[?1049hALT`, { resize: [6, 6] }, S, `${E}[?1049l`, SW] },
  wide_resize: { rows: 4, cols: 10, steps: ["日本語日本語\r\nx", { resize: [4, 5] }, SW] },
  bce_scroll: { rows: 3, cols: 6, steps: [`${E}[44m1\r\n2\r\n3\r\n4\r\n${E}[0m5`, S] },
  erase_above_row1: { rows: 4, cols: 6, steps: ["aaaaaa\r\nbbbbbb\r\ncccccc", `${E}[2;3H${E}[1J`, S] },
  dec_graphics: { steps: [`${E}(0lqkx${E}(Blqk\x0elqk\x0f`, S] },
};

// ---- seeded fuzz scenarios (deterministic) ----
function rng(seed) { let a = seed >>> 0; return () => { a = (a + 0x6d2b79f5) >>> 0; let t = a; t = Math.imul(t ^ (t >>> 15), t | 1); t ^= t + Math.imul(t ^ (t >>> 7), t | 61); return ((t ^ (t >>> 14)) >>> 0) / 4294967296; }; }
function fuzzScenario(seed) {
  const r = rng(seed); const pick = (a) => a[Math.floor(r() * a.length)]; const int = (n) => Math.floor(r() * n);
  const rows = 3 + int(6), cols = 6 + int(14), scrollback = pick([5, 20, 1000]);
  const words = ["abc", "hello", "x", "  ", "日本", "é", "0123456789", "zz", "😀", "wide日", " "];
  const pieces = [
    () => pick(words), () => pick(words).repeat(1 + int(6)), () => "\r\n", () => "\r\n", () => "\n", () => "\r",
    () => `${E}[${1 + int(rows)};${1 + int(cols)}H`, () => `${E}[${int(4)}${pick(["A", "B", "C", "D"])}`,
    () => `${E}[${pick(["0", "1", "3", "4", "7", "9", "31", "42", "38;5;" + int(256), "48;2;" + int(256) + ";1;2", "91", "22", "24", "39", "49"])}m`,
    () => `${E}[${pick(["K", "1K", "2K", "J", "1J", "2J", "L", "M", "P", "@", "X"])}`, () => "\t", () => "\b",
    () => `${E}[?1049${pick(["h", "l"])}`, () => `${E}[?7${pick(["h", "l"])}`, () => `${E}M`, () => `${E}[${1 + int(rows)};${1 + int(rows)}r`,
  ];
  const steps = [];
  const n = 8 + int(14);
  for (let i = 0; i < n; i++) {
    const k = int(10);
    if (k < 6) { let chunk = ""; const m = 1 + int(8); for (let j = 0; j < m; j++) chunk += pieces[int(pieces.length)](); steps.push(chunk); }
    else if (k < 8) steps.push({ resize: [Math.max(2, rows + int(7) - 3), Math.max(3, cols + int(11) - 5)] });
    else steps.push(SW);
  }
  steps.push(SW, { capture: true });
  return { rows, cols, scrollback, steps };
}
for (let seed = 1; seed <= 60; seed++) scenarios["fuzz_" + String(seed).padStart(3, "0")] = fuzzScenario(seed);
for (const [n, sc] of Object.entries(scenarios)) await run(n, sc);

// ---- extra differential-fuzz derived goldens (only scenarios Rust already matches are kept) ----
import { gen as genStructured } from "./fuzz.mjs";
import { genRaw } from "./rawgen.mjs";
import { xtermSnapshots, rustSnapshots, firstDiff, rustAvailable } from "./xt.mjs";
async function extra(kind, make, count, firstSeed) {
  const cands = []; for (let s = firstSeed; cands.length < count * 3; s++) cands.push({ seed: s, scn: make(s) });
  const rust = rustAvailable() ? rustSnapshots(cands.map((c) => c.scn)) : null; let kept = 0;
  for (let i = 0; i < cands.length && kept < count; i++) {
    const { scn } = cands[i]; const xs = await xtermSnapshots(scn); if (!xs) continue;
    if (rust && xs.some((x, k) => firstDiff(rust[i][k], x))) continue;
    let k = 0; const steps = scn.steps.map((st) => (st.snapshot ? { snapshot: st.snapshot, expected: xs[k++] } : st));
    const name = `${kind}_${String(++kept).padStart(2, "0")}`;
    fs.writeFileSync(path.join(OUT, name + ".json"), JSON.stringify(stripFalse({ name, rows: scn.rows, cols: scn.cols, scrollback: scn.scrollback, steps })) + "\n");
  }
  console.log("wrote", kind, kept);
}
await extra("diff_text", (s) => genStructured(s * 31, ["text", "wide", "sgr", "cursor", "erase"], false), 14, 1);
await extra("diff_modes", (s) => genStructured(s * 37, ["modes", "region", "alt", "cursor", "text"], false), 14, 1);
await extra("diff_resize", (s) => genStructured(s * 41, ["text", "wide", "sgr", "cursor", "alt", "resize"], true), 14, 1);
await extra("diff_raw", (s) => genRaw(s * 43), 14, 1);
