//! Ports of the create/duplicate/rename/delete cases in `service.test.ts`.

use paseo_files::explorer::ReadFileParams;
use paseo_files::explorer_entries::*;
use paseo_files::wire::ExplorerEntryKind;
use std::fs;
use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env_remove("GIT_LITERAL_PATHSPECS")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn commit(root: &Path) {
    git(
        root,
        &[
            "-c",
            "user.name=Paseo Test",
            "-c",
            "user.email=test@paseo.local",
            "commit",
            "-m",
            "base",
        ],
    );
}

fn ok(path: &str) -> ExplorerEntryMutationResult {
    ExplorerEntryMutationResult::Ok { path: path.into() }
}

fn err(msg: &str) -> ExplorerEntryMutationResult {
    ExplorerEntryMutationResult::Error { error: msg.into() }
}

fn rename(root: &Path, rel: &str, name: &str) -> ExplorerEntryMutationResult {
    rename_explorer_entry(RenameEntryParams {
        root: root.to_str().unwrap(),
        relative_path: rel,
        name,
    })
}

fn rp<'a>(root: &'a Path, rel: &'a str) -> ReadFileParams<'a> {
    ReadFileParams {
        root: root.to_str().unwrap(),
        relative_path: rel,
    }
}

fn create(
    root: &Path,
    parent: &str,
    name: &str,
    kind: ExplorerEntryKind,
) -> ExplorerEntryMutationResult {
    create_explorer_entry(CreateEntryParams {
        root: root.to_str().unwrap(),
        parent_path: parent,
        name,
        kind,
    })
}

#[test]
fn creates_files_and_directories_refusing_duplicates_and_separator_names() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    assert_eq!(
        create(r, ".", "notes.txt", ExplorerEntryKind::File),
        ok("notes.txt")
    );
    assert!(r.join("notes.txt").is_file());
    assert_eq!(
        create(r, ".", "docs", ExplorerEntryKind::Directory),
        ok("docs")
    );
    assert_eq!(
        create(r, "docs", "guide.md", ExplorerEntryKind::File),
        ok("docs/guide.md")
    );
    assert!(!create(r, ".", "notes.txt", ExplorerEntryKind::File).is_ok());
    assert_eq!(
        create(r, ".", "notes.txt", ExplorerEntryKind::File),
        err("\"notes.txt\" already exists")
    );
    assert_eq!(
        create(r, ".", "../escape", ExplorerEntryKind::Directory),
        err("Name cannot contain path separators")
    );
    assert_eq!(
        create(r, ".", "  ", ExplorerEntryKind::File),
        err("Invalid name")
    );
    assert_eq!(
        create(r, ".", "..", ExplorerEntryKind::File),
        err("Invalid name")
    );
    assert_eq!(
        create(r, "notes.txt", "x", ExplorerEntryKind::File),
        err("Parent path is not a directory")
    );
}

#[test]
fn duplicates_files_and_folders_with_collision_free_sibling_names() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    fs::write(r.join("notes.txt"), "original").unwrap();
    assert_eq!(
        duplicate_explorer_entry(rp(r, "notes.txt")),
        ok("notes copy.txt")
    );
    assert_eq!(
        fs::read_to_string(r.join("notes copy.txt")).unwrap(),
        "original"
    );
    assert_eq!(
        duplicate_explorer_entry(rp(r, "notes.txt")),
        ok("notes copy 2.txt")
    );

    fs::create_dir(r.join("docs")).unwrap();
    fs::write(r.join("docs/guide.md"), "guide").unwrap();
    assert_eq!(duplicate_explorer_entry(rp(r, "docs")), ok("docs copy"));
    assert_eq!(
        fs::read_to_string(r.join("docs copy/guide.md")).unwrap(),
        "guide"
    );

    assert!(!duplicate_explorer_entry(rp(r, ".")).is_ok());
    assert_eq!(
        duplicate_explorer_entry(rp(r, "missing.txt")),
        err("File or folder no longer exists")
    );
}

#[test]
fn duplicate_preserves_timestamps_and_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    fs::write(r.join("a.txt"), "x").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
    fs::File::open(r.join("a.txt"))
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(old))
        .unwrap();
    std::os::unix::fs::symlink("a.txt", r.join("link")).unwrap();
    assert_eq!(duplicate_explorer_entry(rp(r, "a.txt")), ok("a copy.txt"));
    let a = fs::metadata(r.join("a.txt")).unwrap().modified().unwrap();
    let b = fs::metadata(r.join("a copy.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(a, b);
    assert_eq!(duplicate_explorer_entry(rp(r, "link")), ok("link copy"));
    assert_eq!(
        fs::read_link(r.join("link copy")).unwrap(),
        Path::new("a.txt")
    );
}

#[test]
fn renames_tracked_entries_with_git_and_untracked_entries_on_the_filesystem() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    git(r, &["init", "-q"]);
    fs::write(r.join("tracked.txt"), "tracked").unwrap();
    fs::create_dir(r.join("tracked-folder")).unwrap();
    fs::write(r.join("tracked-folder/inside.txt"), "tracked").unwrap();
    git(r, &["add", "."]);
    commit(r);

    assert_eq!(rename(r, "tracked.txt", "renamed.txt"), ok("renamed.txt"));
    assert_eq!(
        git(r, &["status", "--short"]).trim(),
        "R  tracked.txt -> renamed.txt"
    );
    assert_eq!(
        rename(r, "tracked-folder", "renamed-folder"),
        ok("renamed-folder")
    );
    assert!(git(r, &["status", "--short"])
        .contains("tracked-folder/inside.txt -> renamed-folder/inside.txt"));

    fs::write(r.join("untracked.txt"), "untracked").unwrap();
    assert_eq!(rename(r, "untracked.txt", "moved.txt"), ok("moved.txt"));
    assert!(r.join("moved.txt").is_file());
}

#[test]
fn renames_an_untracked_bracketed_filename_without_matching_a_tracked_sibling() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    git(r, &["init", "-q"]);
    fs::write(r.join("i.tsx"), "tracked\n").unwrap();
    git(r, &["add", "i.tsx"]);
    fs::write(r.join("[id].tsx"), "untracked\n").unwrap();
    assert_eq!(rename(r, "[id].tsx", "renamed.tsx"), ok("renamed.tsx"));
    assert_eq!(
        fs::read_to_string(r.join("renamed.tsx")).unwrap(),
        "untracked\n"
    );
    assert!(!r.join("[id].tsx").exists());
    assert_eq!(fs::read_to_string(r.join("i.tsx")).unwrap(), "tracked\n");
    assert_eq!(git(r, &["ls-files", "-z"]), "i.tsx\0");
}

#[test]
fn renames_an_untracked_bracketed_folder_without_matching_a_tracked_sibling() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    git(r, &["init", "-q"]);
    fs::create_dir(r.join("i")).unwrap();
    fs::write(r.join("i/route.tsx"), "tracked\n").unwrap();
    git(r, &["add", "i"]);
    fs::create_dir(r.join("[id]")).unwrap();
    fs::write(r.join("[id]/route.tsx"), "untracked\n").unwrap();
    assert_eq!(rename(r, "[id]", "route"), ok("route"));
    assert_eq!(
        fs::read_to_string(r.join("route/route.tsx")).unwrap(),
        "untracked\n"
    );
    assert!(!r.join("[id]").exists());
    assert_eq!(git(r, &["ls-files", "-z"]), "i/route.tsx\0");
}

#[test]
fn uses_git_to_rename_a_tracked_bracketed_filename_and_preserves_its_tracked_sibling() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    git(r, &["init", "-q"]);
    fs::write(r.join("[id].tsx"), "tracked route\n").unwrap();
    fs::write(r.join("i.tsx"), "tracked sibling\n").unwrap();
    git(r, &["add", "--", "[id].tsx", "i.tsx"]);
    assert_eq!(rename(r, "[id].tsx", "renamed.tsx"), ok("renamed.tsx"));
    assert_eq!(
        fs::read_to_string(r.join("renamed.tsx")).unwrap(),
        "tracked route\n"
    );
    assert_eq!(git(r, &["ls-files", "-z"]), "i.tsx\0renamed.tsx\0");
}

#[test]
fn supports_case_only_renames_for_tracked_and_untracked_files() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    git(r, &["init", "-q"]);
    fs::write(r.join("Tracked.txt"), "tracked").unwrap();
    fs::write(r.join("Loose.txt"), "untracked").unwrap();
    git(r, &["add", "Tracked.txt"]);
    commit(r);
    assert_eq!(rename(r, "Tracked.txt", "tracked.txt"), ok("tracked.txt"));
    assert_eq!(rename(r, "Loose.txt", "loose.txt"), ok("loose.txt"));
    let names: Vec<String> = fs::read_dir(r)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"tracked.txt".to_string()) && names.contains(&"loose.txt".to_string()));
}

#[test]
fn refuses_invalid_renames_existing_targets_and_the_workspace_root() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    fs::write(r.join("source.txt"), "source").unwrap();
    fs::write(r.join("existing.txt"), "existing").unwrap();
    fs::hard_link(r.join("source.txt"), r.join("source-link.txt")).unwrap();
    assert_eq!(
        rename(r, "source.txt", "existing.txt"),
        err("\"existing.txt\" already exists")
    );
    assert_eq!(
        rename(r, "source.txt", "source-link.txt"),
        err("\"source-link.txt\" already exists")
    );
    assert_eq!(
        rename(r, "source.txt", "../outside.txt"),
        err("Name cannot contain path separators")
    );
    assert_eq!(
        rename(r, ".", "renamed-root"),
        err("Cannot rename the workspace root")
    );
    assert_eq!(
        rename(r, "gone.txt", "x"),
        err("File or folder no longer exists")
    );
}

#[test]
fn deletes_files_and_directories_but_never_the_workspace_root_or_outside_paths() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    fs::write(r.join("doomed.txt"), "bye").unwrap();
    assert_eq!(delete_explorer_entry(rp(r, "doomed.txt")), ok("doomed.txt"));
    assert!(!r.join("doomed.txt").exists());

    create(r, ".", "nested", ExplorerEntryKind::Directory);
    fs::write(r.join("nested/inner.txt"), "hi").unwrap();
    assert_eq!(delete_explorer_entry(rp(r, "nested")), ok("nested"));
    assert!(!r.join("nested").exists());

    assert_eq!(
        delete_explorer_entry(rp(r, ".")),
        err("Cannot delete the workspace root")
    );
    assert!(!delete_explorer_entry(rp(r, "../outside")).is_ok());
}

#[test]
fn delete_removes_a_symlink_not_its_target() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    fs::create_dir(r.join("real")).unwrap();
    fs::write(r.join("real/keep.txt"), "x").unwrap();
    std::os::unix::fs::symlink("real", r.join("alias")).unwrap();
    assert_eq!(delete_explorer_entry(rp(r, "alias")), ok("alias"));
    assert!(r.join("real/keep.txt").exists());
    assert!(fs::symlink_metadata(r.join("alias")).is_err());
}

#[test]
fn refuses_mutations_on_dangling_links_behind_an_escaping_symlink() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let r = root.path();
    let o = outside.path();
    std::os::unix::fs::symlink(o, r.join("link")).unwrap();
    std::os::unix::fs::symlink("/nonexistent-paseo-target", o.join("dangling")).unwrap();

    assert!(!rename(r, "link/dangling", "moved").is_ok());
    assert!(fs::symlink_metadata(o.join("dangling")).is_ok());
    assert!(!delete_explorer_entry(rp(r, "link/dangling")).is_ok());
    assert!(fs::symlink_metadata(o.join("dangling")).is_ok());
    assert!(!duplicate_explorer_entry(rp(r, "link/dangling")).is_ok());
    assert!(fs::symlink_metadata(o.join("dangling copy")).is_err());
}
