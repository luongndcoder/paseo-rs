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
    spawn_writer(std::io::BufWriter::new(std::io::stdout()))
}

fn spawn_writer<W: Write + Send + 'static>(w: W) -> (Out, JoinHandle<()>) {
    let (tx, rx) = channel();
    let backlog = Arc::new(Backlog::default());
    let writer_backlog = backlog.clone();
    let handle = std::thread::Builder::new()
        .name("stdout-writer".into())
        .spawn(move || run_writer(rx, w, &writer_backlog))
        .expect("spawn stdout writer");
    (Out(tx, backlog), handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    /// Writer that blocks every write until released: a paused stdout consumer.
    struct Gated {
        open: Arc<(Mutex<bool>, std::sync::Condvar)>,
        written: Arc<AtomicUsize>,
    }

    impl Write for Gated {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let (lock, cv) = &*self.open;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
            self.written.fetch_add(buf.len(), Ordering::Relaxed);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn paused_consumer_bounds_queued_bytes_and_flood_resumes_after_release() {
        let open = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let written = Arc::new(AtomicUsize::new(0));
        let (out, handle) = spawn_writer(Gated {
            open: open.clone(),
            written: written.clone(),
        });

        let payload = "x".repeat(64 * 1024);
        let line_len = json!({ "d": payload }).to_string().len() + 1;
        let sent = Arc::new(AtomicUsize::new(0));
        let producer = {
            let (out, sent) = (out.clone(), sent.clone());
            std::thread::spawn(move || {
                // The "infinite writer": runs until told to stop at 2x the limit.
                while sent.load(Ordering::Relaxed) < 2 * MAX_QUEUED_BYTES + 4 * line_len {
                    out.send(&json!({ "d": payload }));
                    sent.fetch_add(line_len, Ordering::Relaxed);
                }
            })
        };

        // Let the producer run into the throttle and stay there.
        let mut peak = 0;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(25));
            peak = peak.max(out.1.queued.load(Ordering::Relaxed));
        }
        // A single producer checks before adding, so overshoot is at most one line.
        assert!(
            peak <= MAX_QUEUED_BYTES + line_len,
            "queued {peak} exceeded limit {MAX_QUEUED_BYTES}"
        );
        assert!(
            peak > MAX_QUEUED_BYTES / 2,
            "flood never built a backlog: {peak}"
        );
        let stalled = sent.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            stalled,
            sent.load(Ordering::Relaxed),
            "producer not blocked"
        );
        assert!(!producer.is_finished());

        // Resume the consumer: backlog drains, producer finishes, nothing was dropped.
        *open.0.lock().unwrap() = true;
        open.1.notify_all();
        producer.join().unwrap();
        out.quit();
        handle.join().unwrap();
        assert_eq!(out.1.queued.load(Ordering::Relaxed), 0);
        assert_eq!(
            written.load(Ordering::Relaxed),
            sent.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn writer_exit_releases_blocked_producer() {
        let open = Arc::new((Mutex::new(true), std::sync::Condvar::new()));
        let (out, handle) = spawn_writer(Gated {
            open,
            written: Arc::new(AtomicUsize::new(0)),
        });
        // Simulate a saturated backlog, then a dead writer: producers must not hang.
        out.1.queued.store(MAX_QUEUED_BYTES + 1, Ordering::Relaxed);
        out.quit();
        handle.join().unwrap();
        out.send(&json!({"after": "quit"}));
    }
}
