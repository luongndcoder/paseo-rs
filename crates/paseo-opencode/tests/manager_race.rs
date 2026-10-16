//! Concurrent acquire/release against the real `OpenCodeServerManager`, using a fake `opencode`
//! (a python script that serves `/global/event`). Invariants: a held lease is always on a live
//! server, and once every lease is released no server process is left behind.
#![cfg(unix)]

use paseo_opencode::manager::{Acquire, OpenCodeServerManager};
use paseo_opencode::options::OpenCodeOptions;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Barrier;

const FAKE: &str = r#"
import sys, os, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
args = sys.argv[1:]
port = int(args[args.index("--port") + 1])
with open("pids", "a") as f:
    f.write(str(os.getpid()) + "\n")
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        try:
            self.wfile.write(b'data: {"payload":{"type":"server.connected","properties":{}}}\n\n')
            self.wfile.flush()
            while True:
                time.sleep(0.2)
                self.wfile.write(b': hb\n\n')
                self.wfile.flush()
        except Exception:
            pass
srv = ThreadingHTTPServer(("127.0.0.1", port), H)
srv.daemon_threads = True
print("opencode server listening on http://127.0.0.1:%d" % port, flush=True)
srv.serve_forever()
"#;

fn manager(dir: &Path) -> Arc<OpenCodeServerManager> {
    let script = dir.join("fake_opencode.py");
    std::fs::write(&script, FAKE).unwrap();
    let home = dir.join("home");
    OpenCodeServerManager::new(
        OpenCodeOptions::new()
            .binary("python3")
            .prefix_args([script.to_str().unwrap()])
            .home_dir(&home)
            .unsecured()
            .startup_timeout(Duration::from_secs(10)),
    )
}

fn pids(dir: &Path) -> Vec<i32> {
    std::fs::read_to_string(dir.join("home/pids"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.parse().ok())
        .collect()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes for existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn assert_all_dead(dir: &Path) {
    for _ in 0..40 {
        if pids(dir).iter().all(|p| !alive(*p)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("leaked server processes: {:?}", pids(dir));
}

fn tempdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("paseo-oc-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_current_release_acquire_never_hands_out_dead_server() {
    let dir = tempdir("race");
    let mgr = manager(&dir);
    let workers = 4;
    let rounds = 6;
    let barrier = Arc::new(Barrier::new(workers));
    let tasks: Vec<_> = (0..workers)
        .map(|_| {
            let (mgr, barrier) = (mgr.clone(), barrier.clone());
            tokio::spawn(async move {
                barrier.wait().await;
                for _ in 0..rounds {
                    let h = mgr.acquire_arc(Acquire::Current).await.unwrap();
                    assert!(!h.events.is_closed(), "leased a closed server");
                    tokio::task::yield_now().await;
                    assert!(!h.events.is_closed(), "server stopped under a live lease");
                    h.release().await;
                    // release is idempotent
                    h.release().await;
                }
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    assert!(!pids(&dir).is_empty());
    assert_all_dead(&dir).await;
    mgr.shutdown_all().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_leases_share_one_server_until_last_release() {
    let dir = tempdir("share");
    let mgr = manager(&dir);
    let a = mgr.acquire_arc(Acquire::Current).await.unwrap();
    let b = mgr.acquire_arc(Acquire::Current).await.unwrap();
    assert_eq!(a.url, b.url);
    a.release().await;
    assert!(!b.events.is_closed(), "first release must not stop a shared server");
    b.release().await;
    assert!(b.events.is_closed());
    assert_eq!(pids(&dir).len(), 1);
    assert_all_dead(&dir).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_retires_old_server_until_its_users_release() {
    let dir = tempdir("new");
    let mgr = manager(&dir);
    let old = mgr.acquire_arc(Acquire::Current).await.unwrap();
    let fresh = mgr.acquire_arc(Acquire::New).await.unwrap();
    assert_ne!(old.url, fresh.url);
    assert!(!old.events.is_closed(), "retired server lives while leased");
    old.release().await;
    assert!(old.events.is_closed(), "retired server stops on last release");
    assert!(!fresh.events.is_closed());
    fresh.release().await;
    assert_all_dead(&dir).await;
    let _ = std::fs::remove_dir_all(&dir);
}
