//! One terminal = one actor thread owning the emulator and all per-terminal state.
//!
//! Mirrors `createTerminal` (terminal.ts) plus the worker-side wiring in terminal-worker-process.ts.
//! Helper threads only move bytes: a PTY reader, a PTY writer and a child waiter, all feeding the
//! actor's command channel. Deadlines (output coalescing, title debounce, exit grace, kill waits)
//! are handled with `recv_timeout`, so no async runtime is needed.

use crate::activity::{ActivityTracker, Transition};
use crate::coalescer::{Coalescer, Flushed, FLUSH_DELAY};
use crate::emu::{into_terminal_state, Emulator, SnapshotOptions};
use crate::input_mode::InputModeTracker;
use crate::out::Out;
use crate::protocol::{
    output_message, response_ok, terminal_message, title_event, title_message, ActivityState,
    ClientMessage,
};
use crate::scanner::{osc_color_response, Control, Scanner};
use crate::spawn::SpawnedPty;
use crate::titles::{humanize_process_title, normalize_process_title};
use crate::utf8::Utf8Decoder;
use portable_pty::{ChildKiller, MasterPty, PtySize};
use regex::Regex;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const TITLE_DEBOUNCE: Duration = Duration::from_millis(150);
const EXIT_OUTPUT_LINE_LIMIT: usize = 12;
const EXIT_OUTPUT_CHAR_LIMIT: usize = 16_000;
const SCROLLBACK: usize = 1000;
/// How long after the child exits we wait for the reader to hit EOF so trailing output is not lost.
const EXIT_DRAIN_GRACE: Duration = Duration::from_millis(250);
const KILL_DISPOSE_WAIT: Duration = Duration::from_millis(1000);
const DEFAULT_GRACEFUL: u64 = 2000;
const DEFAULT_FORCE: u64 = 1000;
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;

pub enum Cmd {
    PtyData(Vec<u8>),
    PtyEof,
    PtyExit {
        code: Option<i64>,
        signal: Option<i64>,
    },
    /// Sent once, after the post-spawn settle delay: emits terminalCreated + response.
    Subscribe {
        request_id: String,
    },
    Send {
        request_id: String,
        message: ClientMessage,
    },
    FlushInput,
    GetState {
        request_id: String,
        options: Value,
    },
    Capture {
        request_id: String,
        start: Option<i64>,
        end: Option<i64>,
        strip_ansi: bool,
    },
    SetActivity {
        request_id: String,
        state: ActivityState,
    },
    ClearAttention {
        request_id: String,
    },
    Kill {
        request_id: Option<String>,
    },
    KillAndWait {
        request_id: String,
        graceful_ms: u64,
        force_ms: u64,
    },
    /// Daemon is going away: SIGKILL the group, no waiting.
    Shutdown,
}

pub struct Params {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub workspace_id: String,
    pub title: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub rows: u16,
    pub cols: u16,
}

pub struct Running {
    pub tx: Sender<Cmd>,
    pub thread: JoinHandle<()>,
}

enum KillPhase {
    Graceful,
    Force,
}

struct KillWait {
    request_id: String,
    phase: KillPhase,
    deadline: Instant,
    force_ms: u64,
}

#[derive(Clone, Copy)]
enum Sig {
    Hup,
    Kill,
}

struct Actor {
    p: Params,
    out: Out,
    self_tx: Sender<Cmd>,
    on_removed: Box<dyn Fn(&str) + Send>,
    emu: Emulator,
    decoder: Utf8Decoder,
    scanner: Scanner,
    input_mode: InputModeTracker,
    activity: ActivityTracker,
    coalescer: Coalescer,
    master: Box<dyn MasterPty + Send>,
    writer_tx: Sender<Vec<u8>>,
    #[cfg_attr(unix, allow(dead_code))]
    killer: Box<dyn ChildKiller + Send + Sync>,
    pid: Option<u32>,
    pending_bytes: Arc<AtomicUsize>,

    revision: u64,
    recent: String,

    title: Option<String>,
    title_auto: bool,
    last_emu_title: Option<String>,
    pending_title: Option<Option<String>>,
    title_deadline: Option<Instant>,

    pending_input: String,
    flush_scheduled: bool,

    subscribed: bool,
    killed: bool,
    disposed: bool,
    process_exited: bool,
    reader_eof: bool,
    exit_info: Option<Value>,
    pending_exit: Option<(Option<i64>, Option<i64>)>,
    exit_deadline: Option<Instant>,
    kill_wait: Option<KillWait>,
    dispose_deadline: Option<Instant>,
    finished: bool,
}

/// Spawns the actor and its helper threads around an already-spawned PTY.
pub fn start(
    params: Params,
    pty: SpawnedPty,
    out: Out,
    on_removed: Box<dyn Fn(&str) + Send>,
) -> std::io::Result<Running> {
    let (tx, rx) = channel::<Cmd>();
    let (writer_tx, writer_rx) = channel::<Vec<u8>>();
    let pending_bytes = Arc::new(AtomicUsize::new(0));
    let SpawnedPty {
        master,
        reader,
        mut writer,
        child,
        killer,
        pid,
    } = pty;

    std::thread::Builder::new()
        .name(format!("pty-writer-{}", params.id))
        .spawn(move || {
            while let Ok(bytes) = writer_rx.recv() {
                if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        })?;

    let reader_tx = tx.clone();
    let reader_pending = pending_bytes.clone();
    std::thread::Builder::new()
        .name(format!("pty-reader-{}", params.id))
        .spawn(move || reader_loop(reader, reader_tx, reader_pending))?;

    let waiter_tx = tx.clone();
    std::thread::Builder::new()
        .name(format!("pty-wait-{}", params.id))
        .spawn(move || {
            let mut child = child;
            let (code, signal) = match child.wait() {
                Ok(status) => match status.signal().and_then(signal_number) {
                    // node-pty reports exitCode 0 together with the signal number.
                    Some(sig) => (Some(0), Some(sig)),
                    None => (Some(i64::from(status.exit_code())), None),
                },
                Err(_) => (None, None),
            };
            let _ = waiter_tx.send(Cmd::PtyExit { code, signal });
        })?;

    let self_tx = tx.clone();
    let name = format!("term-{}", params.id);
    let thread = std::thread::Builder::new().name(name).spawn(move || {
        let mut actor = Actor::new(
            params,
            out,
            self_tx,
            on_removed,
            master,
            writer_tx,
            killer,
            pid,
            pending_bytes,
        );
        actor.run(rx);
    })?;
    Ok(Running { tx, thread })
}

fn reader_loop(mut reader: Box<dyn Read + Send>, tx: Sender<Cmd>, pending: Arc<AtomicUsize>) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => {
                let _ = tx.send(Cmd::PtyEof);
                return;
            }
            Ok(n) => {
                // Backpressure: do not let a flooding child outrun the actor without bound.
                let mut waits = 0;
                while pending.load(Ordering::Relaxed) > MAX_PENDING_BYTES && waits < 5000 {
                    std::thread::sleep(Duration::from_millis(1));
                    waits += 1;
                }
                pending.fetch_add(n, Ordering::Relaxed);
                if tx.send(Cmd::PtyData(buf[..n].to_vec())).is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(unix)]
fn signal_number(name: &str) -> Option<i64> {
    if let Some(n) = name.strip_prefix("Signal ").and_then(|n| n.parse().ok()) {
        return Some(n);
    }
    (1..=64)
        .find(|&n| {
            // SAFETY: strsignal returns a pointer to a static (or thread-local) NUL-terminated string.
            let p = unsafe { libc::strsignal(n) };
            !p.is_null() && unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy() == name
        })
        .map(i64::from)
}

#[cfg(not(unix))]
fn signal_number(_name: &str) -> Option<i64> {
    None
}

fn ansi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\x1b(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~]|\].*?(?:\x07|\x1b\\))")
            .expect("valid regex")
    })
}

/// extractLastOutputLinesFromText.
fn last_lines_from_text(text: &str, limit: usize) -> Vec<String> {
    let stripped = ansi_re().replace_all(text, "");
    let normalized = stripped.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<String> = normalized
        .split('\n')
        .map(|l| l.trim_end().to_owned())
        .collect();
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let skip = lines.len().saturating_sub(limit);
    lines.split_off(skip)
}

fn tail_chars(s: &str, n: usize) -> &str {
    let count = s.chars().count();
    if count <= n {
        return s;
    }
    let skip = count - n;
    let idx = s.char_indices().nth(skip).map_or(s.len(), |(i, _)| i);
    &s[idx..]
}

impl Actor {
    #[allow(clippy::too_many_arguments)]
    fn new(
        p: Params,
        out: Out,
        self_tx: Sender<Cmd>,
        on_removed: Box<dyn Fn(&str) + Send>,
        master: Box<dyn MasterPty + Send>,
        writer_tx: Sender<Vec<u8>>,
        killer: Box<dyn ChildKiller + Send + Sync>,
        pid: Option<u32>,
        pending_bytes: Arc<AtomicUsize>,
    ) -> Self {
        let preset = p
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned);
        let title_auto = preset.is_none();
        let mut title = preset;
        if title.is_none() {
            if let Some(command) = &p.command {
                let process_title = std::iter::once(command.as_str())
                    .chain(p.args.iter().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join(" ");
                title = humanize_process_title(&process_title)
                    .or_else(|| normalize_process_title(&process_title));
            }
        }
        let (rows, cols) = (p.rows, p.cols);
        Self {
            emu: Emulator::new(rows, cols, SCROLLBACK),
            p,
            out,
            self_tx,
            on_removed,
            decoder: Utf8Decoder::default(),
            scanner: Scanner::default(),
            input_mode: InputModeTracker::default(),
            activity: ActivityTracker::default(),
            coalescer: Coalescer::new(FLUSH_DELAY),
            master,
            writer_tx,
            killer,
            pid,
            pending_bytes,
            revision: 0,
            recent: String::new(),
            title,
            title_auto,
            last_emu_title: None,
            pending_title: None,
            title_deadline: None,
            pending_input: String::new(),
            flush_scheduled: false,
            subscribed: false,
            killed: false,
            disposed: false,
            process_exited: false,
            reader_eof: false,
            exit_info: None,
            pending_exit: None,
            exit_deadline: None,
            kill_wait: None,
            dispose_deadline: None,
            finished: false,
        }
    }

    fn run(&mut self, rx: Receiver<Cmd>) {
        while !self.finished {
            let now = Instant::now();
            let received = match self.next_deadline() {
                Some(d) => rx.recv_timeout(d.saturating_duration_since(now)),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match received {
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.run_timers(Instant::now());
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        [
            self.coalescer.deadline(),
            self.title_deadline,
            self.exit_deadline,
            self.kill_wait.as_ref().map(|k| k.deadline),
            self.dispose_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn run_timers(&mut self, now: Instant) {
        if let Some(f) = self.coalescer.on_timer(now) {
            self.emit_flushed(f);
        }
        if self.title_deadline.is_some_and(|d| now >= d) {
            self.title_deadline = None;
            if let Some(pending) = self.pending_title.take() {
                self.emit_title_change(pending);
            }
        }
        if self.exit_deadline.is_some_and(|d| now >= d) {
            self.finalize_exit();
        }
        let kill_step = self.kill_wait.as_ref().and_then(|k| {
            (now >= k.deadline).then_some((matches!(k.phase, KillPhase::Graceful), k.force_ms))
        });
        if let Some((was_graceful, force_ms)) = kill_step {
            if was_graceful {
                self.signal(Sig::Kill);
                if let Some(k) = self.kill_wait.as_mut() {
                    k.phase = KillPhase::Force;
                    k.deadline = now + Duration::from_millis(force_ms);
                }
            } else {
                // Force window elapsed: finalize like TS `kill()`; the waiter is answered on dispose.
                if let Some(k) = self.kill_wait.as_mut() {
                    k.deadline = now + Duration::from_secs(3600);
                }
                self.kill_inner();
            }
        }
        if self.dispose_deadline.is_some_and(|d| now >= d) {
            self.dispose_deadline = None;
            if !self.process_exited {
                self.signal(Sig::Kill);
            }
            self.dispose();
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::PtyData(bytes) => {
                self.pending_bytes.fetch_sub(bytes.len(), Ordering::Relaxed);
                self.on_pty_data(&bytes);
            }
            Cmd::PtyEof => {
                self.reader_eof = true;
                if self.pending_exit.is_some() {
                    self.finalize_exit();
                }
            }
            Cmd::PtyExit { code, signal } => self.on_pty_exit(code, signal),
            Cmd::Subscribe { request_id } => self.subscribe(&request_id),
            Cmd::Send {
                request_id,
                message,
            } => {
                self.on_client_message(message);
                self.respond(&request_id, None);
            }
            Cmd::FlushInput => {
                self.flush_scheduled = false;
                self.flush_pending_input();
            }
            Cmd::GetState {
                request_id,
                options,
            } => {
                // Flush buffered output first: the emulator state already includes it, so emitting
                // it afterwards (with a revision past the snapshot's) would make clients see it twice.
                self.flush_output();
                let result = if self.exit_emitted() {
                    Value::Null
                } else {
                    let mut snapshot = self.state_snapshot(&options);
                    snapshot["replayPreamble"] = json!(self.input_mode.preamble());
                    snapshot
                };
                self.respond(&request_id, Some(result));
            }
            Cmd::Capture {
                request_id,
                start,
                end,
                strip_ansi,
            } => {
                let result = self.emu.capture(start, end, strip_ansi);
                self.respond(&request_id, Some(result));
            }
            Cmd::SetActivity { request_id, state } => {
                let t = self.activity.set(state);
                self.emit_activity(t);
                self.respond(&request_id, None);
            }
            Cmd::ClearAttention { request_id } => {
                let t = self.activity.clear_attention();
                self.emit_activity(t);
                self.respond(&request_id, None);
            }
            Cmd::Kill { request_id } => {
                self.kill_inner();
                if let Some(id) = request_id {
                    self.respond(&id, None);
                }
            }
            Cmd::KillAndWait {
                request_id,
                graceful_ms,
                force_ms,
            } => self.kill_and_wait(request_id, graceful_ms, force_ms),
            Cmd::Shutdown => {
                if !self.process_exited {
                    self.signal(Sig::Kill);
                }
                self.killed = true;
                self.subscribed = true; // allow the actor to finish
                self.dispose();
                self.finished = true;
            }
        }
    }

    fn exit_emitted(&self) -> bool {
        self.exit_info.is_some()
    }

    fn respond(&self, request_id: &str, result: Option<Value>) {
        self.out.send(&response_ok(request_id, result));
    }

    fn write_pty(&self, data: impl Into<Vec<u8>>) {
        let _ = self.writer_tx.send(data.into());
    }

    // ---- output path -------------------------------------------------------------------------

    fn on_pty_data(&mut self, bytes: &[u8]) {
        if self.killed {
            return;
        }
        let text = self.decoder.decode(bytes);
        if text.is_empty() {
            return;
        }
        for response in self.input_mode.feed(&text) {
            self.write_pty(response);
        }
        self.recent.push_str(&text);
        if self.recent.len() > EXIT_OUTPUT_CHAR_LIMIT * 8 {
            let keep = tail_chars(&self.recent, EXIT_OUTPUT_CHAR_LIMIT * 2).to_owned();
            self.recent = keep;
        }

        // Feed the emulator in segments so cursor-position replies see the state at the query.
        let data = text.as_bytes();
        let mut pos = 0;
        for found in self.scanner.feed(&text) {
            self.emu.feed(&data[pos..found.end]);
            pos = found.end;
            self.on_control(found.control);
        }
        self.emu.feed(&data[pos..]);
        self.poll_title();

        self.revision += 1;
        if self.subscribed && !self.disposed {
            if let Some(f) = self.coalescer.handle(&text, self.revision, Instant::now()) {
                self.emit_flushed(f);
            }
        }
    }

    fn cursor(&self) -> (i64, i64) {
        let opts: SnapshotOptions =
            serde_json::from_value(json!({"scrollbackLines": 0})).unwrap_or_default();
        let state = into_terminal_state(self.emu.snapshot(&opts));
        let get = |k: &str| state["cursor"][k].as_i64().unwrap_or(0);
        (get("row"), get("col"))
    }

    fn on_control(&mut self, control: Control) {
        match control {
            Control::Da1 => self.write_pty("\x1b[?62;4;22c"),
            Control::Dsr5 => self.write_pty("\x1b[0n"),
            Control::Dsr6 => {
                let (row, col) = self.cursor();
                self.write_pty(format!("\x1b[{};{}R", row + 1, col + 1));
            }
            Control::DecDsr6 => {
                let (row, col) = self.cursor();
                self.write_pty(format!("\x1b[?{};{}R", row + 1, col + 1));
            }
            Control::OscColorQuery(code) => {
                if let Some(resp) = osc_color_response(code) {
                    self.write_pty(resp);
                }
            }
            Control::CommandFinished(exit_code) => {
                if self.subscribed && !self.disposed {
                    self.flush_output();
                    self.out.send(&json!({
                        "type": "terminalCommandFinished",
                        "terminalId": self.p.id,
                        "info": {"exitCode": exit_code},
                    }));
                }
            }
        }
    }

    fn emit_flushed(&self, f: Flushed) {
        if self.subscribed && !self.disposed {
            self.out
                .send(&output_message(&self.p.id, &f.payload, f.revision));
        }
    }

    fn flush_output(&mut self) {
        if let Some(f) = self.coalescer.flush(Instant::now()) {
            self.emit_flushed(f);
        }
    }

    // ---- title -------------------------------------------------------------------------------

    fn poll_title(&mut self) {
        if !self.title_auto || self.disposed || self.killed {
            return;
        }
        let current = self.emu.title();
        if current == self.last_emu_title {
            return;
        }
        self.last_emu_title = current.clone();
        self.pending_title = Some(current.filter(|t| !t.trim().is_empty()));
        self.title_deadline = Some(Instant::now() + TITLE_DEBOUNCE);
    }

    fn emit_title_change(&mut self, next: Option<String>) {
        if self.title == next {
            return;
        }
        self.title = next;
        if self.subscribed && !self.disposed {
            self.flush_output();
            self.out
                .send(&title_event(&self.p.id, self.title.as_deref()));
            self.out
                .send(&title_message(&self.p.id, self.title.as_deref()));
        }
    }

    // ---- state -------------------------------------------------------------------------------

    fn state_snapshot(&self, options: &Value) -> Value {
        let opts: SnapshotOptions = serde_json::from_value(options.clone()).unwrap_or_default();
        let mut state = into_terminal_state(self.emu.snapshot(&opts));
        if let Some(obj) = state.as_object_mut() {
            match &self.title {
                Some(t) => obj.insert("title".into(), json!(t)),
                None => obj.remove("title"),
            };
        }
        json!({"state": state, "revision": self.revision})
    }

    fn info_json(&self) -> Value {
        let mut info = json!({
            "id": self.p.id,
            "name": self.p.name,
            "cwd": self.p.cwd,
            "workspaceId": self.p.workspace_id,
            "activity": self.activity.snapshot().to_json(),
        });
        if let Some(t) = &self.title {
            info["title"] = json!(t);
        }
        info
    }

    fn subscribe(&mut self, request_id: &str) {
        self.subscribed = true;
        let state = self.state_snapshot(&json!({}))["state"].clone();
        let terminal = self.info_json();
        self.out.send(&json!({
            "type": "terminalCreated",
            "terminal": terminal,
            "state": state,
        }));
        self.respond(
            request_id,
            Some(json!({"terminal": self.info_json(), "state": state})),
        );
        if let Some(t) = &self.title {
            self.out.send(&title_event(&self.p.id, Some(t)));
        }
        // Creation already carries an authoritative state; a second snapshot could clobber a
        // resize the caller issues right away, so subscribers only get "ready".
        self.out.send(&terminal_message(
            &self.p.id,
            json!({
                "type": "snapshotReady",
                "revision": self.revision,
                "replayPreamble": self.input_mode.preamble(),
            }),
        ));
        if let Some(info) = &self.exit_info {
            self.out.send(&json!({
                "type": "terminalExit",
                "terminalId": self.p.id,
                "info": info,
            }));
        }
        if self.disposed {
            self.finished = true;
        }
    }

    // ---- input -------------------------------------------------------------------------------

    fn on_client_message(&mut self, message: ClientMessage) {
        if self.killed {
            return;
        }
        match message {
            ClientMessage::Input { data } => {
                if data == "\x03" || data == "\x1b" {
                    let t = self.activity.interrupt();
                    self.emit_activity(t);
                }
                self.pending_input.push_str(&data);
                if !self.flush_scheduled {
                    self.flush_scheduled = true;
                    let _ = self.self_tx.send(Cmd::FlushInput);
                }
            }
            ClientMessage::Resize { rows, cols } => {
                self.flush_pending_input();
                let (rows, cols) = (rows.max(1), cols.max(1));
                self.emu.resize(rows, cols);
                let _ = self.master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                self.revision += 1;
            }
            ClientMessage::Mouse { .. } => {}
        }
    }

    fn flush_pending_input(&mut self) {
        let data = std::mem::take(&mut self.pending_input);
        if data.is_empty() || self.killed || self.disposed {
            return;
        }
        self.write_pty(data);
    }

    fn emit_activity(&self, transition: Option<Transition>) {
        let Some((snapshot, previous)) = transition else {
            return;
        };
        if self.disposed || self.killed || !self.subscribed {
            return;
        }
        self.out.send(&json!({
            "type": "terminalActivityChange",
            "terminalId": self.p.id,
            "activity": snapshot.to_json(),
            "previous": previous.to_json(),
        }));
    }

    // ---- lifecycle ---------------------------------------------------------------------------

    fn signal(&self, sig: Sig) {
        #[cfg(unix)]
        {
            let Some(pid) = self.pid else { return };
            let s = match sig {
                Sig::Hup => libc::SIGHUP,
                Sig::Kill => libc::SIGKILL,
            };
            // The PTY child is a session leader, so pgid == pid: signal the whole group, falling
            // back to the leader alone.
            // SAFETY: plain kill(2) calls with a pid we spawned and have not yet seen exit.
            unsafe {
                if libc::kill(-(pid as i32), s) != 0 {
                    libc::kill(pid as i32, s);
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (sig, self.pid);
            let mut killer = self.killer.clone_killer();
            let _ = killer.kill();
        }
    }

    fn build_exit_info(&self, status: Option<(Option<i64>, Option<i64>)>) -> Value {
        let (code, signal) = status.unwrap_or((None, None));
        let mut lines: Vec<String> = self
            .emu
            .capture(None, None, true)
            .get("lines")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        while lines.first().is_some_and(|l| l.trim().is_empty()) {
            lines.remove(0);
        }
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
        if lines.is_empty() {
            lines = last_lines_from_text(
                tail_chars(&self.recent, EXIT_OUTPUT_CHAR_LIMIT),
                EXIT_OUTPUT_LINE_LIMIT,
            );
        } else {
            let skip = lines.len().saturating_sub(EXIT_OUTPUT_LINE_LIMIT);
            lines = lines.split_off(skip);
        }
        json!({
            "exitCode": code,
            "signal": signal.filter(|s| *s > 0),
            "lastOutputLines": lines,
        })
    }

    fn emit_exit(&mut self, info: Value) {
        if self.exit_info.is_some() {
            return;
        }
        self.exit_info = Some(info.clone());
        self.flush_output();
        if self.subscribed {
            self.out.send(&json!({
                "type": "terminalExit",
                "terminalId": self.p.id,
                "info": info,
            }));
        }
        (self.on_removed)(&self.p.id);
    }

    fn on_pty_exit(&mut self, code: Option<i64>, signal: Option<i64>) {
        self.process_exited = true;
        if self.killed {
            // Already reported by kill(); the real status is no longer interesting.
            self.dispose();
            return;
        }
        self.pending_exit = Some((code, signal));
        if self.reader_eof {
            self.finalize_exit();
        } else {
            self.exit_deadline = Some(Instant::now() + EXIT_DRAIN_GRACE);
        }
    }

    fn finalize_exit(&mut self) {
        self.exit_deadline = None;
        let Some(status) = self.pending_exit.take() else {
            return;
        };
        self.killed = true;
        let info = self.build_exit_info(Some(status));
        self.emit_exit(info);
        self.dispose();
    }

    /// TS `kill()`: SIGHUP, report the exit immediately, dispose once the process is gone (or
    /// after 1s, escalating to SIGKILL).
    fn kill_inner(&mut self) {
        if !self.killed {
            self.killed = true;
            if !self.process_exited {
                self.signal(Sig::Hup);
            }
            let info = self.build_exit_info(None);
            self.emit_exit(info);
        }
        if self.process_exited {
            self.dispose();
            return;
        }
        if self.dispose_deadline.is_none() {
            self.dispose_deadline = Some(Instant::now() + KILL_DISPOSE_WAIT);
        }
    }

    fn kill_and_wait(&mut self, request_id: String, graceful_ms: u64, force_ms: u64) {
        if self.process_exited || self.disposed {
            self.kill_inner();
            self.respond(&request_id, None);
            return;
        }
        if self.kill_wait.is_some() {
            // A second concurrent wait just follows the same teardown.
            self.respond(&request_id, None);
            return;
        }
        self.signal(Sig::Hup);
        self.kill_wait = Some(KillWait {
            request_id,
            phase: KillPhase::Graceful,
            deadline: Instant::now() + Duration::from_millis(graceful_ms),
            force_ms,
        });
    }

    fn dispose(&mut self) {
        if self.disposed {
            return;
        }
        self.disposed = true;
        self.activity.clear();
        self.pending_input.clear();
        self.recent.clear();
        self.coalescer.dispose();
        self.title_deadline = None;
        self.pending_title = None;
        self.dispose_deadline = None;
        self.exit_deadline = None;
        (self.on_removed)(&self.p.id);
        if let Some(k) = self.kill_wait.take() {
            self.respond(&k.request_id, None);
        }
        // Before the create ack the actor must stay alive to deliver it.
        if self.subscribed {
            self.finished = true;
        }
    }
}

pub const DEFAULT_GRACEFUL_MS: u64 = DEFAULT_GRACEFUL;
pub const DEFAULT_FORCE_MS: u64 = DEFAULT_FORCE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_lines_strip_ansi_and_trim() {
        let text = "\n\x1b[31mred\x1b[0m\r\nline2   \r\n\r\n";
        assert_eq!(last_lines_from_text(text, 12), vec!["red", "line2"]);
        assert_eq!(last_lines_from_text("a\nb\nc", 2), vec!["b", "c"]);
    }

    #[test]
    fn tail_chars_respects_boundaries() {
        assert_eq!(tail_chars("héllo", 3), "llo");
        assert_eq!(tail_chars("ab", 5), "ab");
    }

    #[cfg(unix)]
    #[test]
    fn maps_signal_names() {
        assert_eq!(signal_number("Signal 9"), Some(9));
        // SAFETY: strsignal on a valid signal returns a NUL-terminated string.
        let name = unsafe { std::ffi::CStr::from_ptr(libc::strsignal(libc::SIGKILL)) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(signal_number(&name), Some(9));
    }
}
