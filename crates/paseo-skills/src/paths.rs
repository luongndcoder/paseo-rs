//! Where skills come from and where they go. Ports `paths.ts`.
//!
//! The bundled catalog ships next to the binary in a resources dir that mirrors the TS dist
//! layout (spec Q6): `<exe dir>/skills`, or `<exe dir>/dist/server/skills`.

use crate::operations::SkillTargets;
use std::path::{Path, PathBuf};

/// First existing candidate under `resources_dir`, else the first candidate.
pub fn resolve_bundled_skills_dir_in(resources_dir: &Path) -> PathBuf {
    let candidates = [
        resources_dir.join("skills"),
        resources_dir.join("dist").join("server").join("skills"),
    ];
    candidates
        .iter()
        .find(|c| c.exists())
        .cloned()
        .unwrap_or_else(|| candidates[0].clone())
}

/// Resolves the catalog next to the running executable.
pub fn resolve_bundled_skills_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    resolve_bundled_skills_dir_in(&exe_dir)
}

/// The fixed managed directories under the daemon user's home.
pub fn resolve_skill_targets(home: &Path, source_dir: PathBuf) -> SkillTargets {
    SkillTargets {
        source_dir,
        agents_dir: home.join(".agents").join("skills"),
        claude_dir: home.join(".claude").join("skills"),
        codex_dir: home.join(".codex").join("skills"),
    }
}

/// `os.homedir()` for the daemon user: `$HOME`, then `$USERPROFILE`.
pub fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_catalog_beside_binary() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("skills");
        std::fs::create_dir_all(&catalog).unwrap();
        assert_eq!(resolve_bundled_skills_dir_in(dir.path()), catalog);
    }

    #[test]
    fn finds_catalog_in_dist_layout() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("dist").join("server").join("skills");
        std::fs::create_dir_all(&catalog).unwrap();
        assert_eq!(resolve_bundled_skills_dir_in(dir.path()), catalog);
    }

    #[test]
    fn falls_back_to_first_candidate_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_bundled_skills_dir_in(dir.path()),
            dir.path().join("skills")
        );
    }

    #[test]
    fn keeps_the_original_fixed_managed_directories_under_home() {
        let home = Path::new("/home/someone");
        let t = resolve_skill_targets(home, PathBuf::from("/bundle"));
        assert_eq!(t.agents_dir, home.join(".agents/skills"));
        assert_eq!(t.claude_dir, home.join(".claude/skills"));
        assert_eq!(t.codex_dir, home.join(".codex/skills"));
        assert_eq!(t.source_dir, PathBuf::from("/bundle"));
    }
}
