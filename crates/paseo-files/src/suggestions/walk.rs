//! Fair tree walk: branches are visited round-robin so a deep subtree cannot
//! starve its siblings of the scan budget. This reproduces the TS async
//! generators lazily, because the order decides which entries a capped scan sees.

use super::fs_read::read_children;
use super::{ChildEntry, Kind, SearchInput, Traversed};
use crate::path_utils::is_path_inside_root;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub(super) struct RoundRobin {
    active: Vec<Option<Branch>>,
    index: usize,
    next_round: Vec<Branch>,
}

impl RoundRobin {
    pub fn new(branches: Vec<Branch>) -> Self {
        Self {
            active: branches.into_iter().map(Some).collect(),
            index: 0,
            next_round: Vec::new(),
        }
    }

    pub fn next(
        &mut self,
        input: &SearchInput,
        visited: &mut HashSet<PathBuf>,
    ) -> Option<Traversed> {
        loop {
            if self.index >= self.active.len() {
                if self.next_round.is_empty() {
                    return None;
                }
                self.active = std::mem::take(&mut self.next_round)
                    .into_iter()
                    .map(Some)
                    .collect();
                self.index = 0;
            }
            let mut branch = self.active[self.index].take()?;
            self.index += 1;
            if let Some(value) = branch.next(input, visited) {
                self.next_round.push(branch);
                return Some(value);
            }
        }
    }
}

enum BranchState {
    Fresh(Traversed),
    Expand(Traversed),
    Children(RoundRobin),
    Done,
}

pub(super) struct Branch {
    state: BranchState,
}

impl Branch {
    pub fn new(entry: Traversed) -> Self {
        Self {
            state: BranchState::Fresh(entry),
        }
    }

    fn next(&mut self, input: &SearchInput, visited: &mut HashSet<PathBuf>) -> Option<Traversed> {
        loop {
            match std::mem::replace(&mut self.state, BranchState::Done) {
                BranchState::Fresh(entry) => {
                    self.state = BranchState::Expand(entry.clone());
                    return Some(entry);
                }
                BranchState::Expand(entry) => {
                    if entry.kind != Kind::Directory
                        || visited.contains(&entry.resolved)
                        || entry.depth >= input.max_depth
                    {
                        return None;
                    }
                    visited.insert(entry.resolved.clone());
                    let branches =
                        child_branches(&entry.resolved, &entry.visible, entry.depth + 1, input);
                    self.state = BranchState::Children(RoundRobin::new(branches));
                }
                BranchState::Children(mut rr) => {
                    let value = rr.next(input, visited);
                    if value.is_some() {
                        self.state = BranchState::Children(rr);
                    }
                    return value;
                }
                BranchState::Done => return None,
            }
        }
    }
}

pub(super) fn child_branches(
    resolved: &Path,
    visible: &Path,
    depth: usize,
    input: &SearchInput,
) -> Vec<Branch> {
    read_children(resolved)
        .into_iter()
        .filter(|child| stays_inside_root(child, &input.root))
        .filter(|child| should_discover(child, input))
        .map(|child| {
            Branch::new(Traversed {
                visible: visible.join(&child.name),
                depth,
                name: child.name,
                resolved: child.resolved,
                kind: child.kind,
            })
        })
        .collect()
}

// Only a symlink can resolve outside the tree being walked.
pub(super) fn stays_inside_root(entry: &ChildEntry, root: &Path) -> bool {
    !entry.via_symlink || is_path_inside_root(root, &entry.resolved)
}

const IGNORED_DIRECTORY_NAMES: [&str; 12] = [
    "node_modules",
    "venv",
    "env",
    "virtualenv",
    "dist",
    "build",
    "target",
    "out",
    "coverage",
    "vendor",
    "__pycache__",
    ".git",
];

pub(super) fn should_discover(entry: &ChildEntry, input: &SearchInput) -> bool {
    if is_newly_git_ignored(entry, input) || is_newly_excluded(entry, input) {
        return false;
    }
    if entry.kind == Kind::File {
        return input.include_files && !entry.name.starts_with('.');
    }
    if IGNORED_DIRECTORY_NAMES.contains(&entry.name.as_str()) {
        return false;
    }
    if !entry.name.starts_with('.') {
        return true;
    }
    input.hidden_directory_names.contains(&entry.name)
}

// Every ancestor of a named child already passed discovery, so only the entry
// itself can be newly ignored. A symlink lands on a path with other ancestors.
fn is_newly_git_ignored(entry: &ChildEntry, input: &SearchInput) -> bool {
    if entry.via_symlink {
        return is_git_ignored_path(&entry.resolved, input);
    }
    input.git_ignored_paths.contains(&entry.resolved)
}

fn is_newly_excluded(entry: &ChildEntry, input: &SearchInput) -> bool {
    if input.excluded_discovery_paths.is_empty() {
        return false;
    }
    if entry.via_symlink {
        return is_inside_excluded_path(&entry.resolved, input);
    }
    input.excluded_discovery_paths.contains(&entry.resolved)
}

pub(super) fn is_inside_excluded_path(path: &Path, input: &SearchInput) -> bool {
    input
        .excluded_discovery_paths
        .iter()
        .any(|excluded| is_path_inside_root(excluded, path))
}

pub(super) fn is_git_ignored_path(path: &Path, input: &SearchInput) -> bool {
    let mut candidate = path.to_path_buf();
    while candidate != input.root && is_path_inside_root(&input.root, &candidate) {
        if input.git_ignored_paths.contains(&candidate) {
            return true;
        }
        match candidate.parent() {
            Some(parent) => candidate = parent.to_path_buf(),
            None => break,
        }
    }
    false
}
