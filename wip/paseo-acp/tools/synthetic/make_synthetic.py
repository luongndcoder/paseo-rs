#!/usr/bin/env python3
"""Writes hand-made fixtures to ../../fixtures for agents that are not installed or not
authenticated here (Cursor, Kimi, Kiro, Trae/generic) and for paths no installed agent
exercises (Copilot allow_all, session/resume, session/list, string numeric ids).

Payloads come from the TS adapter source and its tests
(packages/server/src/server/agent/providers/*.test.ts), not from a live agent.
The first line of each file is {"synthetic": true, ...} so replay tests can tell them apart.
"""
import json, os

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "fixtures")
CWD = "/tmp/paseo-acp-fixture"
SID = "sess-1"
MODE_AGENT = "https://agentclientprotocol.com/protocol/session-modes#agent"
MODE_PLAN = "https://agentclientprotocol.com/protocol/session-modes#plan"
MODE_AUTOPILOT = "https://agentclientprotocol.com/protocol/session-modes#autopilot"


def init_params(meta=None):
    caps = {"fs": {"readTextFile": False, "writeTextFile": False}, "terminal": True}
    if meta:
        caps["_meta"] = meta
    return {"protocolVersion": 1, "clientCapabilities": caps, "clientInfo": {"name": "Paseo", "version": "dev"}}


def init_result(session_caps=None, load=False):
    caps = {"promptCapabilities": {"image": True, "audio": False, "embeddedContext": True}}
    if load:
        caps["loadSession"] = True
    if session_caps is not None:
        caps["sessionCapabilities"] = session_caps
    return {"protocolVersion": 1, "agentCapabilities": caps, "authMethods": []}


def req(i, method, params):
    return {"jsonrpc": "2.0", "id": i, "method": method, "params": params}


def res(i, result):
    return {"jsonrpc": "2.0", "id": i, "result": result}


def err(i, code, message, data=None):
    e = {"code": code, "message": message}
    if data is not None:
        e["data"] = data
    return {"jsonrpc": "2.0", "id": i, "error": e}


def note(method, params):
    return {"jsonrpc": "2.0", "method": method, "params": params}


def select(id_, name, category, current, values):
    return {"type": "select", "id": id_, "name": name, "category": category, "currentValue": current,
            "options": [{"value": v, "name": n} for v, n in values]}


def write(name, source, lines):
    path = os.path.join(OUT, name + ".jsonl")
    with open(path, "w") as f:
        f.write(json.dumps({"synthetic": True, "source": source}) + "\n")
        for d, m in lines:
            f.write(json.dumps({"dir": d, "line": m}) + "\n")
        f.write(json.dumps({"exit": {"code": 0, "signal": None}}) + "\n")


I, O = "in", "out"

# 10: Copilot allow-all (providerModeWriter + beforeModeWriter in copilot-acp-agent.ts)
modes = [
    {"id": MODE_AGENT, "name": "Agent", "description": "Default agent mode for conversational interactions"},
    {"id": MODE_PLAN, "name": "Plan", "description": "Plan mode for creating and executing multi-step plans"},
    {"id": MODE_AUTOPILOT, "name": "Autopilot", "description": "Autonomous mode (experimental)"},
]
mode_opt = lambda cur: select("mode", "Mode", "mode", cur, [(MODE_AGENT, "Agent"), (MODE_PLAN, "Plan"), (MODE_AUTOPILOT, "Autopilot")])
allow_opt = lambda cur: select("allow_all", "Allow all", "_allow_all", cur, [("off", "Off"), ("on", "On")])
write("10-copilot-allow-all", "copilot-acp-agent.ts transformCopilot*/writeCopilotProviderMode/beforeCopilotModeWriter", [
    (I, req(0, "initialize", init_params())),
    (O, res(0, init_result({"list": {}}, load=True))),
    (I, req(1, "session/new", {"cwd": CWD, "mcpServers": []})),
    (O, res(1, {"sessionId": SID, "modes": {"availableModes": modes, "currentModeId": MODE_AGENT},
                "configOptions": [mode_opt(MODE_AGENT), allow_opt("off")]})),
    (I, req(2, "session/set_config_option", {"sessionId": SID, "configId": "allow_all", "value": "on"})),
    (O, res(2, {"configOptions": [mode_opt(MODE_AGENT), allow_opt("on")]})),
    (I, req(3, "session/set_config_option", {"sessionId": SID, "configId": "allow_all", "value": "off"})),
    (O, res(3, {"configOptions": [mode_opt(MODE_AGENT), allow_opt("off")]})),
    (I, req(4, "session/set_mode", {"sessionId": SID, "modeId": MODE_PLAN})),
    (O, res(4, {})),
])

# 11: Kiro commands via extension notification (kiro-acp-agent.ts, acp-agent.test.ts)
write("11-kiro-commands", "kiro-acp-agent.ts mapKiroAvailableCommands; payload from acp-agent.test.ts", [
    (I, req(0, "initialize", init_params())),
    (O, res(0, init_result(load=True))),
    (I, req(1, "session/new", {"cwd": CWD, "mcpServers": []})),
    (O, res(1, {"sessionId": SID})),
    (O, note("_kiro.dev/commands/available", {"sessionId": "other-session",
             "commands": [{"name": "/ignored", "description": "wrong session"}], "prompts": []})),
    (O, note("_kiro.dev/commands/available", {"sessionId": SID,
        "commands": [{"name": "/agent", "description": "Select or list available agents",
                      "meta": {"inputType": "selection", "hint": "swap <name>"}}],
        "prompts": [{"name": "agent-sync-doctor", "description": "Hand off Claude or Codex state across Macs",
                     "arguments": [], "serverName": "skill:config"}],
        "tools": [{"name": "code", "description": "Code intelligence", "source": "built-in"}]})),
])

# 12: Cursor catalog (cursor-acp-agent.ts, cursor-acp-catalog.test.ts)
thinking_bool = select("thinking", "Thinking", "thought_level", "true", [("false", "Off"), ("true", "On")])
thinking_effort = select("effort", "Effort", "thought_level", "xhigh",
                         [("low", "Low"), ("medium", "Medium"), ("high", "High"), ("xhigh", "Extra High")])
cursor_model_opt = select("model", "Model", "model", "claude-haiku-4-5", [("claude-haiku-4-5", "Haiku 4.5"), ("grok-4.6", "Grok 4.6")])
write("12-cursor-catalog", "cursor-acp-agent.ts resolveCursorCatalogModels; payload from cursor-acp-catalog.test.ts", [
    (I, req(0, "initialize", init_params({"parameterizedModelPicker": True}))),
    (O, res(0, init_result(load=True))),
    (I, req(1, "session/new", {"cwd": CWD, "mcpServers": []})),
    (O, res(1, {"sessionId": SID, "configOptions": [cursor_model_opt, thinking_bool]})),
    (I, req(2, "cursor/list_available_models", {})),
    (O, res(2, {"models": [
        {"value": "claude-haiku-4-5", "name": "Haiku 4.5", "configOptions": [thinking_bool]},
        {"value": "grok-4.6", "name": "Grok 4.6", "configOptions": [thinking_effort]}]})),
])
write("12b-cursor-catalog-old-cli", "cursor-acp-catalog.test.ts: -32601 means the CLI is too old", [
    (I, req(0, "initialize", init_params({"parameterizedModelPicker": True}))),
    (O, res(0, init_result(load=True))),
    (I, req(1, "session/new", {"cwd": CWD, "mcpServers": []})),
    (O, res(1, {"sessionId": SID, "configOptions": [cursor_model_opt, thinking_bool]})),
    (I, req(2, "cursor/list_available_models", {})),
    (O, err(2, -32601, "Method not found")),
])

# 13: Kimi per-model probing (kimi-acp-agent.ts, kimi-acp-agent.test.ts)
kimi_model = lambda cur: select("model", "Model", "model", cur, [("kimi-for-coding", "K2.7 Coding Fast"), ("kimi-k3", "K3")])
kimi_bool = select("thinking", "Thinking", "thought_level", "off", [("off", "Off"), ("on", "On")])
kimi_effort = select("thinking", "Thinking", "thought_level", "medium",
                     [("off", "Off"), ("low", "Low"), ("medium", "Medium"), ("high", "High")])
write("13-kimi-catalog", "kimi-acp-agent.ts resolveKimiCatalogModels; payload from kimi-acp-agent.test.ts", [
    (I, req(0, "initialize", init_params())),
    (O, res(0, init_result())),
    (I, req(1, "session/new", {"cwd": CWD, "mcpServers": []})),
    (O, res(1, {"sessionId": SID, "configOptions": [kimi_model("kimi-for-coding"), kimi_bool]})),
    (I, req(2, "session/set_config_option", {"sessionId": SID, "configId": "model", "value": "kimi-for-coding"})),
    (O, res(2, {"configOptions": [kimi_model("kimi-for-coding"), kimi_bool]})),
    (I, req(3, "session/set_config_option", {"sessionId": SID, "configId": "model", "value": "kimi-k3"})),
    (O, res(3, {"configOptions": [kimi_model("kimi-k3"), kimi_effort]})),
])

# 14: generic agent publishing commands with available_commands_update (generic-acp-agent.commands.test.ts)
write("14-generic-commands", "generic-acp-agent.commands.test.ts", [
    (I, req(0, "initialize", init_params())),
    (O, res(0, init_result())),
    (I, req(1, "session/new", {"cwd": CWD, "mcpServers": []})),
    (O, res(1, {"sessionId": SID})),
    (O, note("session/update", {"sessionId": SID, "update": {
        "sessionUpdate": "available_commands_update",
        "availableCommands": [{"name": "review", "description": "Review the diff", "input": {"hint": "query"}},
                              {"name": "status", "description": "Show status"}]}})),
])

# 15: deepseek-tui answers with stringified numeric ids (COMPAT(deepseek-tui-acp-id))
write("15-string-numeric-ids", "acp-agent.ts normalizeACPIncomingMessage; acp-agent.test.ts", [
    (I, req(0, "initialize", init_params())),
    (O, {"jsonrpc": "2.0", "id": "0", "result": init_result()}),
    (I, req(1, "session/new", {"cwd": CWD, "mcpServers": []})),
    (O, {"jsonrpc": "2.0", "id": "1", "result": {"sessionId": SID}}),
])

# 16: session/list paging (listImportableSessions in acp-agent.ts)
write("16-list-sessions", "acp-agent.ts listImportableSessions / acpSessionListRequest", [
    (I, req(0, "initialize", init_params())),
    (O, res(0, init_result({"list": {}}, load=True))),
    (I, req(1, "session/list", {"cwd": CWD})),
    (O, res(1, {"sessions": [{"sessionId": "s-1", "cwd": CWD, "title": "First", "updatedAt": "2026-01-02T03:04:05Z"},
                             {"sessionId": "s-2", "cwd": CWD}], "nextCursor": "c1"})),
    (I, req(2, "session/list", {"cursor": "c1", "cwd": CWD})),
    (O, res(2, {"sessions": [{"sessionId": "s-3", "cwd": CWD, "title": None}]})),
])

# 17: resume through session/resume when the agent cannot load history
write("17-resume-unstable", "acp-agent.ts initializeResumedSession (sessionCapabilities.resume)", [
    (I, req(0, "initialize", init_params())),
    (O, res(0, init_result({"resume": {}, "close": {}}))),
    (I, req(1, "session/resume", {"sessionId": SID, "cwd": CWD, "mcpServers": []})),
    (O, res(1, {"configOptions": [select("mode", "Mode", "mode", "default", [("default", "Default"), ("plan", "Plan")])]})),
    (I, req(2, "session/close", {"sessionId": SID})),
    (O, res(2, {})),
])
print("wrote synthetic fixtures to", os.path.abspath(OUT))
