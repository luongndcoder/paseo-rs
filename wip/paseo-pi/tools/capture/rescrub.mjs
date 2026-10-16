import { readdirSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { scrubFile } from "./scrub.mjs";
const dir = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "fixtures");
for (const f of readdirSync(dir)) if (f.endsWith(".jsonl")) scrubFile(join(dir, f), []);
