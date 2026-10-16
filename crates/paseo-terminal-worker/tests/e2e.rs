//! Spawns the real binary and speaks JSONL to it. Unix only (uses /bin/sh).
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

struct Worker {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
    seen: Vec<Value>,
}

impl Worker {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_paseo-terminal-worker"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn worker");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let v: Value = serde_json::from_str(&line).expect("stdout line is JSON");
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            lines,
            seen: Vec::new(),
        }
    }

    fn send(&mut self, v: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    /// Reads until `pred` matches a message; returns it. Every message is kept in `seen`.
    fn wait_for(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(v) = self.seen.iter().find(|v| pred(v)) {
                return v.clone();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(v) => self.seen.push(v),
                Err(_) => panic!("timed out waiting for {what}; seen: {:#?}", self.seen),
            }
        }
    }

    fn response(&mut self, request_id: &str) -> Value {
        let id = request_id.to_owned();
        self.wait_for(&format!("response {id}"), move |v| {
            v["type"] == "response" && v["requestId"] == id.as_str()
        })
    }

    fn create(&mut self, request_id: &str, options: Value) -> Value {
        self.send(json!({"type":"createTerminal","requestId":request_id,"options":options}));
        self.response(request_id)
    }

    fn output_text(&self, terminal_id: &str) -> String {
        self.seen
            .iter()
            .filter(|v| v["type"] == "terminalMessage" && v["terminalId"] == terminal_id)
            .filter(|v| v["message"]["type"] == "output")
            .filter_map(|v| v["message"]["data"].as_str())
            .collect()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn index_of(seen: &[Value], pred: impl Fn(&Value) -> bool) -> usize {
    seen.iter().position(pred).expect("message present")
}

#[test]
fn runs_command_and_reports_output_and_exit() {
    let mut w = Worker::start();
    let resp = w.create(
        "r1",
        json!({"id":"t1","cwd":"/tmp","workspaceId":"ws","command":"sh",
               "args":["-c","printf 'hello\\n'; exit 3"]}),
    );
    assert_eq!(resp["ok"], true, "{resp}");
    assert_eq!(resp["result"]["terminal"]["id"], "t1");
    assert_eq!(resp["result"]["terminal"]["name"], "Terminal 1");
    assert_eq!(resp["result"]["terminal"]["workspaceId"], "ws");
    assert_eq!(
        resp["result"]["terminal"]["title"],
        "sh -c printf 'hello\\n'; exit 3"
    );
    let exit = w.wait_for("exit", |v| v["type"] == "terminalExit");
    assert_eq!(exit["terminalId"], "t1");
    assert_eq!(exit["info"]["exitCode"], 3);
    assert!(exit["info"]["signal"].is_null());
    assert!(exit["info"]["lastOutputLines"].is_array());

    let created = index_of(&w.seen, |v| v["type"] == "terminalCreated");
    let response = index_of(&w.seen, |v| v["type"] == "response");
    assert!(created < response, "terminalCreated precedes the response");
    assert!(
        w.output_text("t1").contains("hello")
            || !exit["info"]["lastOutputLines"]
                .as_array()
                .unwrap()
                .is_empty()
    );

    // Exited terminals are removed.
    w.send(json!({"type":"getTerminalState","requestId":"s","terminalId":"t1"}));
    assert!(w.response("s")["result"].is_null());
}

#[test]
fn validation_errors() {
    let mut w = Worker::start();
    let r = w.create("a", json!({"cwd":"/tmp"}));
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"], "workspaceId is required");
    let r = w.create(
        "b",
        json!({"cwd":"/tmp","workspaceId":"w","command":"definitely-not-a-cmd"}),
    );
    assert_eq!(r["error"], "definitely-not-a-cmd: command not found");
    let r = w.create("c", json!({"cwd":"rel","workspaceId":"w"}));
    assert_eq!(r["error"], "cwd must be absolute path");
    w.send(json!({"type":"bogus","requestId":"x"}));
    assert_eq!(w.response("x")["ok"], false);
}

#[test]
fn interactive_input_state_capture_and_activity() {
    let mut w = Worker::start();
    let resp = w.create(
        "c",
        json!({"id":"t","cwd":"/tmp","workspaceId":"ws","command":"cat","rows":10,"cols":40,
               "env":{"FOO":"bar"}}),
    );
    assert_eq!(resp["ok"], true, "{resp}");
    w.wait_for("snapshotReady", |v| v["message"]["type"] == "snapshotReady");
    assert_eq!(resp["result"]["state"]["rows"], 10);

    w.send(json!({"type":"send","requestId":"i","terminalId":"t",
                  "message":{"type":"input","data":"ping\r"}}));
    assert_eq!(w.response("i")["ok"], true);
    w.wait_for("echoed output", |v| {
        v["message"]["type"] == "output"
            && v["message"]["data"]
                .as_str()
                .is_some_and(|d| d.contains("ping"))
    });
    let out = w.wait_for("output", |v| v["message"]["type"] == "output");
    assert!(out["message"]["revision"].is_number());

    w.send(
        json!({"type":"getTerminalState","requestId":"g","terminalId":"t",
                  "options":{"scrollbackLines":5}}),
    );
    let state = w.response("g");
    assert!(state["result"]["revision"].as_u64().unwrap() >= 1);
    assert!(state["result"]["replayPreamble"].is_string());
    assert!(state["result"]["state"]["grid"].is_array());

    w.send(json!({"type":"captureTerminal","requestId":"cap","terminalId":"t","stripAnsi":true}));
    assert!(w.response("cap")["result"]["totalLines"].is_number());

    w.send(json!({"type":"send","requestId":"rz","terminalId":"t",
                  "message":{"type":"resize","rows":5,"cols":20}}));
    w.response("rz");
    w.send(json!({"type":"getTerminalState","requestId":"g2","terminalId":"t"}));
    assert_eq!(w.response("g2")["result"]["state"]["rows"], 5);

    w.send(json!({"type":"setActivity","requestId":"a1","terminalId":"t","state":"working"}));
    w.response("a1");
    w.send(json!({"type":"setActivity","requestId":"a2","terminalId":"t","state":"idle"}));
    w.response("a2");
    let change = w.wait_for("finished activity", |v| {
        v["type"] == "terminalActivityChange" && v["activity"]["attentionReason"] == "finished"
    });
    assert_eq!(change["previous"]["state"], "working");
    w.send(json!({"type":"clearAttention","requestId":"ca","terminalId":"t"}));
    w.response("ca");
    w.wait_for("attention cleared", |v| {
        v["type"] == "terminalActivityChange"
            && v["activity"]["attentionReason"].is_null()
            && v["previous"]["attentionReason"] == "finished"
    });

    w.send(json!({"type":"killTerminal","requestId":"k","terminalId":"t"}));
    w.response("k");
    let exit = w.wait_for("exit", |v| v["type"] == "terminalExit");
    assert!(exit["info"]["exitCode"].is_null());
}

#[test]
fn passes_env_and_activity_hooks_to_child() {
    let mut w = Worker::start();
    w.send(json!({"type":"registerCwdEnv","requestId":"e","cwd":"/tmp","env":{"INHERITED":"yes","FOO":"inherited"}}));
    w.response("e");
    w.create(
        "c",
        json!({"id":"tid","cwd":"/tmp/","workspaceId":"ws","command":"sh",
               "args":["-c","sleep 0.3; echo \"$FOO|$INHERITED|$PASEO_TERMINAL_ID|$PASEO_ACTIVITY_TOKEN|$PASEO_TERMINAL_ACTIVITY_URL|$PASEO_WORKSPACE_ID|$TERM|$TERM_PROGRAM\"; sleep 30"],
               "env":{"FOO":"bar"},"activityToken":"tok","activityUrl":"http://x/y"}),
    );
    w.wait_for("env output", |v| {
        v["message"]["data"]
            .as_str()
            .is_some_and(|d| d.contains("|tid|"))
    });
    let text = w.output_text("tid");
    assert!(
        text.contains("bar|yes|tid|tok|http://x/y|ws|xterm-256color|kitty"),
        "got {text:?}"
    );
}

#[test]
fn kill_and_wait_escalates_to_sigkill_and_kill_all_responds() {
    let mut w = Worker::start();
    // Ignores SIGHUP, so the graceful phase must time out and escalate.
    w.create(
        "c",
        json!({"id":"stubborn","cwd":"/tmp","workspaceId":"ws","command":"sh",
               "args":["-c","trap '' HUP; while true; do sleep 1; done"]}),
    );
    let started = Instant::now();
    w.send(
        json!({"type":"killTerminalAndWait","requestId":"kw","terminalId":"stubborn",
                  "options":{"gracefulTimeoutMs":200,"forceTimeoutMs":2000}}),
    );
    assert_eq!(w.response("kw")["ok"], true);
    assert!(started.elapsed() < Duration::from_secs(5));
    w.wait_for("exit", |v| {
        v["type"] == "terminalExit" && v["terminalId"] == "stubborn"
    });

    w.create(
        "c2",
        json!({"id":"a","cwd":"/tmp","workspaceId":"ws","command":"cat"}),
    );
    w.create(
        "c3",
        json!({"id":"b","cwd":"/tmp","workspaceId":"ws","command":"cat"}),
    );
    w.send(json!({"type":"killAll","requestId":"ka"}));
    w.response("ka");
    w.wait_for("exit a", |v| {
        v["type"] == "terminalExit" && v["terminalId"] == "a"
    });
    w.wait_for("exit b", |v| {
        v["type"] == "terminalExit" && v["terminalId"] == "b"
    });
}

#[test]
fn stdin_eof_kills_children_and_exits() {
    let mut w = Worker::start();
    let marker = format!("paseo-eof-marker-{}", std::process::id());
    w.create(
        "c",
        json!({"id":"t","cwd":"/tmp","workspaceId":"ws","command":"sh",
               "args":["-c", format!("exec sleep 300 # {marker}")]}),
    );
    w.stdin.take(); // EOF
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = w.child.try_wait().unwrap() {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "worker did not exit on stdin EOF"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success());
    let ps = Command::new("pgrep")
        .args(["-f", &marker])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&ps.stdout).trim().is_empty(),
        "child survived EOF"
    );
}

#[test]
fn answers_terminal_queries() {
    let mut w = Worker::start();
    // The child prints whatever the terminal answers to DA1 and DSR 5.
    w.create(
        "c",
        json!({"id":"q","cwd":"/tmp","workspaceId":"ws","command":"sh",
               "args":["-c","sleep 0.3; stty raw -echo; printf '\\033[c'; dd bs=1 count=11 2>/dev/null | od -c | head -1; printf '\\033[5n'; dd bs=1 count=4 2>/dev/null | od -c | head -1; sleep 30"]}),
    );
    w.wait_for("DA1 and DSR5 replies", |v| {
        v["message"]["data"]
            .as_str()
            .is_some_and(|d| d.contains("0   n"))
    });
    let text = w.output_text("q");
    // od -c renders ESC [ ? 6 2 ; 4 ; 2 2 c with padded columns.
    assert!(
        text.contains("6   2   ;   4   ;   2   2   c"),
        "DA1 reply missing: {text:?}"
    );
}

#[test]
fn zsh_shell_integration_reports_command_finished() {
    let integration = std::env::var("PASEO_SHELL_INTEGRATION_DIR").ok();
    let zsh = ["/bin/zsh", "/usr/bin/zsh"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists());
    let (Some(integration), Some(zsh)) = (integration, zsh) else {
        eprintln!("skipping: zsh or shell-integration dir missing");
        return;
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_paseo-terminal-worker"))
        .env("SHELL", zsh)
        .env("PASEO_SHELL_INTEGRATION_DIR", integration)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let stdin = child.stdin.take();
    let (tx, lines) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(serde_json::from_str::<Value>(&line).unwrap());
        }
    });
    let mut w = Worker {
        child,
        stdin,
        lines,
        seen: Vec::new(),
    };
    let r = w.create("c", json!({"id":"z","cwd":"/tmp","workspaceId":"ws"}));
    assert_eq!(r["ok"], true, "{r}");
    std::thread::sleep(Duration::from_millis(800));
    w.send(json!({"type":"send","requestId":"i","terminalId":"z",
                  "message":{"type":"input","data":"false\r"}}));
    let finished = w.wait_for("command finished", |v| {
        v["type"] == "terminalCommandFinished"
    });
    assert_eq!(finished["info"]["exitCode"], 1);
}
