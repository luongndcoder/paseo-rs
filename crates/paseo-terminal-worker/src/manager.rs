//! Terminal registry and request dispatch (worker-side `createTerminalManager` + the request
//! switch in terminal-worker-process.ts).

use crate::env::{absolutize, build_terminal_environment, Env, EnvConfig};
use crate::out::Out;
use crate::protocol::{
    response_err, response_ok, ClientMessage, CreateOptions, KillAndWaitOptions, Request,
};
use crate::spawn::{assert_command_can_start, default_shell, resolve_spawn_command, spawn_pty};
use crate::terminal::{self, Cmd, Params, DEFAULT_FORCE_MS, DEFAULT_GRACEFUL_MS};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

/// Post-spawn settle delay (terminal.ts "Small delay to let shell initialize").
const SETTLE_DELAY: Duration = Duration::from_millis(50);

#[derive(Clone)]
struct Handle {
    tx: Sender<Cmd>,
    cwd: String,
}

pub struct Manager {
    out: Out,
    config: EnvConfig,
    terminals: Mutex<HashMap<String, Handle>>,
    actors: Mutex<Vec<(JoinHandle<()>, Sender<Cmd>)>>,
    default_env: Mutex<Vec<(PathBuf, Env)>>,
    create_lock: Mutex<()>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// posix.isAbsolute || win32.isAbsolute (assertAbsolutePath).
fn is_absolute_any(cwd: &str) -> bool {
    let b = cwd.as_bytes();
    cwd.starts_with('/')
        || cwd.starts_with('\\')
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'\\' || b[2] == b'/'))
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf).expect("OS randomness available");
    buf
}

fn uuid_v4() -> String {
    let mut b = random_bytes::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

impl Manager {
    pub fn new(out: Out, config: EnvConfig) -> Arc<Self> {
        Arc::new(Self {
            out,
            config,
            terminals: Mutex::new(HashMap::new()),
            actors: Mutex::new(Vec::new()),
            default_env: Mutex::new(Vec::new()),
            create_lock: Mutex::new(()),
        })
    }

    fn remove(&self, id: &str) {
        lock(&self.terminals).remove(id);
    }

    fn handle_for(&self, id: &str) -> Option<Handle> {
        lock(&self.terminals).get(id).cloned()
    }

    fn resolve_default_env(&self, cwd: &str) -> Option<Env> {
        let normalized = absolutize(Path::new(cwd));
        let roots = lock(&self.default_env);
        roots
            .iter()
            .filter(|(root, _)| normalized.starts_with(root))
            .max_by_key(|(root, _)| root.as_os_str().len())
            .map(|(_, env)| env.clone())
    }

    /// Sends `cmd` to the terminal's actor; when the terminal is gone answers with `fallback`.
    fn send_or(&self, id: &str, cmd: Cmd, fallback: Value) {
        match self.handle_for(id) {
            Some(h) if h.tx.send(cmd).is_ok() => {}
            _ => self.out.send(&fallback),
        }
    }

    pub fn dispatch(self: &Arc<Self>, request: Request) {
        match request {
            Request::CreateTerminal {
                request_id,
                options,
            } => {
                let this = self.clone();
                let spawned = std::thread::Builder::new()
                    .name("create-terminal".into())
                    .spawn(move || this.create_terminal(&request_id, options));
                if let Err(e) = spawned {
                    tracing::error!("failed to spawn create thread: {e}");
                }
            }
            Request::RegisterCwdEnv {
                request_id,
                cwd,
                env,
            } => {
                if !is_absolute_any(&cwd) {
                    self.out
                        .send(&response_err(&request_id, "cwd must be absolute path"));
                    return;
                }
                let root = absolutize(Path::new(&cwd));
                let env: Env = env.into_iter().collect();
                let mut roots = lock(&self.default_env);
                roots.retain(|(r, _)| *r != root);
                roots.push((root, env));
                self.out.send(&response_ok(&request_id, None));
            }
            Request::SetActivity {
                request_id,
                terminal_id,
                state,
            } => self.send_or(
                &terminal_id,
                Cmd::SetActivity {
                    request_id: request_id.clone(),
                    state,
                },
                response_ok(&request_id, None),
            ),
            Request::ClearAttention {
                request_id,
                terminal_id,
            } => self.send_or(
                &terminal_id,
                Cmd::ClearAttention {
                    request_id: request_id.clone(),
                },
                response_ok(&request_id, None),
            ),
            Request::KillTerminal {
                request_id,
                terminal_id,
            } => {
                // Removal from the registry happens at kill time, like manager.killTerminal.
                let handle = lock(&self.terminals).remove(&terminal_id);
                let delivered = handle.is_some_and(|h| {
                    h.tx.send(Cmd::Kill {
                        request_id: Some(request_id.clone()),
                    })
                    .is_ok()
                });
                if !delivered {
                    self.out.send(&response_ok(&request_id, None));
                }
            }
            Request::KillTerminalAndWait {
                request_id,
                terminal_id,
                options,
            } => {
                let KillAndWaitOptions {
                    graceful_timeout_ms,
                    force_timeout_ms,
                } = options.unwrap_or_default();
                self.send_or(
                    &terminal_id,
                    Cmd::KillAndWait {
                        request_id: request_id.clone(),
                        graceful_ms: graceful_timeout_ms.unwrap_or(DEFAULT_GRACEFUL_MS),
                        force_ms: force_timeout_ms.unwrap_or(DEFAULT_FORCE_MS),
                    },
                    response_ok(&request_id, None),
                );
            }
            Request::GetTerminalState {
                request_id,
                terminal_id,
                options,
            } => self.send_or(
                &terminal_id,
                Cmd::GetState {
                    request_id: request_id.clone(),
                    options: options.unwrap_or(Value::Null),
                },
                response_ok(&request_id, Some(Value::Null)),
            ),
            Request::CaptureTerminal {
                request_id,
                terminal_id,
                start,
                end,
                strip_ansi,
            } => self.send_or(
                &terminal_id,
                Cmd::Capture {
                    request_id: request_id.clone(),
                    start,
                    end,
                    strip_ansi: strip_ansi.unwrap_or(true),
                },
                response_ok(&request_id, Some(json!({"lines": [], "totalLines": 0}))),
            ),
            Request::KillAll { request_id } => {
                let handles: Vec<Handle> = lock(&self.terminals).drain().map(|(_, h)| h).collect();
                for h in handles {
                    let _ = h.tx.send(Cmd::Kill { request_id: None });
                }
                self.out.send(&response_ok(&request_id, None));
            }
            Request::Send {
                request_id,
                terminal_id,
                message,
            } => self.send_or(
                &terminal_id,
                Cmd::Send {
                    request_id: request_id.clone(),
                    message: message as ClientMessage,
                },
                response_ok(&request_id, None),
            ),
        }
    }

    fn create_terminal(self: &Arc<Self>, request_id: &str, options: CreateOptions) {
        // Serialized like the Node worker: keeps spawn failures attributable to one request.
        let _guard = lock(&self.create_lock);
        match self.spawn_terminal(options) {
            Ok(tx) => {
                std::thread::sleep(SETTLE_DELAY);
                if tx
                    .send(Cmd::Subscribe {
                        request_id: request_id.to_owned(),
                    })
                    .is_err()
                {
                    self.out.send(&response_err(
                        request_id,
                        "terminal stopped during creation",
                    ));
                }
            }
            Err(message) => self.out.send(&response_err(request_id, &message)),
        }
    }

    fn spawn_terminal(self: &Arc<Self>, options: CreateOptions) -> Result<Sender<Cmd>, String> {
        let workspace_id = options
            .workspace_id
            .clone()
            .filter(|w| !w.is_empty())
            .ok_or_else(|| "workspaceId is required".to_owned())?;
        if !is_absolute_any(&options.cwd) {
            return Err("cwd must be absolute path".into());
        }

        let existing_at_cwd = lock(&self.terminals)
            .values()
            .filter(|h| h.cwd == options.cwd)
            .count();
        let name = options
            .name
            .clone()
            .unwrap_or_else(|| format!("Terminal {}", existing_at_cwd + 1));
        let id = options.id.clone().unwrap_or_else(uuid_v4);
        let token = options
            .activity_token
            .clone()
            .unwrap_or_else(|| URL_SAFE_NO_PAD.encode(random_bytes::<32>()));

        let mut overlay = Env::new();
        if let Some(inherited) = self.resolve_default_env(&options.cwd) {
            overlay.extend(inherited);
        }
        if let Some(env) = &options.env {
            overlay.extend(env.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        overlay.insert("PASEO_TERMINAL_ID".into(), id.clone());
        overlay.insert("PASEO_ACTIVITY_TOKEN".into(), token);
        if let Some(url) = options.activity_url.as_ref().filter(|u| !u.is_empty()) {
            overlay.insert("PASEO_TERMINAL_ACTIVITY_URL".into(), url.clone());
        }
        overlay.insert("PASEO_WORKSPACE_ID".into(), workspace_id.clone());

        let args = options.args.clone().unwrap_or_default();
        let (program, program_args) = match &options.command {
            Some(command) => {
                let r = resolve_spawn_command(command, &args);
                (r.command, r.args)
            }
            None => (default_shell(), Vec::new()),
        };
        let env = build_terminal_environment(&program, &overlay, &self.config, std::env::vars());
        let cwd = Path::new(&options.cwd);
        assert_command_can_start(options.command.as_deref(), cwd, &env)?;

        let rows = options.rows.unwrap_or(24).max(1);
        let cols = options.cols.unwrap_or(80).max(1);
        let pty = spawn_pty(&program, &program_args, cwd, &env, rows, cols)
            .map_err(|e| format!("{e:#}"))?;

        let params = Params {
            id: id.clone(),
            name,
            cwd: options.cwd.clone(),
            workspace_id,
            title: options.title.clone(),
            command: options.command.clone(),
            args,
            rows,
            cols,
        };
        let this = self.clone();
        let running = terminal::start(
            params,
            pty,
            self.out.clone(),
            Box::new(move |terminal_id| this.remove(terminal_id)),
        )
        .map_err(|e| format!("failed to start terminal actor: {e}"))?;

        lock(&self.terminals).insert(
            id,
            Handle {
                tx: running.tx.clone(),
                cwd: options.cwd,
            },
        );
        let mut actors = lock(&self.actors);
        actors.retain(|(t, _)| !t.is_finished());
        actors.push((running.thread, running.tx.clone()));
        Ok(running.tx)
    }

    /// stdin EOF: kill every child (including ones mid-teardown) and wait for the actors.
    pub fn shutdown(&self) {
        lock(&self.terminals).clear();
        let actors: Vec<_> = lock(&self.actors).drain(..).collect();
        for (_, tx) in &actors {
            let _ = tx.send(Cmd::Shutdown);
        }
        for (thread, _) in actors {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_path_check_accepts_posix_and_win32() {
        assert!(is_absolute_any("/tmp"));
        assert!(is_absolute_any("C:\\x"));
        assert!(is_absolute_any("c:/x"));
        assert!(!is_absolute_any("rel/path"));
        assert!(!is_absolute_any(""));
    }

    #[test]
    fn uuid_shape() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
    }
}
