import { E } from "./xt.mjs";
function rng(seed) { let a = seed >>> 0; return () => { a = (a + 0x6d2b79f5) >>> 0; let t = a; t = Math.imul(t ^ (t >>> 15), t | 1); t ^= t + Math.imul(t ^ (t >>> 7), t | 61); return ((t ^ (t >>> 14)) >>> 0) / 4294967296; }; }
const ALPH = ["\x1b", "\x1b", "\x1b[", "\x1b[", "\x1b]", "\x1bP", "[", ";", ":", "?", ">", "!", " ", "\"", "$", "'", "0", "1", "2", "3", "4", "5", "9", "12", "38;5;", "38;2;", "m", "H", "J", "K", "A", "B", "C", "D", "G", "d", "r", "h", "l", "L", "M", "P", "@", "X", "S", "T", "q", "p", "u", "s", "b", "c", "n", "t", "~", "}", "\x07", "\x08", "\x09", "\x0a", "\x0b", "\x0d", "\x0e", "\x0f", "\x18", "\x1a", "\x7f", "a", "b", "z", "A", "日", "é", "😀", "́", "‍", "\x1b\\"];
export function genRaw(seed, rows, cols) {
  const r = rng(seed); const int = (n) => Math.floor(r() * n);
  const steps = []; const n = 2 + int(5);
  for (let i = 0; i < n; i++) {
    let s = ""; const m = 3 + int(25); for (let j = 0; j < m; j++) s += ALPH[int(ALPH.length)];
    let buf = Buffer.from(s, "utf8");
    if (r() < 0.15) { const cut = int(buf.length); buf = Buffer.concat([buf.subarray(0, cut), Buffer.from([0xe6, 0x97])]); }
    buf = Buffer.from(new TextDecoder().decode(buf), "utf8");
    steps.push({ feed: buf.toString("base64") });
  }
  steps.push({ snapshot: { includeWrapFlags: true } });
  return { rows: rows ?? 3 + int(5), cols: cols ?? 4 + int(12), scrollback: 50, steps };
}
