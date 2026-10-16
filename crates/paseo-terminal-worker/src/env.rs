//! Terminal child environment, mirroring `buildTerminalEnvironment` in terminal.ts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub type Env = BTreeMap<String, String>;

const RUNTIME_CONTROL_ENV_KEYS: &[&str] = &[
    "PASEO_NODE_ENV",
    "PASEO_DESKTOP_MANAGED",
    "PASEO_SUPERVISED",
    "ELECTRON_RUN_AS_NODE",
    "ELECTRON_NO_ATTACH_CONSOLE",
    "ESBUILD_BINARY_PATH",
    // Worker-private configuration; the Node worker never had these in its own env.
    "PASEO_SHELL_INTEGRATION_DIR",
    "PASEO_TERMINAL_WORKER_LOG",
];

#[cfg(windows)]
const PATH_DELIMITER: char = ';';
#[cfg(not(windows))]
const PATH_DELIMITER: char = ':';

/// Process-level configuration resolved once at startup.
#[derive(Debug, Clone, Default)]
pub struct EnvConfig {
    /// Absolute path of the paseo CLI executable (`PASEO_CLI`), if the shim provided one.
    pub cli_executable: Option<PathBuf>,
    /// Directory holding `.zshenv` and `paseo-integration.zsh`.
    pub zsh_integration_dir: Option<PathBuf>,
}

impl EnvConfig {
    pub fn from_process_env() -> Self {
        let cli_executable = std::env::var("PASEO_CLI")
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .map(|v| absolutize(Path::new(&v)));
        Self {
            cli_executable,
            zsh_integration_dir: resolve_zsh_integration_dir(),
        }
    }
}

pub fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return normalize_lexically(path);
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    normalize_lexically(&cwd.join(path))
}

/// `path.resolve`-style lexical normalization (no symlink resolution).
pub fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

fn resolve_zsh_integration_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var("PASEO_SHELL_INTEGRATION_DIR") {
        if !dir.trim().is_empty() {
            candidates.push(PathBuf::from(&dir).join("zsh"));
            // Tolerate the shim pointing straight at the zsh directory.
            candidates.push(PathBuf::from(dir));
        }
    }
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        candidates.push(exe_dir.join("shell-integration").join("zsh"));
        candidates.push(exe_dir.join("..").join("shell-integration").join("zsh"));
    }
    candidates
        .into_iter()
        .find(|d| d.join(".zshenv").is_file() && d.join("paseo-integration.zsh").is_file())
}

fn path_key(env: &Env) -> String {
    env.keys()
        .find(|k| k.eq_ignore_ascii_case("path"))
        .cloned()
        .unwrap_or_else(|| "PATH".to_owned())
}

fn prepend_path_entry(current: &str, entry: &str) -> String {
    let mut parts = vec![entry.to_owned()];
    parts.extend(
        current
            .split(PATH_DELIMITER)
            .filter(|v| !v.is_empty() && *v != entry)
            .map(str::to_owned),
    );
    parts.join(&PATH_DELIMITER.to_string())
}

fn unpacked(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    match s.find(".asar") {
        Some(i) => {
            let end = i + ".asar".len();
            let next = s[end..].chars().next();
            if matches!(next, None | Some('/') | Some('\\')) {
                PathBuf::from(format!("{}.asar.unpacked{}", &s[..i], &s[end..]))
            } else {
                path.to_path_buf()
            }
        }
        None => path.to_path_buf(),
    }
}

/// `shell` is the spawned program (command or default shell); only its basename matters.
pub fn build_terminal_environment(
    shell: &str,
    overlay: &Env,
    config: &EnvConfig,
    base: impl Iterator<Item = (String, String)>,
) -> Env {
    let mut env: Env = base.collect();
    env.extend(overlay.iter().map(|(k, v)| (k.clone(), v.clone())));
    env.insert("TERM".into(), "xterm-256color".into());
    env.insert("TERM_PROGRAM".into(), "kitty".into());
    for key in RUNTIME_CONTROL_ENV_KEYS {
        env.remove(*key);
    }

    if let Some(cli) = &config.cli_executable {
        if let Some(dir) = cli.parent() {
            let key = path_key(&env);
            let current = env.get(&key).cloned().unwrap_or_default();
            env.insert(key, prepend_path_entry(&current, &dir.to_string_lossy()));
        }
        env.insert(
            "PASEO_HOOK_CLI".into(),
            absolutize(&unpacked(cli)).to_string_lossy().into_owned(),
        );
    }

    let is_zsh = Path::new(shell).file_name().is_some_and(|n| n == "zsh");
    if is_zsh {
        match prepare_zsh_runtime_dir(config.zsh_integration_dir.as_deref()) {
            Ok(dir) => {
                let original = env.get("ZDOTDIR").cloned().unwrap_or_default();
                env.insert("PASEO_ZSH_ZDOTDIR".into(), original);
                env.insert("ZDOTDIR".into(), dir.to_string_lossy().into_owned());
            }
            Err(e) => tracing::warn!("zsh shell integration disabled: {e}"),
        }
    }
    env
}

pub fn zsh_runtime_dir() -> PathBuf {
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "unknown".into());
    std::env::temp_dir().join(format!("{username}-paseo-zsh-{}", std::process::id()))
}

fn prepare_zsh_runtime_dir(source: Option<&Path>) -> std::io::Result<PathBuf> {
    let source = source.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "shell integration dir not found (set PASEO_SHELL_INTEGRATION_DIR)",
        )
    })?;
    let runtime = zsh_runtime_dir();
    create_private_dir(&runtime)?;
    for name in [".zshenv", "paseo-integration.zsh"] {
        write_private_file_atomic(&runtime.join(name), &std::fs::read(source.join(name))?)?;
    }
    Ok(runtime)
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

fn write_private_file_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(contents)?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// Best-effort cleanup at shutdown.
pub fn cleanup_zsh_runtime_dir() {
    let _ = std::fs::remove_dir_all(zsh_runtime_dir());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> impl Iterator<Item = (String, String)> {
        [
            ("PATH", "/usr/bin:/bin"),
            ("PASEO_NODE_ENV", "development"),
            ("FOO", "base"),
            ("TERM", "dumb"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
    }

    #[test]
    fn overlays_term_and_strips_runtime_keys() {
        let mut overlay = Env::new();
        overlay.insert("FOO".into(), "overlay".into());
        overlay.insert("PASEO_WORKSPACE_ID".into(), "ws".into());
        let env = build_terminal_environment("/bin/bash", &overlay, &EnvConfig::default(), base());
        assert_eq!(env["FOO"], "overlay");
        assert_eq!(env["TERM"], "xterm-256color");
        assert_eq!(env["TERM_PROGRAM"], "kitty");
        assert_eq!(env["PASEO_WORKSPACE_ID"], "ws");
        assert!(!env.contains_key("PASEO_NODE_ENV"));
        assert!(!env.contains_key("PASEO_HOOK_CLI"));
    }

    #[test]
    fn prepends_cli_dir_and_sets_hook_cli() {
        let cfg = EnvConfig {
            cli_executable: Some(PathBuf::from("/opt/paseo/bin/paseo")),
            zsh_integration_dir: None,
        };
        let env = build_terminal_environment("/bin/bash", &Env::new(), &cfg, base());
        assert_eq!(env["PATH"], "/opt/paseo/bin:/usr/bin:/bin");
        assert_eq!(env["PASEO_HOOK_CLI"], "/opt/paseo/bin/paseo");
    }

    #[test]
    fn asar_paths_are_unpacked() {
        assert_eq!(
            unpacked(Path::new("/a/app.asar/bin/paseo")),
            PathBuf::from("/a/app.asar.unpacked/bin/paseo")
        );
        assert_eq!(
            unpacked(Path::new("/a/app.asarx/b")),
            PathBuf::from("/a/app.asarx/b")
        );
    }

    #[test]
    fn path_prepend_dedups() {
        assert_eq!(prepend_path_entry("/x:/a:/x", "/x"), "/x:/a");
    }

    #[test]
    fn normalizes_lexically() {
        assert_eq!(
            normalize_lexically(Path::new("/a/./b/../c")),
            PathBuf::from("/a/c")
        );
    }
}
