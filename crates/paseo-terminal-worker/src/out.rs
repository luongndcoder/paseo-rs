//! Single ordered stdout writer: every response/event goes through one channel so lines never
//! interleave and keep emission order.

use serde_json::Value;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Queued-but-unwritten stdout bytes above which producers block, so a stalled daemon cannot
/// make the worker buffer output without bound.
const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
struct Backlog {
    queued: AtomicUsize,
    writer_gone: AtomicBool,
}

pub enum OutMsg {
    Line(String),
    Quit,
}

#[derive(Clone)]
pub struct Out(Sender<OutMsg>, Arc<Backlog>);

impl Out {
    pub fn send(&self, value: &Value) {
        let line = value.to_string();
        // Throttle the producer (the terminal actor) while the writer is behind; this in turn
        // lets the PTY reader's pending-bytes limit push back on the child.
        while self.1.queued.load(Ordering::Relaxed) > MAX_QUEUED_BYTES
            && !self.1.writer_gone.load(Ordering::Relaxed)
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.1.queued.fetch_add(line.len() + 1, Ordering::Relaxed);
        let _ = self.0.send(OutMsg::Line(line));
    }

    pub fn quit(&self) {
        let _ = self.0.send(OutMsg::Quit);
    }
}

fn run_writer<W: Write>(rx: Receiver<OutMsg>, mut w: W, backlog: &Backlog) {
    let write_line = |line: &str, w: &mut W| {
        let _ = w.write_all(line.as_bytes());
        let _ = w.write_all(b"\n");
        backlog.queued.fetch_sub(line.len() + 1, Ordering::Relaxed);
    };
    while let Ok(msg) = rx.recv() {
        match msg {
            OutMsg::Quit => break,
            OutMsg::Line(line) => {
                write_line(&line, &mut w);
                let mut quit = false;
                while let Ok(next) = rx.try_recv() {
                    match next {
                        OutMsg::Line(l) => write_line(&l, &mut w),
                        OutMsg::Quit => {
                            quit = true;
                            break;
                        }
                    }
                }
                let _ = w.flush();
                if quit {
                    break;
                }
            }
        }
    }
    let _ = w.flush();
    backlog.writer_gone.store(true, Ordering::Relaxed);
}

pub fn spawn_stdout_writer() -> (Out, JoinHandle<()>) {
    let (tx, rx) = channel();
    let backlog = Arc::new(Backlog::default());
    let writer_backlog = backlog.clone();
    let handle = std::thread::Builder::new()
        .name("stdout-writer".into())
        .spawn(move || {
            run_writer(
                rx,
                std::io::BufWriter::new(std::io::stdout()),
                &writer_backlog,
            )
        })
        .expect("spawn stdout writer");
    (Out(tx, backlog), handle)
}
