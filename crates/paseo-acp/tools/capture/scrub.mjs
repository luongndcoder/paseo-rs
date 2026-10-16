// Scrub a fixture JSONL: temp paths -> /tmp/paseo-acp-fixture, home -> /home/user, drop token-looking values.
import { readFileSync, writeFileSync } from "node:fs";

export function scrubText(text, extraPaths = []) {
  let out = text;
  for (const p of extraPaths) out = out.split(p).join("/tmp/paseo-acp-fixture");
  out = out.replace(/\/private\/tmp\/claude-\d+\/[^"\\\s]*?\/scratchpad\/pa-[A-Za-z0-9_.-]+/g, "/tmp/paseo-acp-fixture");
  out = out.replace(/\/Users\/[A-Za-z0-9_.-]+/g, "/home/user");
  out = out.replace(/(gh[pousr]_|github_pat_)[A-Za-z0-9_]{16,}/g, "REDACTED");
  out = out.replace(/sk-[A-Za-z0-9_-]{16,}/g, "REDACTED");
  out = out.replace(/(Bearer\s+)[A-Za-z0-9._~+\/=-]{16,}/g, "$1REDACTED");
  out = out.replace(/\b(eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,})/g, "REDACTED");
  return out;
}
export function scrubFile(path, extraPaths) {
  const lines = readFileSync(path, "utf8").split("\n").filter(Boolean);
  const kept = [];
  for (const raw of lines) {
    let obj;
    try {
      obj = JSON.parse(scrubText(raw, extraPaths));
    } catch {
      continue;
    }
    (function walk(o) {
      if (o && typeof o === "object") {
        for (const k of Object.keys(o)) {
          if (/^(apiKey|api_key|token|authToken|oauth.*|authorization|secret|password)$/i.test(k)) o[k] = "REDACTED";
          else walk(o[k]);
        }
      }
    })(obj);
    kept.push(JSON.stringify(obj));
  }
  writeFileSync(path, kept.join("\n") + "\n");
}
