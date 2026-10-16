//! Minimal git runner: capture stdout, optional timeout, env tweaks.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct GitOutput {
    pub exit_code: i32,
    pub stdout: String,
}

#[derive(Default)]
pub struct GitOptions<'a> {
    pub accept_exit_codes: &'a [i32],
    pub env_set: &'a [(&'a str, &'a str)],
    pub env_remove: &'a [&'a str],
    pub timeout: Option<Duration>,
}

pub fn run_git(args: &[&str], cwd: &Path, options: &GitOptions) -> Result<GitOutput, String> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in options.env_set {
        cmd.env(k, v);
    }
    for k in options.env_remove {
        cmd.env_remove(k);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    let mut out = child.stdout.take().ok_or("git stdout missing")?;
    let mut err = child.stderr.take().ok_or("git stderr missing")?;
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err.read_to_end(&mut buf);
        buf
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if options.timeout.is_some_and(|t| started.elapsed() > t) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("git {} timed out", args.join(" ")));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    let code = status.code().unwrap_or(-1);
    if code != 0 && !options.accept_exit_codes.contains(&code) {
        let msg = String::from_utf8_lossy(&stderr).trim().to_string();
        return Err(if msg.is_empty() {
            format!("git {} exited with code {code}", args.join(" "))
        } else {
            msg
        });
    }
    Ok(GitOutput {
        exit_code: code,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
    })
}
