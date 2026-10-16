//! Own test binary: mutates process env, which would race other tests.

use paseo_files::explorer_entries::*;
use std::fs;
use std::process::Command;

fn git(root: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env_remove("GIT_LITERAL_PATHSPECS")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn keeps_tracked_renames_in_the_index_with_inherited_literal_pathspecs() {
    for (source_path, source_file, target_name, target_file) in [
        ("[id].tsx", "[id].tsx", "renamed.tsx", "renamed.tsx"),
        ("[id]", "[id]/route.tsx", "route", "route/route.tsx"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        std::env::set_var("GIT_LITERAL_PATHSPECS", "1");
        git(r, &["init", "-q"]);
        fs::create_dir_all(r.join(source_file).parent().unwrap()).unwrap();
        fs::write(r.join(source_file), "tracked route\n").unwrap();
        fs::write(r.join("i.tsx"), "tracked sibling\n").unwrap();
        git(r, &["add", "."]);
        let result = rename_explorer_entry(RenameEntryParams {
            root: r.to_str().unwrap(),
            relative_path: source_path,
            name: target_name,
        });
        assert_eq!(
            result,
            ExplorerEntryMutationResult::Ok {
                path: target_name.into()
            }
        );
        assert_eq!(
            fs::read_to_string(r.join(target_file)).unwrap(),
            "tracked route\n"
        );
        assert!(!r.join(source_path).exists());
        assert_eq!(
            fs::read_to_string(r.join("i.tsx")).unwrap(),
            "tracked sibling\n"
        );
        assert_eq!(
            git(r, &["ls-files", "-z"]),
            format!("i.tsx\0{target_file}\0")
        );
        assert_eq!(std::env::var("GIT_LITERAL_PATHSPECS").unwrap(), "1");
        std::env::remove_var("GIT_LITERAL_PATHSPECS");
    }
}
