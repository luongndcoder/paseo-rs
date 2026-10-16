//! Ported TS unit tests for the detail pipeline:
//! `tool-call-read-gutter.test.ts`, `claude/tool-call-mapper.test.ts`,
//! `opencode/tool-call-mapper.test.ts`, `pi/tool-call-mapper.test.ts`, plus Codex cases.

use paseo_agent_types::{Map, ToolCallItem, ToolCallStatus};
use paseo_tool_detail::detail::claude::*;
use paseo_tool_detail::detail::codex::derive_codex_tool_detail;
use paseo_tool_detail::detail::opencode::*;
use paseo_tool_detail::detail::pi::*;
use paseo_tool_detail::detail::primitives::*;
use paseo_tool_detail::gutter::strip_read_line_number_gutter;
use serde_json::{json, Value};

fn claude(
    f: fn(&ClaudeToolCallParams) -> Option<ToolCallItem>,
    call_id: Option<&str>,
    name: &str,
    input: Value,
    output: Value,
) -> Option<ToolCallItem> {
    f(&ClaudeToolCallParams {
        call_id,
        name,
        input: Some(&input),
        output: Some(&output),
        metadata: None,
    })
}

fn detail(item: &ToolCallItem) -> Value {
    item.detail.to_value()
}

// ---- tool-call-read-gutter.test.ts -----------------------------------------------------------

#[test]
fn strips_cat_n_gutter_and_reports_first_line() {
    let r =
        strip_read_line_number_gutter(Some("1\timport a from 'a';\n2\t\n3\tconst x = 1;")).unwrap();
    assert_eq!(r.content, "import a from 'a';\n\nconst x = 1;");
    assert_eq!(r.start_line, Some(1.0));
}

#[test]
fn handles_right_aligned_numbers_and_offset() {
    let r = strip_read_line_number_gutter(Some("  41\tconst y = 2;\n  42\treturn y;")).unwrap();
    assert_eq!(r.content, "const y = 2;\nreturn y;");
    assert_eq!(r.start_line, Some(41.0));
}

#[test]
fn keeps_trailing_notice() {
    let r = strip_read_line_number_gutter(Some(
        "1\tline one\n2\tline two\n<system-reminder>truncated</system-reminder>",
    ))
    .unwrap();
    assert_eq!(
        r.content,
        "line one\nline two\n<system-reminder>truncated</system-reminder>"
    );
    assert_eq!(r.start_line, Some(1.0));
}

#[test]
fn does_not_strip_non_gutter_content() {
    assert!(strip_read_line_number_gutter(Some("import a from 'a';\nconst x = 1;")).is_none());
    assert!(strip_read_line_number_gutter(Some("1\tfoo\n5\tbar")).is_none());
    assert!(strip_read_line_number_gutter(Some("header line\n1\tfoo\n2\tbar")).is_none());
    assert!(strip_read_line_number_gutter(None).is_none());
    assert!(strip_read_line_number_gutter(Some("")).is_none());
}

#[test]
fn gutter_line_with_cr_is_not_a_match() {
    // `.` in the TS regex excludes \r
    assert!(strip_read_line_number_gutter(Some("1\ta\rb\n2\tc")).is_none());
}

#[test]
fn to_read_tool_detail_gutter_normalization() {
    let input = ReadIn {
        file_path: Some("/repo/src/index.ts".into()),
        ..Default::default()
    };
    let out = ReadOut {
        file_path: None,
        content: Some("10\tconst a = 1;\n11\tconst b = 2;".into()),
    };
    let d = to_read_tool_detail(Some(&input), Some(&out), None).unwrap();
    assert_eq!(
        d.to_value(),
        json!({"type":"read","filePath":"/repo/src/index.ts","content":"const a = 1;\nconst b = 2;","offset":10})
    );

    let input = ReadIn {
        file_path: Some("/repo/src/index.ts".into()),
        offset: Some(10.into()),
        limit: Some(2.into()),
    };
    let d = to_read_tool_detail(Some(&input), Some(&out), None).unwrap();
    let v = d.to_value();
    assert_eq!(v["offset"], 10);
    assert_eq!(v["limit"], 2);
    assert_eq!(v["content"], "const a = 1;\nconst b = 2;");

    let raw = ReadOut {
        file_path: None,
        content: Some("const a = 1;\nconst b = 2;".into()),
    };
    let input = ReadIn {
        file_path: Some("/repo/src/index.ts".into()),
        ..Default::default()
    };
    let d = to_read_tool_detail(Some(&input), Some(&raw), None).unwrap();
    assert_eq!(
        d.to_value(),
        json!({"type":"read","filePath":"/repo/src/index.ts","content":"const a = 1;\nconst b = 2;"})
    );
}

// ---- claude/tool-call-mapper.test.ts -----------------------------------------------------------

#[test]
fn claude_plan_identity_and_outcome() {
    let input = json!({"plan": "Ship it"});
    let p = ClaudeToolCallParams {
        call_id: Some("plan-tool-1"),
        name: "ExitPlanMode",
        input: Some(&input),
        output: None,
        metadata: None,
    };
    let pending = map_claude_running_tool_call(&p).unwrap();
    let rejected = map_claude_failed_tool_call(&p, &json!("Denied by user")).unwrap();
    let approved = map_claude_completed_tool_call(&p).unwrap();
    let canceled = map_claude_canceled_tool_call(&p).unwrap();

    assert_eq!(canceled.call_id, "plan-tool-1");
    assert_eq!(canceled.name, "plan_approval");
    assert_eq!(canceled.status, ToolCallStatus::Canceled);
    assert_eq!(detail(&canceled), json!({"type":"plan","text":"Ship it"}));
    assert_eq!(pending.call_id, "plan-tool-1");
    assert_eq!(pending.name, "ExitPlanMode");
    assert_eq!(detail(&pending), json!({"type":"plan","text":"Ship it"}));
    assert_eq!(rejected.name, "plan_approval");
    assert_eq!(rejected.status, ToolCallStatus::Completed);
    assert_eq!(rejected.error, Some(Value::Null));
    assert_eq!(rejected.detail, pending.detail);
    assert_eq!(rejected.metadata.unwrap()["approved"], false);
    assert_eq!(approved.name, "plan_approval");
    assert_eq!(approved.status, ToolCallStatus::Completed);
    assert_eq!(approved.metadata.unwrap()["approved"], true);
}

#[test]
fn claude_running_shell() {
    let item = claude(
        map_claude_running_tool_call,
        Some("claude-call-1"),
        "Bash",
        json!({"command":"pwd","cwd":"/tmp/repo"}),
        Value::Null,
    )
    .unwrap();
    assert_eq!(item.status, ToolCallStatus::Running);
    assert_eq!(item.error, Some(Value::Null));
    assert_eq!(item.call_id, "claude-call-1");
    assert_eq!(
        detail(&item),
        json!({"type":"shell","command":"pwd","cwd":"/tmp/repo"})
    );
}

#[test]
fn claude_partial_running_input() {
    let item = claude(
        map_claude_running_tool_call,
        Some("c"),
        "Bash",
        json!({"command":"echo "}),
        Value::Null,
    )
    .unwrap();
    assert_eq!(detail(&item), json!({"type":"shell","command":"echo "}));
}

#[test]
fn claude_running_known_variants_have_detail() {
    let cases = [
        (
            "read_file",
            json!({"file_path":"README.md"}),
            json!({"type":"read","filePath":"README.md"}),
        ),
        (
            "write_file",
            json!({"file_path":"src/new.ts"}),
            json!({"type":"write","filePath":"src/new.ts"}),
        ),
        (
            "apply_patch",
            json!({"file_path":"src/index.ts"}),
            json!({"type":"edit","filePath":"src/index.ts"}),
        ),
        (
            "web_search",
            json!({"query":"tool call mapping"}),
            json!({"type":"search","query":"tool call mapping","toolName":"web_search"}),
        ),
    ];
    for (name, input, want) in cases {
        let item = claude(
            map_claude_running_tool_call,
            Some("c"),
            name,
            input,
            Value::Null,
        )
        .unwrap();
        assert_eq!(detail(&item), want, "{name}");
    }
}

#[test]
fn claude_completed_read_enrichment_and_variants() {
    let item = claude(
        map_claude_completed_tool_call,
        Some("claude-call-2"),
        "read_file",
        json!({"file_path":"README.md"}),
        json!({"content":"hello"}),
    )
    .unwrap();
    assert_eq!(item.status, ToolCallStatus::Completed);
    assert_eq!(
        detail(&item),
        json!({"type":"read","filePath":"README.md","content":"hello"})
    );

    let item = claude(
        map_claude_completed_tool_call,
        Some("c"),
        "read_file",
        json!({"file_path":"README.md"}),
        json!({"content":[{"type":"output_text","text":"alpha"},{"type":"output_text","content":"beta"}]}),
    )
    .unwrap();
    assert_eq!(detail(&item)["content"], "alpha\nbeta");

    let item = claude(
        map_claude_completed_tool_call,
        Some("c"),
        "read_file",
        json!({"file_path":"README.md"}),
        json!({"structured_content":{"content":{"type":"output_text","text":"gamma"}}}),
    )
    .unwrap();
    assert_eq!(detail(&item)["content"], "gamma");
}

#[test]
fn claude_failed_requires_error() {
    let input = json!({"command":"false"});
    let p = ClaudeToolCallParams {
        call_id: Some("claude-call-3"),
        name: "shell",
        input: Some(&input),
        output: None,
        metadata: None,
    };
    let item = map_claude_failed_tool_call(&p, &json!({"message":"Command failed"})).unwrap();
    assert_eq!(item.status, ToolCallStatus::Failed);
    assert_eq!(item.error, Some(json!({"message":"Command failed"})));
    let item = map_claude_failed_tool_call(&p, &Value::Null).unwrap();
    assert_eq!(item.error, Some(json!({"message":"Tool call failed"})));
}

#[test]
fn claude_write_edit_search_shapes() {
    let w = claude(
        map_claude_completed_tool_call,
        Some("c"),
        "write_file",
        json!({"file_path":"src/new.ts","content":"export const x = 1;"}),
        Value::Null,
    )
    .unwrap();
    assert_eq!(detail(&w)["type"], "write");
    assert_eq!(detail(&w)["filePath"], "src/new.ts");

    let e = claude(
        map_claude_completed_tool_call,
        Some("c"),
        "apply_patch",
        json!({"file_path":"src/index.ts","patch":"@@\\n-old\\n+new\\n"}),
        Value::Null,
    )
    .unwrap();
    assert_eq!(detail(&e)["type"], "edit");
    assert!(detail(&e)["unifiedDiff"].as_str().unwrap().contains("@@"));
}

#[test]
fn claude_unknown_tools_keep_raw_payloads() {
    let item = claude(
        map_claude_completed_tool_call,
        Some("claude-call-4"),
        "my_custom_tool",
        json!({"foo":"bar"}),
        json!({"ok":true}),
    )
    .unwrap();
    assert_eq!(
        detail(&item),
        json!({"type":"unknown","input":{"foo":"bar"},"output":{"ok":true}})
    );
}

#[test]
fn claude_glob_grep() {
    let item = claude(
        map_claude_completed_tool_call,
        Some("c"),
        "Glob",
        json!({"pattern":"**/.claude/commands/paseo*"}),
        json!({"durationMs":7,"numFiles":2,"filenames":["a.txt","b.txt"],"truncated":false}),
    )
    .unwrap();
    assert_eq!(item.name, "Glob");
    assert_eq!(
        detail(&item),
        json!({"type":"search","query":"**/.claude/commands/paseo*","toolName":"glob",
            "filePaths":["a.txt","b.txt"],"numFiles":2,"durationMs":7,"truncated":false})
    );

    let item = claude(
        map_claude_completed_tool_call,
        Some("c"),
        "Grep",
        json!({"pattern":"\\\\\\\"cli\\\\\\\"\"","path":"/workspaces/paseo/packages/desktop/src","output_mode":"content","-n":true}),
        json!({"mode":"content","numFiles":1,"filenames":["src/main.rs"],"content":"12:const cli = true;","numLines":1,"numMatches":1}),
    )
    .unwrap();
    assert_eq!(
        detail(&item),
        json!({"type":"search","query":"\\\\\\\"cli\\\\\\\"\"","toolName":"grep",
            "content":"12:const cli = true;","filePaths":["src/main.rs"],"numFiles":1,"numMatches":1,"mode":"content"})
    );

    let item = claude(
        map_claude_completed_tool_call,
        Some("c"),
        "Grep",
        json!({"pattern":"MaskedView","output_mode":"files_with_matches"}),
        json!({"output":"Found 2 files\nsrc/foo.tsx\nsrc/bar.tsx"}),
    )
    .unwrap();
    assert_eq!(
        detail(&item),
        json!({"type":"search","query":"MaskedView","toolName":"grep",
            "content":"Found 2 files\nsrc/foo.tsx\nsrc/bar.tsx","numFiles":0})
    );
}

#[test]
fn claude_web_search_and_fetch() {
    let item = claude(
        map_claude_completed_tool_call,
        Some("c"),
        "WebSearch",
        json!({"query":"OpenAI latest news"}),
        json!({"query":"OpenAI latest news","results":["Top results:",
            {"tool_use_id":"toolu_123","content":[
                {"title":"OpenAI launches thing","url":"https://example.com/1"},
                {"title":"Another result","url":"https://example.com/2"}]}],
            "durationSeconds":1.5}),
    )
    .unwrap();
    assert_eq!(
        detail(&item),
        json!({"type":"search","query":"OpenAI latest news","toolName":"web_search",
            "webResults":[{"title":"OpenAI launches thing","url":"https://example.com/1"},
                          {"title":"Another result","url":"https://example.com/2"}],
            "annotations":["Top results:"],"durationSeconds":1.5})
    );

    let item = claude(
        map_claude_completed_tool_call,
        Some("c"),
        "WebFetch",
        json!({"url":"https://example.com/article","prompt":"Summarize this page"}),
        json!({"bytes":5120,"code":200,"codeText":"OK","result":"Summary text","durationMs":250,"url":"https://example.com/article"}),
    )
    .unwrap();
    assert_eq!(
        detail(&item),
        json!({"type":"fetch","url":"https://example.com/article","prompt":"Summarize this page",
            "result":"Summary text","code":200,"codeText":"OK","bytes":5120,"durationMs":250})
    );
}

#[test]
fn claude_speak_normalization() {
    for (id, name, text) in [
        (
            "claude-speak-1",
            "mcp__paseo__speak",
            "Voice response from Claude.",
        ),
        (
            "claude-speak-2",
            "mcp__paseo_voice__speak",
            "Hey! I can hear you.",
        ),
    ] {
        let item = claude(
            map_claude_completed_tool_call,
            Some(id),
            name,
            json!({"text": text}),
            json!({"ok":true}),
        )
        .unwrap();
        assert_eq!(item.name, "speak");
        assert_eq!(
            detail(&item),
            json!({"type":"unknown","input":text,"output":null})
        );
    }
}

#[test]
fn claude_drops_missing_call_id() {
    assert!(claude(
        map_claude_completed_tool_call,
        None,
        "read_file",
        json!({"file_path":"README.md"}),
        json!({"content":"hello"})
    )
    .is_none());
}

#[test]
fn claude_speak_with_empty_text_falls_back_to_unknown_raw() {
    let d = derive_claude_tool_detail(
        "speak",
        Some(&json!({"text":"  "})),
        Some(&json!({"ok":true})),
    );
    assert_eq!(
        d.to_value(),
        json!({"type":"unknown","input":{"text":"  "},"output":{"ok":true}})
    );
}

#[test]
fn claude_skill_branch() {
    let d = derive_claude_tool_detail(
        "Skill",
        Some(&json!({"skill":"diagnose"})),
        Some(&json!({"output":"ok"})),
    );
    assert_eq!(
        d.to_value(),
        json!({"type":"plain_text","label":"diagnose","text":"ok","icon":"sparkles"})
    );
}

// ---- opencode/tool-call-mapper.test.ts ---------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn oc(
    tool_name: &str,
    call_id: Option<&str>,
    status: &str,
    input: Value,
    output: Value,
    error: Option<Value>,
    metadata: Option<Map>,
) -> Option<ToolCallItem> {
    let status = Value::String(status.into());
    map_opencode_tool_call(&OpencodeToolCallParams {
        tool_name,
        call_id,
        status: Some(&status),
        input: Some(&input),
        output: Some(&output),
        error: error.as_ref(),
        metadata: metadata.as_ref(),
    })
}

fn oc_ok(tool: &str, id: &str, status: &str, input: Value, output: Value) -> ToolCallItem {
    oc(tool, Some(id), status, input, output, None, None).unwrap()
}

#[test]
fn opencode_running_shell_and_variants() {
    let item = oc_ok(
        "shell",
        "opencode-call-1",
        "running",
        json!({"command":"pwd","cwd":"/tmp/repo"}),
        Value::Null,
    );
    assert_eq!(item.status, ToolCallStatus::Running);
    assert_eq!(item.error, Some(Value::Null));
    assert_eq!(detail(&item)["command"], "pwd");

    let cases = [
        (
            "read_file",
            json!({"file_path":"README.md"}),
            json!({"type":"read","filePath":"README.md"}),
        ),
        (
            "write_file",
            json!({"file_path":"src/new.ts"}),
            json!({"type":"write","filePath":"src/new.ts"}),
        ),
        (
            "apply_patch",
            json!({"file_path":"src/index.ts"}),
            json!({"type":"edit","filePath":"src/index.ts"}),
        ),
        (
            "web_search",
            json!({"query":"opencode mapper"}),
            json!({"type":"search","query":"opencode mapper","toolName":"web_search"}),
        ),
        (
            "glob",
            json!({"pattern":"**/*.md"}),
            json!({"type":"search","query":"**/*.md","toolName":"glob"}),
        ),
        (
            "grep",
            json!({"pattern":"sendCorrelatedSessionRequest"}),
            json!({"type":"search","query":"sendCorrelatedSessionRequest","toolName":"grep"}),
        ),
    ];
    for (tool, input, want) in cases {
        let item = oc_ok(tool, "id", "running", input, Value::Null);
        assert_eq!(detail(&item), want, "{tool}");
    }
}

#[test]
fn opencode_completed_read_variants() {
    let item = oc_ok(
        "read_file",
        "opencode-call-2",
        "complete",
        json!({"file_path":"README.md"}),
        json!({"content":"hello"}),
    );
    assert_eq!(item.status, ToolCallStatus::Completed);
    assert_eq!(
        detail(&item),
        json!({"type":"read","filePath":"README.md","content":"hello"})
    );

    let item = oc_ok(
        "read_file",
        "id",
        "completed",
        json!({"file_path":"README.md"}),
        json!({"content":[{"type":"output_text","text":"alpha"},{"type":"output_text","output":"beta"}]}),
    );
    assert_eq!(detail(&item)["content"], "alpha\nbeta");

    let item = oc_ok(
        "read_file",
        "id",
        "completed",
        json!({"file_path":"README.md"}),
        json!({"data":{"content":{"type":"output_text","text":"gamma"}}}),
    );
    assert_eq!(detail(&item)["content"], "gamma");
}

#[test]
fn opencode_xml_read_output() {
    let xml = [
        "<path>/workspaces/paseo/docs/release.md</path>",
        "<type>file</type>",
        "<content>",
        "1: # Release",
        "2:",
        "3: All workspaces share one version and release together.",
        "</content>",
    ]
    .join("\n");
    let item = oc_ok(
        "read",
        "id",
        "completed",
        json!({"filePath":"/workspaces/paseo/docs/release.md"}),
        json!(xml),
    );
    assert_eq!(
        detail(&item),
        json!({"type":"read","filePath":"/workspaces/paseo/docs/release.md",
            "content":"1: # Release\n2:\n3: All workspaces share one version and release together."})
    );
}

#[test]
fn opencode_failed_requires_error() {
    let item = oc(
        "shell",
        Some("opencode-call-3"),
        "error",
        json!({"command":"false"}),
        Value::Null,
        Some(json!("command failed")),
        None,
    )
    .unwrap();
    assert_eq!(item.status, ToolCallStatus::Failed);
    assert_eq!(item.error, Some(json!("command failed")));
}

#[test]
fn opencode_write_edit_search() {
    let w = oc_ok(
        "write_file",
        "id",
        "completed",
        json!({"file_path":"src/new.ts","content":"const x = 1;"}),
        Value::Null,
    );
    assert_eq!(detail(&w)["type"], "write");
    let e = oc_ok(
        "apply_patch",
        "id",
        "completed",
        json!({"file_path":"src/index.ts","diff":"@@\\n-old\\n+new\\n"}),
        Value::Null,
    );
    assert!(detail(&e)["unifiedDiff"].as_str().unwrap().contains("@@"));

    let item = oc_ok(
        "write",
        "id",
        "completed",
        json!({"filePath":"/w/dummy.txt","content":"hello world\n"}),
        json!("Wrote file successfully."),
    );
    assert_eq!(
        detail(&item),
        json!({"type":"write","filePath":"/w/dummy.txt","content":"hello world\n"})
    );

    let item = oc_ok(
        "edit",
        "id",
        "completed",
        json!({"filePath":"/w/agent-pages.ts","oldString":"metaTitle: \"Agent page\"","newString":"metaTitle: \"Updated agent page\""}),
        json!("Edit applied successfully."),
    );
    assert_eq!(
        detail(&item),
        json!({"type":"edit","filePath":"/w/agent-pages.ts",
            "oldString":"metaTitle: \"Agent page\"","newString":"metaTitle: \"Updated agent page\""})
    );
}

#[test]
fn opencode_skill_and_grep_string() {
    let item = oc_ok(
        "skill",
        "id",
        "completed",
        json!({"name":"diagnose"}),
        json!("<skill_content name=\"diagnose\"># Skill: diagnose</skill_content>"),
    );
    assert_eq!(
        detail(&item),
        json!({"type":"plain_text","label":"diagnose","icon":"sparkles",
            "text":"<skill_content name=\"diagnose\"># Skill: diagnose</skill_content>"})
    );

    let item = oc_ok(
        "grep",
        "id",
        "completed",
        json!({"pattern":"todowrite"}),
        json!("Found 2 matches\nsrc/file.ts:\n  Line 1: todowrite"),
    );
    assert_eq!(
        detail(&item),
        json!({"type":"search","query":"todowrite","toolName":"grep",
            "content":"Found 2 matches\nsrc/file.ts:\n  Line 1: todowrite","numFiles":0})
    );
}

#[test]
fn opencode_apply_patch_text() {
    let patch = [
        "*** Begin Patch",
        "*** Delete File: /tmp/repo/src/App.tsx",
        "*** End Patch",
    ]
    .join("\n");
    let item = oc_ok(
        "apply_patch",
        "id",
        "completed",
        json!({"patchText": patch}),
        json!("Success. Updated the following files:\nD /tmp/repo/src/App.tsx"),
    );
    assert_eq!(
        detail(&item),
        json!({"type":"edit","filePath":"/tmp/repo/src/App.tsx",
            "unifiedDiff":"diff --git a//tmp/repo/src/App.tsx b//tmp/repo/src/App.tsx\n--- a//tmp/repo/src/App.tsx\n+++ /dev/null"})
    );

    let item = oc_ok(
        "patch",
        "id",
        "completed",
        json!({"patchText":"*** Begin Patch\n*** Update File: patch.txt\n@@\n-north south\n+NORTH south\n*** End Patch"}),
        json!("Success. Updated the following files:\nM patch.txt"),
    );
    assert_eq!(
        detail(&item),
        json!({"type":"edit","filePath":"patch.txt",
            "unifiedDiff":"diff --git a/patch.txt b/patch.txt\n--- a/patch.txt\n+++ b/patch.txt\n@@\n-north south\n+NORTH south"})
    );
}

#[test]
fn opencode_unknown_and_speak_not_normalized() {
    let item = oc_ok(
        "my_custom_tool",
        "id",
        "completed",
        json!({"foo":"bar"}),
        json!({"ok":true}),
    );
    assert_eq!(item.status, ToolCallStatus::Completed);
    assert_eq!(
        detail(&item),
        json!({"type":"unknown","input":{"foo":"bar"},"output":{"ok":true}})
    );

    let item = oc_ok(
        "paseo_voice.speak",
        "id",
        "completed",
        json!({"text":"Voice response from OpenCode."}),
        json!({"ok":true}),
    );
    assert_eq!(item.name, "paseo_voice.speak");
    assert_eq!(
        detail(&item),
        json!({"type":"unknown","input":{"text":"Voice response from OpenCode."},"output":{"ok":true}})
    );
}

#[test]
fn opencode_task_calls() {
    let item = oc_ok(
        "task",
        "opencode-task-running",
        "running",
        json!({"subagent_type":"explore","description":"Explore agent-tools codebase"}),
        Value::Null,
    );
    assert_eq!(
        detail(&item),
        json!({"type":"sub_agent","subAgentType":"explore","description":"Explore agent-tools codebase","log":"","actions":[]})
    );

    let item = oc_ok(
        "task",
        "id",
        "completed",
        json!({"subagent_type":"explore","description":"Explore agent-tools codebase"}),
        json!({"result":"Found the CLI entrypoint and provider registry."}),
    );
    assert_eq!(
        detail(&item)["log"],
        "Found the CLI entrypoint and provider registry."
    );

    let md: Map = json!({"sessionId":"ses_2268db431ffe299vL1bbot8R7Z"})
        .as_object()
        .unwrap()
        .clone();
    let item = oc(
        "task",
        Some("id"),
        "completed",
        json!({"subagent_type":"explore","description":"Explore current directory"}),
        json!("<task id=\"ses_2268db431ffe299vL1bbot8R7Z\">done</task>"),
        None,
        Some(md),
    )
    .unwrap();
    assert_eq!(
        detail(&item),
        json!({"type":"sub_agent","subAgentType":"explore","description":"Explore current directory",
            "childSessionId":"ses_2268db431ffe299vL1bbot8R7Z",
            "log":"<task id=\"ses_2268db431ffe299vL1bbot8R7Z\">done</task>","actions":[]})
    );

    let item = oc(
        "task",
        Some("id"),
        "aborted",
        json!({"subagent_type":"explore","description":"Explore agent-tools codebase"}),
        Value::Null,
        Some(json!("Tool execution aborted")),
        None,
    )
    .unwrap();
    assert_eq!(item.status, ToolCallStatus::Failed);
    assert_eq!(item.error, Some(json!("Tool execution aborted")));
    assert_eq!(detail(&item)["log"], "Tool execution aborted");
}

#[test]
fn opencode_task_id_from_output_text() {
    let d = derive_opencode_tool_detail(
        "task",
        Some(&json!({"subagent_type":"explore"})),
        Some(&json!("done\ntask_id: ses_AbC123 more")),
        None,
        None,
    );
    assert_eq!(d.to_value()["childSessionId"], "ses_AbC123");
}

#[test]
fn opencode_drops_missing_call_id() {
    assert!(oc(
        "read_file",
        None,
        "completed",
        json!({"file_path":"README.md"}),
        json!({"content":"hello"}),
        None,
        None
    )
    .is_none());
}

// ---- pi/tool-call-mapper.test.ts ----------------------------------------------------------------

#[test]
fn pi_bash_to_shell() {
    let call = parse_tool_args("bash", Some(&json!({"command":"echo hello"})));
    let result = parse_tool_result(Some(&json!({"output":"hello\n","exitCode":0})));
    assert_eq!(
        map_tool_detail(&call, Some(&result)).to_value(),
        json!({"type":"shell","command":"echo hello","output":"hello\n","exitCode":0})
    );
}

#[test]
fn pi_legacy_edit() {
    let call = parse_tool_args(
        "edit",
        Some(&json!({"path":"app.ts","old_string":"before","new_string":"after"})),
    );
    let result = parse_tool_result(Some(&json!({"details":{"diff":"-before\n+after"}})));
    assert_eq!(
        map_tool_detail(&call, Some(&result)).to_value(),
        json!({"type":"edit","filePath":"app.ts","oldString":"before","newString":"after","unifiedDiff":"-before\n+after"})
    );
}

#[test]
fn pi_write_and_unknown() {
    let call = parse_tool_args(
        "write",
        Some(&json!({"path":"notes.txt","content":"unchanged\n"})),
    );
    let result = parse_tool_result(Some(&json!({"text":"Wrote notes.txt"})));
    assert_eq!(
        map_tool_detail(&call, Some(&result)).to_value(),
        json!({"type":"write","filePath":"notes.txt","content":"unchanged\n"})
    );

    let call = parse_tool_args("custom_tool", Some(&json!({"value":42})));
    let result = parse_tool_result(Some(&json!({"text":"custom result"})));
    assert_eq!(
        map_tool_detail(&call, Some(&result)).to_value(),
        json!({"type":"unknown","input":{"value":42},"output":{"text":"custom result"}})
    );
}

#[test]
fn pi_exit_code_null_when_object_has_none() {
    let call = parse_tool_args("bash", Some(&json!({"command":"ls"})));
    let result = parse_tool_result(Some(&json!({"stdout":"a"})));
    assert_eq!(
        map_tool_detail(&call, Some(&result)).to_value(),
        json!({"type":"shell","command":"ls","output":"a","exitCode":null})
    );
}

// ---- Codex derive cases ------------------------------------------------------------------------

#[test]
fn codex_shell_envelope_output_and_cwd_stripping() {
    let out = "Chunk ID: abc\nWall time: 0.1 seconds\nProcess exited with code 0\nOriginal token count: 3\nOutput:\nhello\n";
    let d = derive_codex_tool_detail(
        "exec_command",
        Some(&json!({"cmd":["ls", " -la "]})),
        Some(&json!(out)),
        None,
    );
    assert_eq!(
        d.to_value(),
        json!({"type":"shell","command":"ls -la","output":"hello\n"})
    );

    let d = derive_codex_tool_detail(
        "read_file",
        Some(&json!({"path":"/repo/src/a.ts"})),
        Some(&json!({"content":"x"})),
        Some("/repo"),
    );
    assert_eq!(
        d.to_value(),
        json!({"type":"read","filePath":"src/a.ts","content":"x"})
    );

    let d = derive_codex_tool_detail(
        "apply_patch",
        Some(&json!({"path":"/repo/a.ts","patch":"@@"})),
        Some(&json!("ok")),
        Some("/repo"),
    );
    assert_eq!(
        d.to_value(),
        json!({"type":"edit","filePath":"a.ts","unifiedDiff":"@@"})
    );
}

#[test]
fn union_and_intersection_edge_cases() {
    // path and filePath disagree: the zod intersection is unmergable, the call becomes unknown
    let input = json!({"path":"a","filePath":"b","content":"x"});
    let d = derive_codex_tool_detail("write_file", Some(&input), Some(&Value::Null), None);
    assert!(d.is_unknown());
    // null in an optional string slot fails the schema
    let input = json!({"path":"a","content":null});
    let d = derive_claude_tool_detail("Write", Some(&input), Some(&Value::Null));
    assert!(d.is_unknown());
    // matched branch with no detail (empty command) is still a match -> unknown with raw payload
    let d = derive_claude_tool_detail("Bash", Some(&json!({"command":""})), Some(&Value::Null));
    assert_eq!(
        d.to_value(),
        json!({"type":"unknown","input":{"command":""},"output":null})
    );
}

#[test]
fn shell_exit_code_nullish_coalescing_skips_null() {
    let d = derive_claude_tool_detail(
        "Bash",
        Some(&json!({"command":"x"})),
        Some(&json!({"exitCode":null,"exit_code":3})),
    );
    assert_eq!(d.to_value()["exitCode"], 3);
    let d = derive_claude_tool_detail(
        "Bash",
        Some(&json!({"command":"x"})),
        Some(&json!({"exitCode":null})),
    );
    assert!(d.to_value().get("exitCode").is_none());
}

#[test]
fn diff_truncation_uses_utf16_length() {
    let big = "a".repeat(12_001);
    let d = derive_claude_tool_detail(
        "Edit",
        Some(&json!({"file_path":"f","patch":big})),
        Some(&Value::Null),
    );
    let diff = d.to_value()["unifiedDiff"].as_str().unwrap().to_string();
    assert!(diff.ends_with("\n...[truncated 1 chars]"));
    assert_eq!(diff.len(), 12_000 + "\n...[truncated 1 chars]".len());
}
