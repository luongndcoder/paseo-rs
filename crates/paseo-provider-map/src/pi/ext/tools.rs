//! Tool-name and task-list extensions: `pi-mcp-adapter`, `rpiv-todo`, `pi-example-todo`,
//! `pi-goal-x`.

use paseo_tool_detail::js::js_trim;
use serde_json::{json, Value};

use super::{ExtMapping, ExtSession, ExtToolCall};
use crate::pi::util::*;
use paseo_tool_detail::detail::pi::PiToolResult;

// ---- pi-mcp-adapter -----------------------------------------------------------------------

pub(super) struct PiMcpAdapter;

fn non_empty_trim(v: Option<&Value>) -> Option<String> {
    let t = js_trim(v?.as_str()?);
    (!t.is_empty()).then(|| t.to_string())
}

impl ExtSession for PiMcpAdapter {
    fn map_tool_call(&mut self, call: &ExtToolCall) -> Option<ExtMapping> {
        if call.tool_name != "mcp" {
            return None;
        }
        let named = |name: String| {
            Some(ExtMapping {
                name: Some(name),
                ..ExtMapping::default()
            })
        };
        if matches!(call.result, PiToolResult::Object(_)) {
            if let Some(d) = call.details().and_then(Value::as_object) {
                let server = non_empty_trim(d.get("server"));
                let tool = non_empty_trim(d.get("tool"));
                if let (Some(server), Some(tool)) = (server, tool) {
                    return named(format!("{server}.{tool}"));
                }
            }
        }
        let args = call.args.as_object()?;
        let tool = non_empty_trim(args.get("tool"));
        let server = non_empty_trim(args.get("server"));
        if let (Some(tool), Some(server)) = (&tool, &server) {
            let prefix = format!("{server}_");
            let short = tool.strip_prefix(&prefix).unwrap_or(tool);
            return named(format!("{server}.{short}"));
        }
        if let Some(tool) = tool {
            let mut parts = tool.split('_');
            let prefix = parts.next().unwrap_or("");
            let rest: Vec<&str> = parts.collect();
            if !prefix.is_empty() && !rest.is_empty() {
                return named(format!("{prefix}.{}", rest.join("_")));
            }
        }
        None
    }
}

// ---- todo extensions ----------------------------------------------------------------------

fn completed_object_result(call: &ExtToolCall) -> Option<&Value> {
    if call.status != "completed" || !matches!(call.result, PiToolResult::Object(_)) {
        return None;
    }
    call.details()
}

fn todo_timeline(items: Vec<Value>) -> ExtMapping {
    ExtMapping {
        timeline: vec![json!({"type": "todo", "items": items})],
        ..ExtMapping::default()
    }
}

pub(super) struct RpivTodo;

impl ExtSession for RpivTodo {
    fn map_tool_call(&mut self, call: &ExtToolCall) -> Option<ExtMapping> {
        if call.tool_name != "todo" {
            return None;
        }
        let d = completed_object_result(call)?.as_object()?;
        if !matches!(
            req_str(d, "action")?,
            "create" | "update" | "list" | "get" | "delete" | "clear"
        ) {
            return None;
        }
        d.get("params")?.as_object()?;
        req_int(d, "nextId")?;
        let error = opt_str(d, "error")?;
        let mut items = Vec::new();
        for t in req_array(d, "tasks")? {
            let tm = t.as_object()?;
            let id = req_int(tm, "id")?;
            let subject = req_str(tm, "subject")?;
            let status = req_str(tm, "status")?;
            if !matches!(status, "pending" | "in_progress" | "completed" | "deleted") {
                return None;
            }
            let active_form = opt_str(tm, "activeForm")?;
            if status == "deleted" {
                continue;
            }
            let mut item = json!({
                "id": id.to_string(),
                "text": subject,
                "status": status,
                "completed": status == "completed",
            });
            if let Some(a) = active_form.filter(|a| !a.is_empty()) {
                item["activeForm"] = json!(a);
            }
            items.push(item);
        }
        Some(if error.is_some_and(|e| !e.is_empty()) {
            ExtMapping::default()
        } else {
            todo_timeline(items)
        })
    }
}

pub(super) struct PiExampleTodo;

impl ExtSession for PiExampleTodo {
    fn map_tool_call(&mut self, call: &ExtToolCall) -> Option<ExtMapping> {
        if call.tool_name != "todo" {
            return None;
        }
        let d = completed_object_result(call)?.as_object()?;
        if !matches!(req_str(d, "action")?, "list" | "add" | "toggle" | "clear") {
            return None;
        }
        req_int(d, "nextId")?;
        let error = opt_str(d, "error")?;
        let mut items = Vec::new();
        for t in req_array(d, "todos")? {
            let tm = t.as_object()?;
            let id = req_int(tm, "id")?;
            let text = req_str(tm, "text")?;
            let done = req_bool(tm, "done")?;
            items.push(json!({
                "id": id.to_string(),
                "text": text,
                "status": if done { "completed" } else { "pending" },
                "completed": done,
            }));
        }
        Some(if error.is_some_and(|e| !e.is_empty()) {
            ExtMapping::default()
        } else {
            todo_timeline(items)
        })
    }
}

// ---- pi-goal-x ----------------------------------------------------------------------------

struct GoalTask {
    id: String,
    title: String,
    status: String,
    subtasks: Vec<GoalTask>,
}

fn parse_goal_task(v: &Value) -> Option<GoalTask> {
    let m = v.as_object()?;
    let status = req_str(m, "status")?;
    if !matches!(status, "pending" | "complete" | "skipped") {
        return None;
    }
    let subtasks = match m.get("subtasks") {
        None => Vec::new(),
        Some(Value::Array(a)) => a.iter().map(parse_goal_task).collect::<Option<Vec<_>>>()?,
        Some(_) => return None,
    };
    Some(GoalTask {
        id: req_str(m, "id")?.to_string(),
        title: req_str(m, "title")?.to_string(),
        status: status.to_string(),
        subtasks,
    })
}

fn goal_items(tasks: &[GoalTask], current: Option<&str>, out: &mut Vec<Value>) {
    for task in tasks {
        let status = if task.status != "pending" {
            "completed"
        } else if Some(task.id.as_str()) == current {
            "in_progress"
        } else {
            "pending"
        };
        out.push(json!({
            "id": task.id,
            "text": task.title,
            "status": status,
            "completed": task.status != "pending",
        }));
        goal_items(&task.subtasks, current, out);
    }
}

pub(super) struct PiGoalX;

impl ExtSession for PiGoalX {
    fn map_tool_call(&mut self, call: &ExtToolCall) -> Option<ExtMapping> {
        if !matches!(
            call.tool_name.as_str(),
            "create_goal" | "get_goal" | "update_goal" | "set_goal_tasks" | "update_goal_task"
        ) {
            return None;
        }
        let d = completed_object_result(call)?.as_object()?;
        if d.get("version")?.as_i64() != Some(3) {
            return None;
        }
        let goal = match d.get("goal")? {
            Value::Null => return None,
            Value::Object(g) => g,
            _ => return None,
        };
        let current = opt_str(goal, "currentTaskId")?;
        let task_list = match goal.get("taskList") {
            None => return None,
            Some(Value::Object(t)) => t,
            Some(_) => return None,
        };
        let tasks = req_array(task_list, "tasks")?
            .iter()
            .map(parse_goal_task)
            .collect::<Option<Vec<_>>>()?;
        let mut items = Vec::new();
        goal_items(&tasks, current, &mut items);
        Some(todo_timeline(items))
    }
}
