//! `ClaudeTaskState` (`claude/task-state.ts`): folds TodoWrite and ID-based task tools into
//! canonical `todo` timeline snapshots.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

use super::text::{obj_get, read_trimmed};

#[derive(Debug, Clone)]
struct Task {
    id: Option<String>,
    text: String,
    status: String,
    completed: bool,
    active_form: Option<String>,
}

impl Task {
    fn to_value(&self) -> Value {
        let mut m = Map::new();
        if let Some(id) = &self.id {
            m.insert("id".into(), json!(id));
        }
        m.insert("text".into(), json!(self.text));
        m.insert("status".into(), json!(self.status));
        m.insert("completed".into(), json!(self.completed));
        if let Some(a) = &self.active_form {
            m.insert("activeForm".into(), json!(a));
        }
        Value::Object(m)
    }
}

fn s(v: Option<&Value>) -> Option<String> {
    read_trimmed(v)
}

fn task_status(v: Option<&Value>) -> &'static str {
    match v.and_then(Value::as_str) {
        Some("completed") => "completed",
        Some("deleted") => "deleted",
        Some("in_progress") => "in_progress",
        _ => "pending",
    }
}

fn to_task_item(v: &Value) -> Option<Task> {
    let t = v.as_object()?;
    let text = s(t.get("subject"))
        .or_else(|| s(t.get("content")))
        .or_else(|| s(t.get("text")))?;
    let status = task_status(t.get("status"));
    if status == "deleted" {
        return None;
    }
    Some(Task {
        id: s(t.get("id")).or_else(|| s(t.get("taskId"))),
        text,
        status: status.to_owned(),
        completed: status == "completed",
        active_form: s(t.get("activeForm")).or_else(|| s(t.get("active_form"))),
    })
}

fn is_task_tool(name: &str) -> bool {
    matches!(name, "TodoWrite" | "TaskCreate" | "TaskUpdate" | "TaskList")
}

#[derive(Debug)]
struct Pending {
    name: String,
    input: Value,
}

#[derive(Debug, Default)]
pub struct TaskState {
    /// Insertion-ordered like a JS `Map`.
    order: Vec<String>,
    tasks: HashMap<String, Task>,
    calls: HashMap<String, Pending>,
    applied: HashSet<String>,
}

impl TaskState {
    pub fn reset(&mut self) {
        self.order.clear();
        self.tasks.clear();
        self.calls.clear();
        self.applied.clear();
    }

    fn set(&mut self, id: String, task: Task) {
        if !self.tasks.contains_key(&id) {
            self.order.push(id.clone());
        }
        self.tasks.insert(id, task);
    }

    fn remove(&mut self, id: &str) {
        self.tasks.remove(id);
        self.order.retain(|k| k != id);
    }

    fn clear_tasks(&mut self) {
        self.order.clear();
        self.tasks.clear();
    }

    fn snapshot(&self) -> Value {
        let items: Vec<Value> = self
            .order
            .iter()
            .filter_map(|k| self.tasks.get(k))
            .map(Task::to_value)
            .collect();
        json!({"type": "todo", "items": items})
    }

    pub fn observe(&mut self, message: &Value) -> Option<Value> {
        let msg = message.as_object()?;
        let mut snapshot = None;
        let content = obj_get(message, "message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for block in content.iter().filter_map(Value::as_object) {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let (Some(id), Some(name)) = (s(block.get("id")), s(block.get("name"))) else {
                continue;
            };
            if !is_task_tool(&name) {
                continue;
            }
            let input = match block.get("input") {
                Some(v) if v.is_object() => v.clone(),
                _ => json!({}),
            };
            self.calls.insert(
                id,
                Pending {
                    name: name.clone(),
                    input: input.clone(),
                },
            );
            if name == "TodoWrite" {
                snapshot = Some(self.replace_legacy_todos(input.get("todos")));
            }
        }

        let result_id = content
            .iter()
            .filter_map(Value::as_object)
            .find(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            .and_then(|b| s(b.get("tool_use_id")));
        let Some(result_id) = result_id else {
            return snapshot;
        };
        if self.applied.contains(&result_id) {
            return snapshot;
        }
        let Some(call) = self.calls.remove(&result_id) else {
            return snapshot;
        };
        self.applied.insert(result_id);
        let structured = msg
            .get("toolUseResult")
            .filter(|v| v.is_object())
            .or_else(|| msg.get("tool_use_result").filter(|v| v.is_object()));
        self.apply_result(&call, structured).or(snapshot)
    }

    fn replace_legacy_todos(&mut self, todos: Option<&Value>) -> Value {
        self.clear_tasks();
        if let Some(Value::Array(list)) = todos {
            for (index, v) in list.iter().enumerate() {
                let Some(mut item) = to_task_item(v) else {
                    continue;
                };
                let id = item.id.clone().unwrap_or_else(|| format!("legacy:{index}"));
                item.id = Some(id.clone());
                self.set(id, item);
            }
        }
        self.snapshot()
    }

    fn apply_result(&mut self, call: &Pending, result: Option<&Value>) -> Option<Value> {
        if result.and_then(|r| r.get("success")) == Some(&Value::Bool(false)) {
            return None;
        }
        match call.name.as_str() {
            "TaskCreate" => self.apply_create(&call.input, result),
            "TaskUpdate" => self.apply_update(&call.input, result),
            "TaskList" => self.apply_list(result),
            _ => None,
        }
    }

    fn apply_create(&mut self, input: &Value, result: Option<&Value>) -> Option<Value> {
        let result_task = result.and_then(|r| r.get("task"));
        let id = s(result_task.and_then(|t| t.get("id")))
            .or_else(|| s(result.and_then(|r| r.get("taskId"))))?;
        let text =
            s(result_task.and_then(|t| t.get("subject"))).or_else(|| s(input.get("subject")))?;
        let task = Task {
            id: Some(id.clone()),
            text,
            status: "pending".into(),
            completed: false,
            active_form: s(input.get("activeForm")),
        };
        self.set(id, task);
        Some(self.snapshot())
    }

    fn apply_update(&mut self, input: &Value, result: Option<&Value>) -> Option<Value> {
        let id = s(input.get("taskId")).or_else(|| s(result.and_then(|r| r.get("taskId"))))?;
        let current = self.tasks.get(&id)?.clone();
        let status_value = input.get("status").filter(|v| !v.is_null()).or_else(|| {
            result
                .and_then(|r| r.get("statusChange"))
                .and_then(|c| c.get("to"))
                .filter(|v| !v.is_null())
        });
        let status = match status_value {
            None => current.status.clone(),
            some => task_status(some).to_owned(),
        };
        if status == "deleted" {
            self.remove(&id);
            return Some(self.snapshot());
        }
        let mut next = current;
        if let Some(t) = s(input.get("subject")) {
            next.text = t;
        }
        if let Some(a) = s(input.get("activeForm")) {
            next.active_form = Some(a);
        }
        next.completed = status == "completed";
        next.status = status;
        self.set(id, next);
        Some(self.snapshot())
    }

    fn apply_list(&mut self, result: Option<&Value>) -> Option<Value> {
        let tasks = result?.get("tasks")?.as_array()?;
        self.clear_tasks();
        for v in tasks {
            if let Some(item) = to_task_item(v) {
                if let Some(id) = item.id.clone() {
                    self.set(id, item);
                }
            }
        }
        Some(self.snapshot())
    }
}
