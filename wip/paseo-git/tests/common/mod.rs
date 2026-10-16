#![allow(dead_code)]
//! Shared fixtures: temp git repos mirroring the TS test helpers.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Once;

use paseo_git::repo::CheckoutContext;
use paseo_git::worktree_meta::{
    compute_worktree_path, write_paseo_worktree_metadata, WorktreeRootOptions,
};
use tempfile::TempDir;

static INIT: Once = Once::new();

/// Isolate git from the machine's config. Must run before any command.
pub fn init() {
    INIT.call_once(|| {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_TERMINAL_PROMPT", "0");
        std::env::remove_var("GIT_EXTERNAL_DIFF");
    });
}

pub struct Fixture {
    _temp: TempDir,
    pub temp_dir: String,
    pub repo: String,
    pub paseo_home: String,
}

impl Fixture {
    pub fn ctx(&self) -> CheckoutContext {
        CheckoutContext {
            paseo_home: Some(self.paseo_home.clone()),
            ..Default::default()
        }
    }

    pub fn path(&self, name: &str) -> String {
        format!("{}/{}", self.temp_dir, name)
    }

    pub fn root_options(&self) -> WorktreeRootOptions {
        WorktreeRootOptions {
            paseo_home: Some(self.paseo_home.clone()),
            worktrees_root: None,
        }
    }
}

pub fn temp_dir() -> (TempDir, String) {
    init();
    let temp = tempfile::Builder::new()
        .prefix("paseo-git-test-")
        .tempdir()
        .unwrap();
    let path = fs::canonicalize(temp.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    (temp, path)
}

pub fn git(cwd: &str, args: &[&str]) -> String {
    init();
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed in {cwd}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn git_env(cwd: &str, args: &[&str], env: &[(&str, &str)]) -> String {
    init();
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn git_succeeds(cwd: &str, args: &[&str]) -> bool {
    init();
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn git_trim(cwd: &str, args: &[&str]) -> String {
    git(cwd, args).trim().to_string()
}

pub fn write(path: impl AsRef<Path>, content: impl AsRef<[u8]>) {
    if let Some(parent) = path.as_ref().parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

pub fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).unwrap().replace("\r\n", "\n")
}

pub fn exists(path: impl AsRef<Path>) -> bool {
    path.as_ref().exists()
}

pub fn commit(cwd: &str, message: &str) {
    git(
        cwd,
        &["-c", "commit.gpgsign=false", "commit", "-m", message],
    );
}

pub fn commit_file(cwd: &str, path: &str, content: &str, message: &str) {
    write(format!("{cwd}/{path}"), content);
    git(cwd, &["add", path]);
    commit(cwd, message);
}

pub fn set_identity(cwd: &str) {
    git(cwd, &["config", "user.email", "test@test.com"]);
    git(cwd, &["config", "user.name", "Test"]);
}

/// Mirrors the TS `initRepo`: a repo on `main` with `file.txt` ("hello") committed.
pub fn init_repo() -> Fixture {
    let (temp, temp_dir) = temp_dir();
    let repo = format!("{temp_dir}/repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    set_identity(&repo);
    write(format!("{repo}/file.txt"), "hello\n");
    git(&repo, &["add", "."]);
    commit(&repo, "initial");
    let paseo_home = format!("{temp_dir}/paseo-home");
    Fixture {
        _temp: temp,
        temp_dir,
        repo,
        paseo_home,
    }
}

/// A repo on `main` with identity configured and no commits.
pub fn init_empty_repo(user_name: &str) -> Fixture {
    let (temp, temp_dir) = temp_dir();
    let repo = format!("{temp_dir}/repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@test.com"]);
    git(&repo, &["config", "user.name", user_name]);
    let paseo_home = format!("{temp_dir}/paseo-home");
    Fixture {
        _temp: temp,
        temp_dir,
        repo,
        paseo_home,
    }
}

pub fn add_bare_remote(f: &Fixture) -> String {
    let remote = f.path("remote.git");
    git(&f.temp_dir, &["init", "--bare", "-b", "main", &remote]);
    git(&f.repo, &["remote", "add", "origin", &remote]);
    remote
}

/// Returns (remote_dir, clone_dir).
pub fn setup_remote_tracking_main(f: &Fixture) -> (String, String) {
    let remote = add_bare_remote(f);
    git(&f.repo, &["push", "-u", "origin", "main"]);
    let clone = f.path("upstream-clone");
    git(&f.temp_dir, &["clone", &remote, &clone]);
    set_identity(&clone);
    (remote, clone)
}

pub fn clone_remote(f: &Fixture, remote: &str, name: &str) -> String {
    let clone = f.path(name);
    git(&f.temp_dir, &["clone", remote, &clone]);
    set_identity(&clone);
    clone
}

/// Equivalent of the TS legacy worktree helper: a Paseo-owned worktree with metadata.
pub async fn create_paseo_worktree(f: &Fixture, slug: &str, branch: &str, base: &str) -> String {
    let path = compute_worktree_path(&f.repo, slug, &f.root_options()).await;
    fs::create_dir_all(Path::new(&path).parent().unwrap()).unwrap();
    git(
        &f.repo,
        &["worktree", "add", "-b", branch, "--no-track", &path, base],
    );
    write_paseo_worktree_metadata(&path, base, Some(&format!("refs/heads/{base}"))).unwrap();
    fs::canonicalize(&path)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

pub fn real(path: &str) -> String {
    fs::canonicalize(path)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}
