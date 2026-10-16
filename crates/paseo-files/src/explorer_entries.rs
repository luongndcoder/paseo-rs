//! Create, duplicate, rename and delete for explorer entries.

use crate::error::{io_is_missing, ExplorerError, Result};
use crate::explorer::{path_str, resolve_scoped_path, ReadFileParams, ScopedPath};
use crate::git::{run_git, GitOptions};
use crate::path_utils::{basename, expand_user_path, extname, normalize_relative_path};
use crate::wire::ExplorerEntryKind;
use std::fs::{self, FileTimes, Metadata, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplorerEntryMutationResult {
    Ok { path: String },
    Error { error: String },
}

impl ExplorerEntryMutationResult {
    fn ok(path: String) -> Self {
        Self::Ok { path }
    }
    fn error(error: impl Into<String>) -> Self {
        Self::Error {
            error: error.into(),
        }
    }
    fn from_err(e: ExplorerError) -> Self {
        Self::Error {
            error: e.to_string(),
        }
    }
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }
}

#[derive(Debug, Clone)]
pub struct CreateEntryParams<'a> {
    pub root: &'a str,
    pub parent_path: &'a str,
    pub name: &'a str,
    pub kind: ExplorerEntryKind,
}

#[derive(Debug, Clone)]
pub struct RenameEntryParams<'a> {
    pub root: &'a str,
    pub relative_path: &'a str,
    pub name: &'a str,
}

fn validate_name(name: &str) -> std::result::Result<&str, ExplorerEntryMutationResult> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return Err(ExplorerEntryMutationResult::error("Invalid name"));
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err(ExplorerEntryMutationResult::error(
            "Name cannot contain path separators",
        ));
    }
    Ok(trimmed)
}

pub fn create_explorer_entry(params: CreateEntryParams) -> ExplorerEntryMutationResult {
    let name = match validate_name(params.name) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let attempt = (|| -> Result<ExplorerEntryMutationResult> {
        let parent = resolve_scoped_path(params.root, params.parent_path)?;
        if !fs::metadata(&parent.resolved)?.is_dir() {
            return Ok(ExplorerEntryMutationResult::error(
                "Parent path is not a directory",
            ));
        }
        let target = parent.resolved.join(name);
        match params.kind {
            ExplorerEntryKind::Directory => fs::create_dir(&target)?,
            ExplorerEntryKind::File => {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o644)
                    .open(&target)?;
            }
        }
        let workspace = expand_user_path(params.root);
        Ok(ExplorerEntryMutationResult::ok(normalize_relative_path(
            &workspace,
            &parent.requested.join(name),
        )))
    })();
    match attempt {
        Ok(r) => r,
        Err(e) if e.is_exists() => {
            ExplorerEntryMutationResult::error(format!("\"{name}\" already exists"))
        }
        Err(e) => ExplorerEntryMutationResult::from_err(e),
    }
}

fn is_workspace_root(source: &ScopedPath, root: &str) -> Result<bool> {
    let real_root = fs::canonicalize(expand_user_path(root))?;
    Ok(source.resolved == real_root)
}

fn no_longer_exists(e: &ExplorerError) -> bool {
    e.is_missing()
}

pub fn duplicate_explorer_entry(params: ReadFileParams) -> ExplorerEntryMutationResult {
    let attempt = (|| -> Result<ExplorerEntryMutationResult> {
        let source = resolve_scoped_path(params.root, params.relative_path)?;
        if is_workspace_root(&source, params.root)? {
            return Ok(ExplorerEntryMutationResult::error(
                "Cannot duplicate the workspace root",
            ));
        }
        let stats = fs::symlink_metadata(&source.requested)?;
        let source_name = basename(&path_str(&source.requested)).to_string();
        let extension = if stats.is_dir() {
            String::new()
        } else {
            extname(&source_name)
        };
        let base_name = if extension.is_empty() {
            source_name.clone()
        } else {
            source_name[..source_name.len() - extension.len()].to_string()
        };
        let parent = source.requested.parent().unwrap_or(Path::new("/"));
        let mut copy_number = 1u32;
        let target = loop {
            let suffix = if copy_number == 1 {
                " copy".to_string()
            } else {
                format!(" copy {copy_number}")
            };
            let candidate = parent.join(format!("{base_name}{suffix}{extension}"));
            match fs::symlink_metadata(&candidate) {
                Ok(_) => copy_number += 1,
                Err(e) if io_is_missing(&e) => break candidate,
                Err(e) => return Err(e.into()),
            }
        };
        copy_entry(&source.requested, &target, &stats)?;
        let workspace = expand_user_path(params.root);
        Ok(ExplorerEntryMutationResult::ok(normalize_relative_path(
            &workspace, &target,
        )))
    })();
    match attempt {
        Ok(r) => r,
        Err(e) if no_longer_exists(&e) => {
            ExplorerEntryMutationResult::error("File or folder no longer exists")
        }
        Err(e) => ExplorerEntryMutationResult::from_err(e),
    }
}

/// `fs.cp` with `preserveTimestamps` and `verbatimSymlinks`.
fn copy_entry(source: &Path, target: &Path, stats: &Metadata) -> std::io::Result<()> {
    if stats.file_type().is_symlink() {
        return std::os::unix::fs::symlink(fs::read_link(source)?, target);
    }
    if stats.is_dir() {
        fs::create_dir(target)?;
        for child in fs::read_dir(source)? {
            let child = child?;
            let child_stats = fs::symlink_metadata(child.path())?;
            copy_entry(&child.path(), &target.join(child.file_name()), &child_stats)?;
        }
        fs::set_permissions(target, stats.permissions())?;
    } else {
        fs::copy(source, target)?;
    }
    if !stats.file_type().is_symlink() {
        let times = FileTimes::new()
            .set_accessed(stats.accessed()?)
            .set_modified(stats.modified()?);
        fs::File::open(target)?.set_times(times)?;
    }
    Ok(())
}

pub fn rename_explorer_entry(params: RenameEntryParams) -> ExplorerEntryMutationResult {
    let name = match validate_name(params.name) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let attempt = (|| -> Result<ExplorerEntryMutationResult> {
        let source = resolve_scoped_path(params.root, params.relative_path)?;
        let real_root = fs::canonicalize(expand_user_path(params.root))?;
        if source.resolved == real_root {
            return Ok(ExplorerEntryMutationResult::error(
                "Cannot rename the workspace root",
            ));
        }
        let parent = source.requested.parent().unwrap_or(Path::new("/"));
        let target = parent.join(name);
        let source_stats = fs::symlink_metadata(&source.requested)?;
        match fs::symlink_metadata(&target) {
            Ok(target_stats) => {
                if !is_case_only_rename(&source, &target, &source_stats, &target_stats) {
                    return Ok(ExplorerEntryMutationResult::error(format!(
                        "\"{name}\" already exists"
                    )));
                }
            }
            Err(e) if io_is_missing(&e) => {}
            Err(e) => return Err(e.into()),
        }
        let workspace = expand_user_path(params.root);
        let source_path = normalize_relative_path(&workspace, &source.requested);
        let renamed_path = normalize_relative_path(&workspace, &target);
        if source_path == renamed_path {
            return Ok(ExplorerEntryMutationResult::ok(renamed_path));
        }
        if is_tracked(&real_root, &source_path)? {
            run_git(
                &["mv", "--", &source_path, &renamed_path],
                &real_root,
                &GitOptions::default(),
            )
            .map_err(ExplorerError::message)?;
        } else {
            fs::rename(&source.requested, &target)?;
        }
        Ok(ExplorerEntryMutationResult::ok(renamed_path))
    })();
    match attempt {
        Ok(r) => r,
        Err(e) if e.is_exists() => {
            ExplorerEntryMutationResult::error(format!("\"{name}\" already exists"))
        }
        Err(e) if no_longer_exists(&e) => {
            ExplorerEntryMutationResult::error("File or folder no longer exists")
        }
        Err(e) => ExplorerEntryMutationResult::from_err(e),
    }
}

fn is_tracked(real_root: &Path, source_path: &str) -> Result<bool> {
    let repo = run_git(
        &["rev-parse", "--is-inside-work-tree"],
        real_root,
        &GitOptions {
            accept_exit_codes: &[0, 128],
            ..Default::default()
        },
    )
    .map_err(ExplorerError::message)?;
    if repo.exit_code != 0 {
        return Ok(false);
    }
    let literal = format!(":(literal){source_path}");
    // Inherited GIT_LITERAL_PATHSPECS disables the :(literal) magic.
    let tracked = run_git(
        &["ls-files", "-z", "--", &literal],
        real_root,
        &GitOptions {
            env_remove: &["GIT_LITERAL_PATHSPECS"],
            ..Default::default()
        },
    )
    .map_err(ExplorerError::message)?;
    Ok(!tracked.stdout.is_empty())
}

fn is_case_only_rename(
    source: &ScopedPath,
    target: &Path,
    source_stats: &Metadata,
    target_stats: &Metadata,
) -> bool {
    let same_entry =
        source_stats.dev() == target_stats.dev() && source_stats.ino() == target_stats.ino();
    same_entry && path_str(&source.requested).to_lowercase() == path_str(target).to_lowercase()
}

pub fn delete_explorer_entry(params: ReadFileParams) -> ExplorerEntryMutationResult {
    let attempt = (|| -> Result<ExplorerEntryMutationResult> {
        let scoped = resolve_scoped_path(params.root, params.relative_path)?;
        if is_workspace_root(&scoped, params.root)? {
            return Ok(ExplorerEntryMutationResult::error(
                "Cannot delete the workspace root",
            ));
        }
        // Remove the requested path, not the realpath: a symlink is removed, never its target.
        let stats = fs::symlink_metadata(&scoped.requested)?;
        if stats.is_dir() {
            fs::remove_dir_all(&scoped.requested)?;
        } else {
            fs::remove_file(&scoped.requested)?;
        }
        let workspace = expand_user_path(params.root);
        Ok(ExplorerEntryMutationResult::ok(normalize_relative_path(
            &workspace,
            &scoped.requested,
        )))
    })();
    match attempt {
        Ok(r) => r,
        Err(e) if no_longer_exists(&e) => {
            ExplorerEntryMutationResult::error("File or folder no longer exists")
        }
        Err(e) => ExplorerEntryMutationResult::from_err(e),
    }
}
