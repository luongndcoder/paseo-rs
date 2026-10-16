#!/usr/bin/env python3
"""Writes the hand-made OMP fixtures. OMP is not installed on the capture machine, so these are
built from the TS adapter's schemas (omp/rpc-types.ts) and tests (protocol-session.test.ts,
agent.test.ts, host-tools.test.ts, v17-rpc-compat.test.ts). Replace with a real capture when an
omp binary is available: point tools/capture/run-scenarios.mjs at OMP via PI_REAL=omp."""
import json, os

OUT = os.path.join(os.path.dirname(__file__), "..", "fixtures")
NOTE = "synthetic: hand-written from the TS adapter schemas and tests (omp binary not installed)"
READY = {"type": "ready", "supportedProtocolVersions": [1, 2],
         "maxFrameBytes": 1024 * 1024, "maxReassembledFrameBytes": 64 * 1024 * 1024}
MODEL = {"provider": "anthropic", "id": "claude-haiku-4-5", "name": "Claude Haiku 4.5",
         "reasoning": True, "contextWindow": 200000, "maxTokens": 64000,
         "thinking": {"mode": "effort", "efforts": ["minimal", "low", "medium", "high"],
                      "defaultLevel": "medium"},
         "input": ["text", "image"]}
STATE = {"model": MODEL, "thinkingLevel": "medium", "isStreaming": False, "isCompacting": False,
         "autoCompactionEnabled": True, "sessionFile": "/tmp/paseo-pi-fixture/omp/session.jsonl",
         "sessionId": "omp-session-1", "messageCount": 0, "queuedMessageCount": 0}


def i(frame):
    return {"dir": "in", "line": frame}


def o(frame):
    return {"dir": "out", "line": frame}


def resp(id_, command, data=None, ok=True, error=None):
    frame = {"id": id_, "type": "response", "command": command, "success": ok}
    if data is not None:
        frame["data"] = data
    if error:
        frame["error"] = error
    return o(frame)


def write(name, header, entries):
    with open(os.path.join(OUT, name), "w") as f:
        f.write(json.dumps({**header, "synthetic": NOTE}) + "\n")
        for e in entries:
            f.write(json.dumps(e) + "\n")


def assistant(text, **extra):
    return {"role": "assistant", "content": [{"type": "text", "text": text}],
            "provider": "anthropic", "model": "claude-haiku-4-5", "stopReason": "stop", **extra}


user = {"role": "user", "content": [{"type": "text", "text": "Reply with exactly: pong"}], "timestamp": 1}

# 1. ready, v2 negotiation, prompt, streaming reply, late prompt_result.
write("omp-01-prompt-reply.jsonl",
      {"argv": ["--mode", "rpc", "--model", "anthropic/claude-haiku-4-5", "--no-session"]}, [
    o(READY),
    i({"type": "negotiate_protocol", "protocolVersion": 2, "id": "req_1"}),
    resp("req_1", "negotiate_protocol", {"protocolVersion": 2}),
    i({"type": "get_state", "id": "req_2"}),
    resp("req_2", "get_state", STATE),
    i({"type": "prompt", "message": "Reply with exactly: pong", "id": "req_3"}),
    resp("req_3", "prompt", {"agentInvoked": True}),
    o({"type": "agent_start"}),
    o({"type": "turn_start"}),
    o({"type": "message_start", "message": user}),
    o({"type": "message_end", "message": user}),
    o({"type": "message_start", "message": {**assistant(""), "content": []}}),
    o({"type": "message_update", "message": assistant("po"),
       "assistantMessageEvent": {"type": "text_delta", "delta": "po"}}),
    o({"type": "message_update", "message": assistant("pong"),
       "assistantMessageEvent": {"type": "text_delta", "delta": "ng"}}),
    o({"type": "message_end", "message": assistant("pong")}),
    o({"type": "agent_end", "messages": [user, assistant("pong")]}),
    o({"type": "prompt_result", "id": "req_3", "agentInvoked": True, "status": "ok"}),
    {"exit": {"code": 0, "signal": None}},
])

# 2. a prompt OMP acknowledges and then rejects (no API key), then the process dies.
write("omp-02-prompt-rejected.jsonl",
      {"argv": ["--mode", "rpc", "--no-session"]}, [
    o(READY),
    i({"type": "negotiate_protocol", "protocolVersion": 2, "id": "req_1"}),
    resp("req_1", "negotiate_protocol", {"protocolVersion": 2}),
    i({"type": "get_state", "id": "req_2"}),
    resp("req_2", "get_state", STATE),
    i({"type": "prompt", "message": "hello", "id": "req_3"}),
    resp("req_3", "prompt", {}),
    o({"type": "prompt_result", "id": "req_3", "agentInvoked": False, "status": "error",
       "error": {"message": "No API key found for anthropic."}}),
    o({"type": "notice", "level": "error", "message": "No API key found for anthropic.",
       "source": "auth"}),
    {"exit": {"code": 1, "signal": None}},
])

# 3. dialogs, host tools, subagents, commands, branching.
write("omp-03-host-tools-and-ui.jsonl",
      {"argv": ["--mode", "rpc-ui", "--no-session"]}, [
    o(READY),
    i({"type": "negotiate_protocol", "protocolVersion": 2, "id": "req_1"}),
    resp("req_1", "negotiate_protocol", {"protocolVersion": 2}),
    i({"type": "get_state", "id": "req_2"}),
    resp("req_2", "get_state", STATE),
    i({"type": "get_available_commands", "id": "req_3"}),
    resp("req_3", "get_available_commands", {"commands": [
        {"name": "compact", "description": "Compact the context", "source": "builtin",
         "input": {"hint": "[instructions]"}},
        {"name": "review", "source": "skill", "input": None}]}),
    i({"type": "set_subagent_subscription", "level": "events", "id": "req_4"}),
    resp("req_4", "set_subagent_subscription"),
    i({"type": "set_host_tools", "id": "req_5", "tools": [
        {"name": "paseo_echo", "description": "Echo text",
         "parameters": {"type": "object", "properties": {"text": {"type": "string"}}}}]}),
    resp("req_5", "set_host_tools", {"toolNames": ["paseo_echo"]}),
    i({"type": "prompt", "message": "use the echo tool", "id": "req_6"}),
    resp("req_6", "prompt", {"agentInvoked": True}),
    o({"type": "agent_start"}),
    o({"type": "extension_ui_request", "id": "ui-1", "method": "confirm",
       "title": "Allow echo?", "message": "paseo_echo wants to run"}),
    i({"type": "extension_ui_response", "id": "ui-1", "confirmed": True}),
    o({"type": "host_tool_call", "id": "ht-1", "toolCallId": "call-1", "toolName": "paseo_echo",
       "arguments": {"text": "hi"}}),
    i({"type": "host_tool_update", "id": "ht-1", "partialResult": {"content": [{"type": "text", "text": "h"}]}}),
    i({"type": "host_tool_result", "id": "ht-1", "isError": False,
       "result": {"content": [{"type": "text", "text": "hi"}]}}),
    o({"type": "subagent_lifecycle", "payload": {"id": "sa-1", "agent": "explore", "status": "started", "index": 0}}),
    o({"type": "subagent_progress", "payload": {"index": 0, "agent": "explore", "task": "look",
       "progress": {"id": "sa-1", "status": "running"}}}),
    o({"type": "todo_reminder", "todos": [{"content": "write tests", "status": "in_progress"}]}),
    o({"type": "available_commands_update", "commands": [{"name": "review", "source": "skill"}]}),
    o({"type": "message_end", "message": assistant("done")}),
    o({"type": "agent_end", "messages": [assistant("done")]}),
    i({"type": "get_branch_messages", "id": "req_7"}),
    resp("req_7", "get_branch_messages", {"messages": [{"entryId": "e1", "text": "use the echo tool"}]}),
    i({"type": "branch", "entryId": "e1", "id": "req_8"}),
    resp("req_8", "branch", {"text": "use the echo tool"}),
    {"exit": {"code": 0, "signal": None}},
])
