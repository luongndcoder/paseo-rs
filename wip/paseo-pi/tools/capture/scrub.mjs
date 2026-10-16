// Scrub a fixture JSONL: temp paths, home dirs, secrets, proxy URLs, system prompts.
import { readFileSync, writeFileSync } from "node:fs";

export function scrubText(text, extraPaths = []) {
  let out = text;
  for (const p of extraPaths) out = out.split(p).join("/tmp/paseo-pi-fixture");
  out = out.replace(/\/private\/tmp\/claude-\d+\/[^"\\\s]*?\/scratchpad\/[A-Za-z0-9_.-]+/g, "/tmp/paseo-pi-fixture");
  out = out.replace(/--private-tmp-claude-[A-Za-z0-9-]*?-scratchpad-pp-[A-Za-z0-9]+-[A-Za-z0-9]+--/g, "--tmp-paseo-pi-fixture--");
  out = out.replace(/\/Users\/[A-Za-z0-9_.-]+/g, "/home/user");
  out = out.replace(/https?:\/\/[A-Za-z0-9.-]*proxy.invalid\.vn[^"\\\s]*/g, "https://proxy.invalid/v1");
  out = out.replace(/sk-[A-Za-z0-9_-]{8,}/g, "REDACTED");
  out = out.replace(/(Bearer\s+)[A-Za-z0-9._~+\/=-]{16,}/g, "$1REDACTED");
  out = out.replace(/\b(eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,})/g, "REDACTED");
  return out;
}

function walk(o) {
  if (Array.isArray(o)) return o.forEach(walk);
  if (!o || typeof o !== "object") return;
  if (o.role === "system") {
    // The system prompt embeds local config, extension lists and paths.
    if (typeof o.content === "string") o.content = o.content ? "[scrubbed system prompt]" : "";
    if (o.sections) o.sections = { scrubbed: true };
  }
  // Command listings enumerate the operator's installed skills; keep a few.
  if (Array.isArray(o.commands) && o.commands.length > 5) o.commands = o.commands.slice(0, 5);
  for (const k of Object.keys(o)) {
    if (/^(apiKey|api_key|token|authToken|oauth.*|authorization|secret|password)$/i.test(k)) o[k] = "REDACTED";
    else if (k === "headers" && o[k] && typeof o[k] === "object" && !Array.isArray(o[k])) o[k] = {};
    else walk(o[k]);
  }
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
    walk(obj);
    kept.push(JSON.stringify(obj));
  }
  writeFileSync(path, kept.join("\n") + "\n");
}
