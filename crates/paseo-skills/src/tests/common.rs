use crate::controller::SkillsController;
use crate::error::{Result, SkillsError};
use crate::operations::SkillTargets;
use crate::selection::{ConfigStore, SelectionStore, SkillSelection};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

pub struct Sandbox {
    pub root: TempDir,
    pub targets: SkillTargets,
}

pub fn sandbox() -> Sandbox {
    let root = tempfile::Builder::new()
        .prefix("paseo-skills-")
        .tempdir()
        .unwrap();
    // Canonical so realpath comparisons (symlink tests) match lexical paths on macOS.
    let base = fs::canonicalize(root.path()).unwrap();
    let targets = SkillTargets {
        source_dir: base.join("bundle"),
        agents_dir: base.join("home/.agents/skills"),
        claude_dir: base.join("home/.claude/skills"),
        codex_dir: base.join("home/.codex/skills"),
    };
    fs::create_dir_all(&targets.source_dir).unwrap();
    Sandbox { root, targets }
}

pub fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (rel, content) in files {
        let full = root.join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }
}

pub fn write_bundle_skill(source: &Path, name: &str, files: &[(&str, &str)]) {
    write_files(&source.join(name), files);
}

pub fn write_on_disk_skill(dir: &Path, name: &str, files: &[(&str, &str)]) {
    write_files(&dir.join(name), files);
}

pub fn write_to_all(targets: &SkillTargets, name: &str, files: &[(&str, &str)]) {
    for dir in targets.roots() {
        write_on_disk_skill(dir, name, files);
    }
}

pub fn write_current_bundle(source: &Path) {
    write_bundle_skill(source, "paseo", &[("SKILL.md", "paseo-v1")]);
    write_bundle_skill(source, "paseo-loop", &[("SKILL.md", "loop-v1")]);
}

pub fn installed_in(targets: &SkillTargets, name: &str) -> [bool; 3] {
    let roots = targets.roots();
    [0, 1, 2].map(|i| fs::symlink_metadata(roots[i].join(name)).is_ok())
}

pub fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).unwrap()
}

pub fn exists(path: impl AsRef<Path>) -> bool {
    fs::symlink_metadata(path).is_ok()
}

pub fn strs(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

pub fn only(skills: &[&str]) -> SkillSelection {
    SkillSelection::Custom {
        skills: strs(skills),
    }
}

// ---- controller harness -------------------------------------------------------------

pub const BUNDLED_SKILLS: [&str; 3] = ["paseo", "paseo-advisor", "paseo-loop"];

/// A fake `ConfigStore` adapter: the selection store is an injected port, so a store that
/// refuses or pauses persistence is a fake adapter rather than a mock of the code under
/// test.
pub struct TestStore {
    current: Mutex<Option<SkillSelection>>,
    gate: Option<Gate>,
    fail_when_ungated: bool,
}

struct Gate {
    started: Sender<()>,
    release: Mutex<Receiver<bool>>,
}

pub struct GateControl {
    pub started: Receiver<()>,
    release: Sender<bool>,
}

impl GateControl {
    /// Lets the paused `set` return (`succeed`) or fail with the read-only error.
    pub fn release(&self, succeed: bool) {
        let _ = self.release.send(succeed);
    }
}

fn read_only() -> SkillsError {
    SkillsError::Store("selection store is read-only".into())
}

impl TestStore {
    pub fn plain(initial: Option<SkillSelection>) -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(initial),
            gate: None,
            fail_when_ungated: false,
        })
    }

    pub fn unwritable(initial: SkillSelection) -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(Some(initial)),
            gate: None,
            fail_when_ungated: true,
        })
    }

    /// `set` announces itself on `started`, then waits for `release`.
    pub fn gated(initial: SkillSelection) -> (Arc<Self>, GateControl) {
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let store = Arc::new(Self {
            current: Mutex::new(Some(initial)),
            gate: Some(Gate {
                started: started_tx,
                release: Mutex::new(release_rx),
            }),
            fail_when_ungated: false,
        });
        (
            store,
            GateControl {
                started: started_rx,
                release: release_tx,
            },
        )
    }

    pub fn current(&self) -> Option<SkillSelection> {
        self.current.lock().unwrap().clone()
    }
}

impl ConfigStore for TestStore {
    fn selection(&self) -> Result<Option<Value>> {
        Ok(self
            .current
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| serde_json::to_value(s).unwrap()))
    }

    fn set_selection(&self, selection: &SkillSelection) -> Result<()> {
        if let Some(gate) = &self.gate {
            let _ = gate.started.send(());
            let succeed = gate.release.lock().unwrap().recv().unwrap_or(false);
            if !succeed {
                return Err(read_only());
            }
        } else if self.fail_when_ungated {
            return Err(read_only());
        }
        *self.current.lock().unwrap() = Some(selection.clone());
        Ok(())
    }
}

pub struct Harness {
    pub root: PathBuf,
    pub targets: SkillTargets,
    pub controller: Arc<SkillsController>,
    pub store: Arc<TestStore>,
    _dir: TempDir,
}

pub fn harness() -> Harness {
    harness_with(TestStore::plain(Some(SkillSelection::All)))
}

pub fn harness_with(store: Arc<TestStore>) -> Harness {
    let sb = sandbox();
    for name in BUNDLED_SKILLS {
        write_bundle_skill(
            &sb.targets.source_dir,
            name,
            &[("SKILL.md", &format!("{name}-v1"))],
        );
    }
    let root = fs::canonicalize(sb.root.path()).unwrap();
    let targets = sb.targets.clone();
    let resolved = targets.clone();
    let controller = Arc::new(SkillsController::new(
        Arc::new(move || resolved.clone()),
        SelectionStore::new(store.clone()),
    ));
    Harness {
        root,
        targets,
        controller,
        store,
        _dir: sb.root,
    }
}

impl Harness {
    pub fn user_file(&self, skill: &str, rel: &str, contents: &str) {
        for dir in self.targets.roots() {
            let file = dir.join(skill).join(rel);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, contents).unwrap();
        }
    }

    pub fn read_user_file(&self, skill: &str, rel: &str) -> Vec<Option<String>> {
        self.targets
            .roots()
            .iter()
            .map(|dir| fs::read_to_string(dir.join(skill).join(rel)).ok())
            .collect()
    }

    pub fn installed_everywhere(&self) -> Vec<Vec<String>> {
        self.targets
            .roots()
            .iter()
            .map(|dir| installed_skills(dir))
            .collect()
    }

    pub fn is_installed(&self, name: &str) -> bool {
        [&self.targets.agents_dir, &self.targets.claude_dir]
            .iter()
            .all(|dir| dir.join(name).exists())
    }

    /// Anything the transaction staged and failed to clean up sits beside the skills tree.
    pub fn backup_artifacts(&self) -> Vec<Vec<String>> {
        self.targets
            .roots()
            .iter()
            .map(|dir| {
                let parent = dir.parent().unwrap();
                let base = dir.file_name().unwrap().to_string_lossy().into_owned();
                let mut out: Vec<String> = names(parent)
                    .into_iter()
                    .filter(|e| *e != base && !e.starts_with(".paseo-skills-recovered-"))
                    .collect();
                out.extend(
                    names(dir)
                        .into_iter()
                        .filter(|e| e.starts_with(".paseo-skills-transaction-")),
                );
                out.sort();
                out
            })
            .collect()
    }

    pub fn block_agents_dir(&self) {
        let _ = fs::remove_dir_all(&self.targets.agents_dir);
        fs::create_dir_all(self.targets.agents_dir.parent().unwrap()).unwrap();
        fs::write(&self.targets.agents_dir, "not a directory").unwrap();
    }
}

pub fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

pub fn installed_skills(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

pub fn first_with_prefix(dir: &Path, prefix: &str) -> Option<String> {
    names(dir).into_iter().find(|e| e.starts_with(prefix))
}
