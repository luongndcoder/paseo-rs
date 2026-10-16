//! `git` process runner. Mirrors `run-git-command.ts`: same global `-c` flags,
//! same error text, same stdout/stderr bounds. The TS process scheduler and
//! runtime metrics window are out of scope; callers may bound concurrency
//! themselves.

use std::collections::HashMap;
use std::future::Future;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::error::{GitError, Result};
use crate::scheduler::{
    resolve_git_process_policy, GitProcessPolicy, GitProcessPriority, GitProcessScheduler,
};

pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 20 * 1024 * 1024;
const STDERR_LIMIT: usize = 2048;

/// `GIT_OPTIONAL_LOCKS=0` plus `LC_ALL=C` (checkout-git.ts READ_ONLY_GIT_ENV).
pub const READ_ONLY_ENV: &[(&str, &str)] = &[("GIT_OPTIONAL_LOCKS", "0"), ("LC_ALL", "C")];

#[derive(Debug, Clone)]
pub struct GitOptions {
    pub cwd: String,
    pub input: Option<Vec<u8>>,
    pub env_overlay: Vec<(String, String)>,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
    pub accept_exit_codes: Vec<i32>,
}

impl GitOptions {
    pub fn new(cwd: impl Into<String>) -> Self {
        Self {
            cwd: cwd.into(),
            input: None,
            env_overlay: Vec::new(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            accept_exit_codes: vec![0],
        }
    }

    pub fn read_only(mut self) -> Self {
        self.env_overlay.extend(
            READ_ONLY_ENV
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        );
        self
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env_overlay.push((key.to_string(), value.to_string()));
        self
    }

    pub fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }

    pub fn max_output(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    pub fn accept(mut self, codes: &[i32]) -> Self {
        self.accept_exit_codes = codes.to_vec();
        self
    }

    pub fn input(mut self, input: Vec<u8>) -> Self {
        self.input = Some(input);
        self
    }
}

#[derive(Debug, Clone)]
pub struct GitOutput<T = String> {
    pub stdout: T,
    pub stderr: String,
    pub truncated: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

/// Recorded `git` invocations, for tests that assert command counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCommand {
    pub args: Vec<String>,
    pub cwd: String,
}

static METRICS: Mutex<Vec<(u64, Vec<RecordedCommand>)>> = Mutex::new(Vec::new());
static NEXT_RECORDER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Records every `git` invocation made while it is alive. Independent
/// recorders see the same commands, so concurrent users do not interfere;
/// filter by `cwd` to isolate one repository.
pub struct GitCommandMetrics {
    id: u64,
}

pub fn start_git_command_metrics() -> GitCommandMetrics {
    let id = NEXT_RECORDER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    METRICS.lock().unwrap().push((id, Vec::new()));
    GitCommandMetrics { id }
}

impl GitCommandMetrics {
    pub fn stop(self) -> Vec<RecordedCommand> {
        let mut recorders = METRICS.lock().unwrap();
        recorders
            .iter()
            .position(|(id, _)| *id == self.id)
            .map(|i| recorders.remove(i).1)
            .unwrap_or_default()
    }
}

impl Drop for GitCommandMetrics {
    fn drop(&mut self) {
        let mut recorders = METRICS.lock().unwrap();
        if let Some(i) = recorders.iter().position(|(id, _)| *id == self.id) {
            recorders.remove(i);
        }
    }
}

fn scheduler_cell() -> &'static Mutex<Arc<GitProcessScheduler>> {
    static CELL: OnceLock<Mutex<Arc<GitProcessScheduler>>> = OnceLock::new();
    CELL.get_or_init(|| {
        let policy = resolve_git_process_policy(&|k| std::env::var(k).ok(), None);
        Mutex::new(Arc::new(GitProcessScheduler::new(policy)))
    })
}

/// Replace the process-wide admission policy (`configureGitProcessPolicy`).
pub fn configure_git_process_policy(policy: GitProcessPolicy) {
    *scheduler_cell().lock().unwrap() = Arc::new(GitProcessScheduler::new(policy));
}

pub fn git_process_scheduler() -> Arc<GitProcessScheduler> {
    scheduler_cell().lock().unwrap().clone()
}

tokio::task_local! {
    static GIT_PRIORITY: GitProcessPriority;
}

/// Run `operation` with every git command inside it admitted at `priority`
/// (`runWithGitCommandPriority`).
pub async fn run_with_git_command_priority<F: Future>(
    priority: GitProcessPriority,
    operation: F,
) -> F::Output {
    GIT_PRIORITY.scope(priority, operation).await
}

fn format_command(args: &[String]) -> String {
    let mut out = String::from("git");
    for a in args {
        out.push(' ');
        out.push_str(a);
    }
    out
}

fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP".into(),
        2 => "SIGINT".into(),
        6 => "SIGABRT".into(),
        9 => "SIGKILL".into(),
        13 => "SIGPIPE".into(),
        15 => "SIGTERM".into(),
        n => format!("SIG{n}"),
    }
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

/// Run git and return raw stdout bytes.
pub async fn run_git_bytes<S: AsRef<str>>(
    args: &[S],
    options: &GitOptions,
) -> Result<GitOutput<Vec<u8>>> {
    let args: Vec<String> = args.iter().map(|a| a.as_ref().to_string()).collect();
    let command = format_command(&args);
    for (_, commands) in METRICS.lock().unwrap().iter_mut() {
        commands.push(RecordedCommand {
            args: args.clone(),
            cwd: options.cwd.clone(),
        });
    }

    let priority = GIT_PRIORITY.try_with(|p| *p).unwrap_or_default();
    let _permit = git_process_scheduler().acquire(priority).await;

    let mut cmd = Command::new("git");
    // `core.quotepath=false` keeps non-ASCII paths raw; `core.fsmonitor=false`
    // stops repository config from launching a command.
    cmd.args(["-c", "core.quotepath=false", "-c", "core.fsmonitor=false"])
        .args(&args)
        .current_dir(&options.cwd)
        .stdin(if options.input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let overlay: HashMap<&str, &str> = options
        .env_overlay
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    for (k, v) in overlay {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().map_err(|e| GitError::Other(e.to_string()))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    if let (Some(input), Some(mut stdin)) = (options.input.clone(), child.stdin.take()) {
        tokio::spawn(async move {
            // The command may exit before reading all input (EPIPE is fine).
            let _ = stdin.write_all(&input).await;
            let _ = stdin.shutdown().await;
        });
    }

    let max = options.max_output_bytes;
    let work = async {
        let stderr_task = tokio::spawn(async move {
            let mut kept = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let room = STDERR_LIMIT.saturating_sub(kept.len());
                        kept.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
            kept
        });

        let mut out = Vec::new();
        let mut truncated = false;
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match stdout.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let remaining = max.saturating_sub(out.len());
                    if n > remaining {
                        out.extend_from_slice(&chunk[..remaining]);
                        truncated = true;
                        let _ = child.start_kill();
                        break;
                    }
                    out.extend_from_slice(&chunk[..n]);
                }
            }
        }
        let status = child.wait().await;
        let err_bytes = stderr_task.await.unwrap_or_default();
        (out, truncated, status, err_bytes)
    };

    let timeout = Duration::from_millis(options.timeout_ms);
    let (out, truncated, status, err_bytes) = match tokio::time::timeout(timeout, work).await {
        Ok(v) => v,
        Err(_) => {
            return Err(GitError::Other(format!(
                "Git command timed out after {}ms: {command}",
                options.timeout_ms
            )));
        }
    };
    let status = status.map_err(|e| GitError::Other(e.to_string()))?;
    let exit_code = status.code();
    let signal = exit_signal(&status);
    let stderr_text = String::from_utf8_lossy(&err_bytes).into_owned();

    if !truncated && !exit_code.is_some_and(|c| options.accept_exit_codes.contains(&c)) {
        let preview = if stderr_text.trim().is_empty() {
            "(no stderr)".to_string()
        } else {
            stderr_text.trim().to_string()
        };
        let message = format!(
            "Git command failed: {command} (exit code: {}, signal: {})\n{preview}",
            exit_code.map_or("null".to_string(), |c| c.to_string()),
            signal.map_or("none".to_string(), signal_name),
        );
        return Err(GitError::Command {
            message,
            stderr: stderr_text,
            stdout: String::from_utf8_lossy(&out).into_owned(),
        });
    }

    Ok(GitOutput {
        stdout: out,
        stderr: stderr_text,
        truncated,
        exit_code,
        signal,
    })
}

/// Run git and decode stdout as (lossy) UTF-8, like Node's `toString("utf8")`.
pub async fn run_git<S: AsRef<str>>(args: &[S], options: &GitOptions) -> Result<GitOutput> {
    let out = run_git_bytes(args, options).await?;
    Ok(GitOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: out.stderr,
        truncated: out.truncated,
        exit_code: out.exit_code,
        signal: out.signal,
    })
}

/// Shorthand for the read-only invocations that dominate checkout-git.ts.
pub async fn run_read<S: AsRef<str>>(args: &[S], cwd: &str) -> Result<GitOutput> {
    run_git(args, &GitOptions::new(cwd).read_only()).await
}

/// Build a `[String; N]` argument list from heterogeneous string-likes
/// (call `.to_vec()` when the list must grow).
#[macro_export]
macro_rules! args {
    ($($x:expr),* $(,)?) => { [$(String::from($x)),*] };
}

/// Like [`args!`] but yields a growable `Vec<String>`.
#[macro_export]
macro_rules! argv {
    ($($x:expr),* $(,)?) => { vec![$(String::from($x)),*] };
}
