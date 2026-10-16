#!/usr/bin/env python3
"""Derive data/tool-defs.json from the WP0 golden fixtures/p4/tool-catalog/tools-list.json.

One entry per tool in registration order (the `full` configuration). `input` is the catalog
inputJsonSchema without a caller agent, `inputCaller` is present only when the agent-scoped
catalog serializes a different schema (create_agent, send_agent_prompt, create_schedule).
`output` is the outputJsonSchema (null for browser tools). The MCP tools/list shape is derived
from these at runtime (see serialization.rs); tests assert both stay equal to the fixture.

    python3 derive-tool-defs.py  # run from this directory
"""
import json
from pathlib import Path

root = Path(__file__).resolve().parents[3]
src = json.loads((root / "fixtures/p4/tool-catalog/tools-list.json").read_text())
lists = {l["name"]: l for l in src["lists"]}


def by_name(cfg):
    return {t["name"]: t for t in lists[cfg]["catalogTools"]}


full, caller = by_name("full"), by_name("caller_agent")
out = []
for name in lists["full"]["toolNames"]:
    t = full[name]
    entry = {
        "name": name,
        "title": t["title"],
        "description": t["description"],
        "input": t["inputJsonSchema"],
    }
    if name in caller and caller[name]["inputJsonSchema"] != t["inputJsonSchema"]:
        entry["inputCaller"] = caller[name]["inputJsonSchema"]
    entry["output"] = t["outputJsonSchema"]
    out.append(entry)
(Path(__file__).parent / "tool-defs.json").write_text(json.dumps(out, indent=1, ensure_ascii=False) + "\n")
print(len(out), "tools")
