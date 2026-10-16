/**
 * WP12 golden capture for paseo-skills. Drives the real TS orchestration-skills module.
 *
 *   cd <ts-repo>/packages/server
 *   TS_SKILLS_DIR=<ts-repo>/packages/server/src/server/orchestration-skills \
 *     ../../node_modules/.bin/tsx <paseo-rs>/fixtures/p4/skills/capture-skills.mts capture --out <paseo-rs>/fixtures/p4/skills
 *
 * Reverse interop (Rust crashed state recovered by TS), used by the ignored Rust test:
 *   ... capture-skills.mts recover <root> '<selection json>'      (prints the tree snapshot)
 *
 * Output is deterministic: fixed bundle contents, no wall clock, root and transaction uuid
 * replaced by {{ROOT}} / {{TXID}}.
 */
import { promises as fs } from "node:fs";
import os from "node:os";
import path from "node:path";

const SKILLS_DIR = process.env.TS_SKILLS_DIR;
if (!SKILLS_DIR) throw new Error("TS_SKILLS_DIR is required");

const controllerMod = await import(path.join(SKILLS_DIR, "internal/controller.ts"));
const operationsMod = await import(path.join(SKILLS_DIR, "internal/operations.ts"));
const transactionMod = await import(path.join(SKILLS_DIR, "internal/transaction.ts"));

type Selection = { mode: "all" } | { mode: "custom"; skills: string[] };
interface Targets {
  sourceDir: string;
  agentsDir: string;
  claudeDir: string;
  codexDir: string;
}
interface Entry {
  path: string;
  type: "file" | "dir" | "symlink";
  content?: string;
  mode?: number;
  target?: string;
}

function targetsFor(root: string): Targets {
  return {
    sourceDir: path.join(root, "bundle"),
    agentsDir: path.join(root, "home", ".agents", "skills"),
    claudeDir: path.join(root, "home", ".claude", "skills"),
    codexDir: path.join(root, "home", ".codex", "skills"),
  };
}

async function writeFiles(dir: string, files: Record<string, string>) {
  for (const [rel, content] of Object.entries(files)) {
    const full = path.join(dir, rel);
    await fs.mkdir(path.dirname(full), { recursive: true });
    await fs.writeFile(full, content);
  }
}

async function makeBundle(targets: Targets, version: string) {
  for (const name of ["paseo", "paseo-advisor", "paseo-loop"]) {
    await fs.rm(path.join(targets.sourceDir, name), { recursive: true, force: true });
    await writeFiles(path.join(targets.sourceDir, name), { "SKILL.md": `${name}-${version}` });
  }
  await writeFiles(path.join(targets.sourceDir, "paseo-loop"), {
    "references/guide.md": `guide-${version}`,
  });
}

async function snapshot(root: string): Promise<Entry[]> {
  const out: Entry[] = [];
  async function walk(dir: string) {
    const names = (await fs.readdir(dir)).sort();
    for (const name of names) {
      const full = path.join(dir, name);
      const rel = path.relative(root, full).split(path.sep).join("/");
      const info = await fs.lstat(full);
      if (info.isSymbolicLink()) {
        out.push({ path: rel, type: "symlink", target: await fs.readlink(full) });
      } else if (info.isDirectory()) {
        out.push({ path: rel, type: "dir" });
        await walk(full);
      } else {
        out.push({
          path: rel,
          type: "file",
          content: await fs.readFile(full, "utf8"),
          mode: info.mode & 0o777,
        });
      }
    }
  }
  await walk(root);
  return out;
}

function memoryStore(initial: Selection | undefined) {
  let current = initial;
  return {
    get: async () => current ?? ({ mode: "all" } as Selection),
    isSet: async () => current !== undefined,
    set: async (value: unknown) => {
      const v = value as { mode?: string; skills?: unknown };
      const names = Array.isArray(v?.skills)
        ? [...new Set((v.skills as unknown[]).filter((s): s is string => typeof s === "string").map((s) => s.trim()).filter(Boolean))].sort()
        : [];
      current = v?.mode === "custom" ? { mode: "custom", skills: names } : { mode: "all" };
      return current;
    },
  };
}

function hide(text: string, root: string, txid: string): string {
  return text.split(root).join("{{ROOT}}").split(txid).join("{{TXID}}");
}

async function findTx(parent: string): Promise<string> {
  const name = (await fs.readdir(parent)).find((e) => e.startsWith(".paseo-skills-transaction-"));
  if (!name) throw new Error("no transaction dir");
  return name;
}

async function wireScenario() {
  const root = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(), "paseo-skills-wire-")));
  try {
    const targets = targetsFor(root);
    await makeBundle(targets, "v1");
    const make = (initial: Selection | undefined) =>
      controllerMod.createSkillsController({
        resolveTargets: () => targets,
        selectionStore: memoryStore(initial),
      });
    const steps: Array<{ call: string; args?: unknown; text: string }> = [];
    const wrapStatus = (type: string, snap: unknown) => ({ type, payload: { requestId: "req-1", ...(snap as object) } });
    const controller = make({ mode: "all" });
    const record = (call: string, args: unknown, wire: unknown) =>
      steps.push({ call, ...(args === undefined ? {} : { args }), text: JSON.stringify(wire) });

    record("status", undefined, wrapStatus("agent.skills.get_status.response", await controller.status()));
    const save = async (selection: Selection, confirmedRemovals: string[] = []) => {
      const result = await controller.save({ ...selection, confirmedRemovals });
      record("save", { selection, confirmedRemovals }, {
        type: "agent.skills.save_selection.response",
        payload: { requestId: "req-1", ...result },
      });
    };
    await save({ mode: "custom", skills: ["paseo-loop", "paseo"] });
    record("status", undefined, wrapStatus("agent.skills.get_status.response", await controller.status()));
    await save({ mode: "all" });
    await save({ mode: "custom", skills: ["paseo"] });
    await save({ mode: "custom", skills: ["paseo"] }, ["paseo-advisor", "paseo-loop"]);
    record("uninstall", undefined, wrapStatus("agent.skills.uninstall.response", await controller.uninstall()));
    record("reconcile", undefined, wrapStatus("agent.skills.reconcile.response", await controller.update()));
    const imp = async (c: typeof controller, selection: unknown) => {
      const result = await c.importLegacySelectionIfUnset(selection);
      record("import", { selection }, {
        type: "agent.skills.import_legacy_selection.response",
        payload: { requestId: "req-1", ...result },
      });
    };
    await imp(controller, { mode: "custom", skills: ["paseo"] });
    // A second controller over an unset store: first import wins, second is ignored.
    const fresh = make(undefined);
    await imp(fresh, { mode: "custom", skills: ["b", "a", "a"] });
    await imp(fresh, { mode: "all" });
    return steps;
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
}

const PREVIOUS: Selection = { mode: "custom", skills: ["paseo", "paseo-loop"] };
const NEXT: Selection = { mode: "custom", skills: ["paseo", "paseo-advisor"] };

async function transactionScenarios() {
  const root = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(), "paseo-skills-tx-")));
  try {
    const targets = targetsFor(root);
    await makeBundle(targets, "v1");
    const store = memoryStore(PREVIOUS);
    const controller = controllerMod.createSkillsController({ resolveTargets: () => targets, selectionStore: store });
    await controller.save({ ...PREVIOUS, confirmedRemovals: [] });
    for (const dir of [targets.agentsDir, targets.claudeDir, targets.codexDir]) {
      await writeFiles(path.join(dir, "paseo-loop"), { "notes/mine.md": "hand written" });
    }
    await writeFiles(path.join(targets.agentsDir, "paseo"), { "my-context.md": "user context" });
    await makeBundle(targets, "v2");
    const before = await snapshot(root);

    const plan = await operationsMod.getSkillsStatus(targets, NEXT);
    const kinds = plan.ops.map((op: { kind: string; name: string }) => `${op.kind}:${op.name}`);
    const tx = await transactionMod.beginSkillsTransaction(targets, PREVIOUS, NEXT, plan.ops);
    void tx;
    await operationsMod.installSkills(targets, NEXT, {
      ...plan,
      ops: plan.ops.filter((op: { kind: string }) => op.kind !== "delete"),
    });
    const crashed = await snapshot(root);
    const txName = await findTx(path.dirname(targets.agentsDir));
    const txid = txName.replace(".paseo-skills-transaction-", "");

    const toPrevious = async () => {
      await transactionMod.recoverInterruptedSkillTransactions(targets, PREVIOUS);
      return snapshot(root);
    };
    const recoveredToPrevious = await toPrevious();

    // Rebuild the crashed state byte for byte to recover with the other selection.
    await fs.rm(root, { recursive: true, force: true });
    await fs.mkdir(root, { recursive: true });
    await materialize(root, crashed);
    await transactionMod.recoverInterruptedSkillTransactions(targets, NEXT);
    const recoveredToNext = await snapshot(root);

    // capturing phase: only the first manifest write happened.
    await fs.rm(root, { recursive: true, force: true });
    await fs.mkdir(root, { recursive: true });
    await materialize(root, before);
    const capDir = path.join(path.dirname(targets.agentsDir), `.paseo-skills-transaction-${txid}`);
    await fs.mkdir(capDir, { recursive: true });
    await fs.writeFile(
      path.join(capDir, "transaction.json"),
      `${JSON.stringify({ owner: "paseo-skills-transaction", version: 1, phase: "capturing", previousSelection: PREVIOUS, nextSelection: NEXT, entries: [] }, null, 2)}\n`,
    );
    const capturing = await snapshot(root);
    await transactionMod.recoverInterruptedSkillTransactions(targets, PREVIOUS);
    const recoveredCapturing = await snapshot(root);

    const norm = (entries: Entry[]) =>
      entries.map((e) => ({
        ...e,
        path: hide(e.path, root, txid),
        ...(e.content !== undefined ? { content: hide(e.content, root, txid) } : {}),
        ...(e.target !== undefined ? { target: hide(e.target, root, txid) } : {}),
      }));
    const beforeN = norm(before);
    const sameTree = JSON.stringify(norm(recoveredToPrevious)) === JSON.stringify(beforeN);
    if (!sameTree) console.warn("WARNING: TS recovery did not return to the before tree");
    return {
      previous: PREVIOUS,
      next: NEXT,
      planOps: kinds,
      before: beforeN,
      crashed: norm(crashed),
      recoveredToPrevious: norm(recoveredToPrevious),
      recoveredToNext: norm(recoveredToNext),
      capturing: norm(capturing),
      recoveredCapturing: norm(recoveredCapturing),
    };
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
}

async function materialize(root: string, entries: Entry[]) {
  for (const e of entries) {
    const full = path.join(root, e.path);
    if (e.type === "dir") await fs.mkdir(full, { recursive: true });
    else if (e.type === "symlink") await fs.symlink(e.target!, full);
    else {
      await fs.mkdir(path.dirname(full), { recursive: true });
      await fs.writeFile(full, e.content!, { mode: e.mode });
      await fs.chmod(full, e.mode!);
    }
  }
}

const [mode, ...rest] = process.argv.slice(2);
if (mode === "capture") {
  const out = rest[rest.indexOf("--out") + 1];
  if (!out) throw new Error("--out required");
  await fs.mkdir(out, { recursive: true });
  const wire = await wireScenario();
  await fs.writeFile(path.join(out, "wire-scenario.json"), `${JSON.stringify({ steps: wire }, null, 2)}\n`);
  const tx = await transactionScenarios();
  await fs.writeFile(path.join(out, "transactions.json"), `${JSON.stringify(tx, null, 2)}\n`);
  console.log(`wrote ${wire.length} wire steps and transaction scenarios to ${out}`);
} else if (mode === "recover") {
  const [root, selection] = rest;
  const targets = targetsFor(root!);
  await transactionMod.recoverInterruptedSkillTransactions(targets, JSON.parse(selection!));
  console.log(JSON.stringify(await snapshot(root!)));
} else {
  throw new Error("usage: capture|recover");
}
