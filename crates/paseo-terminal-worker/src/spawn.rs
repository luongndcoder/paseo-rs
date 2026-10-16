//! Command resolution and PTY spawn (portable-pty; ConPTY on Windows).

use crate::env::Env;
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub struct SpawnedPty {
    pub master: Box<dyn MasterPty + Send>,
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub child: Box<dyn Child + Send + Sync>,
    pub killer: Box<dyn ChildKiller + Send + Sync>,
    pub pid: Option<u32>,
}

pub fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("ComSpec")
            .or_else(|_| std::env::var("COMSPEC"))
            .unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".into())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into())
    }
}

/// `TerminalCommandNotFoundError` message.
pub fn command_not_found(command: &str) -> String {
    format!("{command}: command not found")
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// POSIX only: execve inside the PTY helper reports a missing command as a silent exit 1, so
/// resolve against the terminal's own cwd/PATH up front (assertTerminalCommandCanStart).
pub fn assert_command_can_start(
    command: Option<&str>,
    cwd: &Path,
    env: &Env,
) -> Result<(), String> {
    let Some(command) = command else {
        return Ok(());
    };
    if cfg!(windows) {
        return Ok(());
    }
    if command.contains('/') {
        return if is_executable_file(&cwd.join(command)) {
            Ok(())
        } else {
            Err(command_not_found(command))
        };
    }
    let path_value = env
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("path"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let found = path_value
        .split(':')
        .filter(|d| !d.is_empty())
        .any(|d| is_executable_file(&cwd.join(d).join(command)));
    if found {
        Ok(())
    } else {
        Err(command_not_found(command))
    }
}

pub struct ResolvedCommand {
    pub command: String,
    pub args: Vec<String>,
}

const CMD_METACHARS: &str = "()%!^\"`<>&|;, *?";

fn escape_cmd_metachars(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if CMD_METACHARS.contains(c) {
            out.push('^');
        }
        out.push(c);
    }
    out
}

/// "qntm" cmd.exe argument escaping, as in terminal.ts `escapeCmdExeArgument`.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn escape_cmd_exe_argument(raw: &str) -> String {
    let arg = raw.replace("\r\n", " ").replace(['\r', '\n'], " ");
    let mut escaped = String::with_capacity(arg.len() + 2);
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                escaped.push_str(&"\\".repeat(backslashes * 2));
                escaped.push_str("\\\"");
                backslashes = 0;
            }
            other => {
                escaped.push_str(&"\\".repeat(backslashes));
                escaped.push(other);
                backslashes = 0;
            }
        }
    }
    escaped.push_str(&"\\".repeat(backslashes * 2));
    let quoted = format!("\"{escaped}\"");
    escape_cmd_metachars(&escape_cmd_metachars(&quoted))
}

#[cfg_attr(not(windows), allow(dead_code))]
fn find_windows_executable(command: &str) -> Option<PathBuf> {
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let exts: Vec<&str> = pathext.split(';').filter(|e| !e.is_empty()).collect();
    let direct = Path::new(command);
    let dirs: Vec<PathBuf> = if direct.components().count() > 1 {
        vec![PathBuf::new()]
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect()
    };
    for dir in dirs {
        let base = dir.join(command);
        if base.extension().is_some() && base.is_file() {
            return Some(base);
        }
        for ext in &exts {
            let mut candidate = base.clone().into_os_string();
            candidate.push(ext);
            let candidate = PathBuf::from(candidate);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Windows: CreateProcess ignores PATHEXT, and `.cmd`/`.bat` shims need `cmd.exe /c`.
/// Other platforms pass through unchanged. NOTE: the cmd.exe line is passed as one argument; it
/// relies on portable-pty quoting and is unverified on Windows.
pub fn resolve_spawn_command(command: &str, args: &[String]) -> ResolvedCommand {
    #[cfg(windows)]
    {
        if let Some(resolved) = find_windows_executable(command) {
            let ext = resolved
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let resolved_str = resolved.to_string_lossy().into_owned();
            if ext == "cmd" || ext == "bat" {
                let mut parts = vec![escape_cmd_metachars(&resolved_str)];
                parts.extend(args.iter().map(|a| escape_cmd_exe_argument(a)));
                return ResolvedCommand {
                    command: default_shell(),
                    args: vec![
                        "/d".into(),
                        "/s".into(),
                        "/c".into(),
                        format!("\"{}\"", parts.join(" ")),
                    ],
                };
            }
            return ResolvedCommand {
                command: resolved_str,
                args: args.to_vec(),
            };
        }
    }
    ResolvedCommand {
        command: command.to_owned(),
        args: args.to_vec(),
    }
}

pub fn spawn_pty(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &Env,
    rows: u16,
    cols: u16,
) -> anyhow::Result<SpawnedPty> {
    let pair = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    cmd.cwd(cwd);
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    let child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let killer = child.clone_killer();
    let pid = child.process_id();
    Ok(SpawnedPty {
        master: pair.master,
        reader,
        writer,
        child,
        killer,
        pid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_cmd_arguments() {
        assert_eq!(escape_cmd_exe_argument("a b"), "^^^\"a^^^ b^^^\"");
        assert_eq!(escape_cmd_exe_argument("x\ny"), "^^^\"x^^^ y^^^\"");
    }

    #[cfg(unix)]
    #[test]
    fn missing_command_is_reported() {
        let mut env = Env::new();
        env.insert("PATH".into(), "/bin:/usr/bin".into());
        assert!(assert_command_can_start(Some("sh"), Path::new("/"), &env).is_ok());
        assert_eq!(
            assert_command_can_start(Some("definitely-not-a-cmd"), Path::new("/"), &env),
            Err("definitely-not-a-cmd: command not found".into())
        );
        assert!(assert_command_can_start(Some("./nope"), Path::new("/tmp"), &env).is_err());
        assert!(assert_command_can_start(None, Path::new("/"), &env).is_ok());
    }
}
