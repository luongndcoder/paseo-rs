//! Line transport under the RPC client. A real process, or a replay in tests.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::launch::Launch;

const STDERR_BUFFER_LIMIT: usize = 8192;
const GRACEFUL_SHUTDOWN: Duration = Duration::from_secs(2);
const FORCE_SHUTDOWN: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stderr_tail: String,
}

/// One JSONL connection to a provider process.
///
/// `read_line` is called from a single reader thread; `write_line` and `close` may be called
/// from any thread.
pub trait Transport: Send + Sync + 'static {
    fn write_line(&self, line: &str) -> Result<()>;
    /// Blocks for the next stdout line (without the newline). `None` means EOF.
    fn read_line(&self) -> Option<String>;
    /// Exit details; only meaningful after `read_line` returned `None`.
    fn exit_info(&self) -> ExitInfo;
    /// End stdin and terminate the process. Idempotent.
    fn close(&self);
}

pub struct ProcessTransport {
    stdin: Mutex<Option<ChildStdin>>,
    stdout: Mutex<BufReader<ChildStdout>>,
    child: Mutex<Child>,
    stderr: Arc<Mutex<Vec<u8>>>,
    pid: u32,
}

impl ProcessTransport {
    pub fn spawn(launch: &Launch) -> Result<Self> {
        let (program, args) = launch
            .argv
            .split_first()
            .ok_or_else(|| Error::Protocol("empty launch argv".into()))?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &launch.cwd {
            cmd.current_dir(cwd);
        }
        cmd.envs(&launch.env);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own process group so close() can take down subagents the provider spawned.
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|source| Error::Spawn {
            command: program.clone(),
            source,
        })?;
        let pid = child.id();
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr is piped");
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&stderr);
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = std::io::Read::read(&mut stderr_pipe, &mut buf) {
                if n == 0 {
                    break;
                }
                let mut guard = sink.lock().unwrap();
                guard.extend_from_slice(&buf[..n]);
                if guard.len() > STDERR_BUFFER_LIMIT {
                    let cut = guard.len() - STDERR_BUFFER_LIMIT;
                    guard.drain(..cut);
                }
            }
        });
        Ok(Self {
            stdin: Mutex::new(Some(stdin)),
            stdout: Mutex::new(BufReader::new(stdout)),
            child: Mutex::new(child),
            stderr,
            pid,
        })
    }

    fn signal_group(&self, signal: &str) {
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .arg(format!("-{signal}"))
                .arg("--")
                .arg(format!("-{}", self.pid))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        #[cfg(not(unix))]
        {
            let _ = signal;
        }
    }

    fn wait_for_exit(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if let Ok(Some(_)) = self.child.lock().unwrap().try_wait() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Transport for ProcessTransport {
    fn write_line(&self, line: &str) -> Result<()> {
        let mut guard = self.stdin.lock().unwrap();
        let Some(stdin) = guard.as_mut() else {
            return Err(Error::Closed("stdin is not writable".into()));
        };
        stdin.write_all(line.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
    }

    fn read_line(&self) -> Option<String> {
        let mut buf = Vec::new();
        let mut reader = self.stdout.lock().unwrap();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => None,
            Ok(_) => {
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                }
                Some(String::from_utf8_lossy(&buf).into_owned())
            }
        }
    }

    fn exit_info(&self) -> ExitInfo {
        // Poll instead of a blocking wait so close() is never stuck behind this lock.
        let deadline = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Ok(Some(status)) = self.child.lock().unwrap().try_wait() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                break None;
            }
            thread::sleep(Duration::from_millis(10));
        };
        let stderr_tail = String::from_utf8_lossy(&self.stderr.lock().unwrap())
            .trim()
            .to_string();
        let (code, signal) = match status {
            Some(status) => {
                #[cfg(unix)]
                let signal = std::os::unix::process::ExitStatusExt::signal(&status);
                #[cfg(not(unix))]
                let signal = None;
                (status.code(), signal)
            }
            None => (None, None),
        };
        ExitInfo {
            code,
            signal,
            stderr_tail,
        }
    }

    fn close(&self) {
        // A writer blocked on a full pipe holds the stdin lock; never wait for it here. Skipping the
        // stdin close sends us straight to TERM/KILL, whose broken pipe unblocks that writer.
        if let Ok(mut stdin) = self.stdin.try_lock() {
            stdin.take();
        }
        if self.wait_for_exit(Duration::from_millis(50)) {
            return;
        }
        self.signal_group("TERM");
        if self.wait_for_exit(GRACEFUL_SHUTDOWN) {
            return;
        }
        self.signal_group("KILL");
        let _ = self.child.lock().unwrap().kill();
        self.wait_for_exit(FORCE_SHUTDOWN);
    }
}

impl Drop for ProcessTransport {
    fn drop(&mut self) {
        self.close();
    }
}
