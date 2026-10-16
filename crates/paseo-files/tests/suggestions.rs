//! Port of `utils/directory-suggestions.test.ts` (behavioral cases; the Node
//! threadpool and async-hook cases have no Rust equivalent).

use paseo_files::suggestions::*;
use paseo_files::wire::{DirectorySuggestionEntry, MatchMode, SuggestionKind};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

fn canon(p: impl AsRef<Path>) -> PathBuf {
    fs::canonicalize(p).unwrap()
}

fn mk(p: impl AsRef<Path>) {
    fs::create_dir_all(p).unwrap();
}

fn touch(p: impl AsRef<Path>) {
    if let Some(parent) = p.as_ref().parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, "").unwrap();
}

fn dir(path: impl AsRef<Path>) -> DirectorySuggestionEntry {
    DirectorySuggestionEntry {
        path: path.as_ref().to_string_lossy().into_owned(),
        kind: SuggestionKind::Directory,
    }
}

fn file(path: impl AsRef<Path>) -> DirectorySuggestionEntry {
    DirectorySuggestionEntry {
        path: path.as_ref().to_string_lossy().into_owned(),
        kind: SuggestionKind::File,
    }
}

fn init_git_repo(root: &Path, ignore: &str) {
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(root)
        .status()
        .unwrap()
        .success());
    fs::write(root.join(".gitignore"), ignore).unwrap();
}

fn paths(entries: &[DirectorySuggestionEntry]) -> Vec<String> {
    entries.iter().map(|e| e.path.clone()).collect()
}

// Workspace-style search: relative output, slashes policy, blank = children.
fn workspace(cwd: &Path, query: &str) -> SearchOptions {
    let mut o = SearchOptions::new(cwd.to_str().unwrap(), query, PathFormat::Relative);
    o.path_query_policy = Some(PathQueryPolicy::Slashes);
    o.blank_query_behavior = Some(BlankQueryBehavior::Children);
    o.traversable_hidden_directory_names = WORKSPACE_SEARCH_HIDDEN_DIRECTORIES
        .iter()
        .map(|s| s.to_string())
        .collect();
    o
}

struct Fixture {
    _guard: tempfile::TempDir,
    configured: PathBuf,
    root: PathBuf,
}

fn fixture() -> Fixture {
    let guard = tempfile::tempdir().unwrap();
    let configured = guard.path().to_path_buf();
    let root = canon(&configured);
    mk(root.join("projects/paseo-desktop"));
    mk(root.join("src/components"));
    mk(root.join(".hidden/secret"));
    touch(root.join("src/components/message-renderer.tsx"));
    Fixture {
        _guard: guard,
        configured,
        root,
    }
}

#[test]
fn applies_result_paths_and_entry_kinds_as_parameters_of_one_search() {
    let f = fixture();
    let mut o = SearchOptions::new(f.configured.to_str().unwrap(), "pso", PathFormat::Absolute);
    o.include_files = Some(false);
    o.include_directories = Some(true);
    let directories = search_directory_entries(&o);
    let mut o = SearchOptions::new(f.root.to_str().unwrap(), "msgrndr", PathFormat::Relative);
    o.include_files = Some(true);
    o.include_directories = Some(false);
    let files = search_directory_entries(&o);
    assert_eq!(
        directories,
        vec![dir(f.root.join("projects/paseo-desktop"))]
    );
    assert_eq!(files, vec![file("src/components/message-renderer.tsx")]);
}

#[test]
fn prunes_directories_ignored_by_git() {
    let f = fixture();
    init_git_repo(&f.root, "data/\n");
    mk(f.root.join("data/matching-ignored-directory"));
    mk(f.root.join("visible/matching-visible-directory"));
    let run = |q: &str| {
        let mut o = SearchOptions::new(f.root.to_str().unwrap(), q, PathFormat::Relative);
        o.include_files = Some(false);
        o.include_directories = Some(true);
        o.respect_git_ignore = true;
        search_directory_entries(&o)
    };
    assert_eq!(
        run("matching"),
        vec![dir("visible/matching-visible-directory")]
    );
    assert_eq!(
        run("visible"),
        vec![dir("visible"), dir("visible/matching-visible-directory")]
    );
}

#[test]
fn prunes_a_symlink_whose_target_is_inside_a_git_ignored_directory() {
    let f = fixture();
    init_git_repo(&f.root, "generated/\n");
    mk(f.root.join("generated/output"));
    symlink(f.root.join("generated/output"), f.root.join("linked")).unwrap();
    let mut o = SearchOptions::new(f.root.to_str().unwrap(), "linked", PathFormat::Relative);
    o.respect_git_ignore = true;
    assert_eq!(search_directory_entries(&o), vec![]);
}

#[test]
fn configures_raw_blank_queries_independently_from_explicit_root_aliases() {
    let f = fixture();
    let root_entries = vec![dir("projects"), dir("src")];
    let run = |query: &str, behavior: BlankQueryBehavior, mode: Option<MatchMode>| {
        let mut o = SearchOptions::new(f.root.to_str().unwrap(), query, PathFormat::Relative);
        o.include_files = Some(false);
        o.include_directories = Some(true);
        o.root_aliases = vec!["~".into()];
        o.blank_query_behavior = Some(behavior);
        o.match_mode = mode;
        search_directory_entries(&o)
    };
    assert_eq!(run("", BlankQueryBehavior::None, None), vec![]);
    assert_eq!(run("~", BlankQueryBehavior::None, None), root_entries);
    assert_eq!(run("", BlankQueryBehavior::Children, None), root_entries);
    assert_eq!(
        run("", BlankQueryBehavior::Children, Some(MatchMode::Suffix)),
        root_entries
    );
    assert_eq!(
        run("~", BlankQueryBehavior::None, Some(MatchMode::Suffix)),
        root_entries
    );
}

#[test]
fn matches_rooted_queries_against_the_full_path_at_any_depth() {
    let f = fixture();
    mk(f.root.join("nested/pso-global"));
    mk(f.root.join("pso-root"));
    let absolute_query = f.configured.join("pso").to_string_lossy().into_owned();
    let expected = vec![
        dir("pso-root"),
        dir("nested/pso-global"),
        dir("projects/paseo-desktop"),
    ];
    for q in ["~/pso", "./pso", absolute_query.as_str()] {
        let mut o = SearchOptions::new(f.configured.to_str().unwrap(), q, PathFormat::Relative);
        o.include_files = Some(false);
        o.include_directories = Some(true);
        o.path_query_policy = Some(PathQueryPolicy::Rooted);
        o.root_aliases = vec!["~".into()];
        assert_eq!(search_directory_entries(&o), expected, "{q}");
    }
}

#[test]
fn browses_an_absolute_root_and_an_absolute_directory_ending_in_a_separator() {
    let f = fixture();
    let run = |q: &str| {
        let mut o = SearchOptions::new(f.configured.to_str().unwrap(), q, PathFormat::Relative);
        o.include_files = Some(false);
        o.include_directories = Some(true);
        o.path_query_policy = Some(PathQueryPolicy::Rooted);
        search_directory_entries(&o)
    };
    assert_eq!(
        run(f.configured.to_str().unwrap()),
        vec![dir("projects"), dir("src")]
    );
    let projects = format!("{}/", f.configured.join("projects").display());
    assert_eq!(
        run(&projects),
        vec![dir("projects"), dir("projects/paseo-desktop")]
    );
}

#[test]
fn anchors_single_segment_absolute_queries_when_the_search_root_is_a_filesystem_root() {
    let exact = "/usr";
    let run = |q: &str| {
        let mut o = SearchOptions::new("/", q, PathFormat::Absolute);
        o.include_files = Some(false);
        o.include_directories = Some(true);
        o.path_query_policy = Some(PathQueryPolicy::Rooted);
        o.limit = Some(5);
        search_directory_entries(&o)
    };
    assert_eq!(run("/us"), vec![]);
    let entries = run(exact);
    assert_eq!(entries[0], dir(exact));
    assert!(entries
        .iter()
        .all(|e| e.path == exact || e.path.starts_with("/usr/")));
}

#[test]
fn does_not_return_entries_below_the_configured_traversal_depth() {
    let f = fixture();
    let mut o = SearchOptions::new(
        f.root.to_str().unwrap(),
        "message-renderer",
        PathFormat::Relative,
    );
    o.include_files = Some(true);
    o.include_directories = Some(false);
    o.max_depth = Some(2);
    assert_eq!(search_directory_entries(&o), vec![]);
}

#[test]
fn does_not_spend_the_scan_budget_on_excluded_entry_kinds() {
    let f = fixture();
    let budget_root = f.root.join("kind-budget");
    mk(budget_root.join("z-projects/paseo-target"));
    for i in 0..10 {
        touch(budget_root.join(format!("a-noise-{i}.txt")));
    }
    let mut o = SearchOptions::new(
        budget_root.to_str().unwrap(),
        "paseo-target",
        PathFormat::Relative,
    );
    o.include_files = Some(false);
    o.include_directories = Some(true);
    o.max_entries_scanned = Some(2);
    assert_eq!(
        search_directory_entries(&o),
        vec![dir("z-projects/paseo-target")]
    );
}

#[test]
fn applies_ignored_directory_policy_to_parent_scoped_queries() {
    let f = fixture();
    mk(f.root.join("node_modules"));
    let o = SearchOptions::new(f.root.to_str().unwrap(), "./node", PathFormat::Relative);
    assert_eq!(search_directory_entries(&o), vec![]);
}

#[test]
fn does_not_traverse_dependency_environment_or_build_output_directories() {
    let f = fixture();
    for name in [
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
    ] {
        touch(f.root.join(name).join("search-target.ts"));
    }
    touch(f.root.join("src/search-target.ts"));
    let mut o = SearchOptions::new(
        f.root.to_str().unwrap(),
        "search-target.ts",
        PathFormat::Relative,
    );
    o.include_files = Some(true);
    o.include_directories = Some(false);
    assert_eq!(
        search_directory_entries(&o),
        vec![file("src/search-target.ts")]
    );
}

#[test]
fn rechecks_symlink_children_against_each_search_root() {
    let f = fixture();
    let narrow = f.root.join("narrow");
    let outside = f.root.join("outside");
    mk(&narrow);
    mk(outside.join("leaked-child"));
    symlink(&outside, narrow.join("outside-link")).unwrap();
    let mut o = SearchOptions::new(
        f.root.to_str().unwrap(),
        "leaked-child",
        PathFormat::Absolute,
    );
    o.include_directories = Some(true);
    assert!(!search_directory_entries(&o).is_empty());
    let o = SearchOptions::new(narrow.to_str().unwrap(), "outside", PathFormat::Relative);
    assert_eq!(search_directory_entries(&o), vec![]);
}

#[test]
fn rechecks_a_symlink_after_its_target_changes() {
    let f = fixture();
    let narrow = f.root.join("retargeted-link-root");
    let inside = narrow.join("inside");
    let outside = f.root.join("retargeted-link-outside");
    let link = narrow.join("project-link");
    mk(&inside);
    mk(&outside);
    symlink(&inside, &link).unwrap();
    let run = || {
        search_directory_entries(&SearchOptions::new(
            narrow.to_str().unwrap(),
            "project-link",
            PathFormat::Relative,
        ))
    };
    assert_eq!(run(), vec![dir("project-link")]);
    fs::remove_file(&link).unwrap();
    symlink(&outside, &link).unwrap();
    assert_eq!(run(), vec![]);
}

#[test]
fn sees_a_child_created_after_a_previous_search() {
    let f = fixture();
    let dynamic = f.root.join("dynamic-cache-root");
    mk(&dynamic);
    let run = || {
        search_directory_entries(&SearchOptions::new(
            dynamic.to_str().unwrap(),
            "fresh-project",
            PathFormat::Relative,
        ))
    };
    assert_eq!(run(), vec![]);
    mk(dynamic.join("fresh-project"));
    assert_eq!(run(), vec![dir("fresh-project")]);
}

struct Home {
    _guard: tempfile::TempDir,
    temp_root: PathBuf,
    home: PathBuf,
    outside: PathBuf,
}

fn home_fixture() -> Home {
    let guard = tempfile::tempdir().unwrap();
    let temp_root = canon(guard.path());
    let home = temp_root.join("home");
    let outside = temp_root.join("outside");
    mk(&home);
    mk(&outside);
    mk(home.join("projects/paseo"));
    mk(home.join("projects/playground"));
    mk(home.join("documents/plans"));
    mk(home.join(".hidden/cache"));
    touch(home.join("projects/README.md"));
    mk(outside.join("outside-match"));
    symlink(outside.join("outside-match"), home.join("outside-link")).unwrap();
    Home {
        _guard: guard,
        temp_root,
        home,
        outside,
    }
}

fn run_absolute(
    home: &Path,
    query: &str,
    limit: Option<i64>,
    max_depth: Option<usize>,
    max_scanned: Option<u64>,
) -> Vec<String> {
    let mut o = SearchOptions::new(home.to_str().unwrap(), query, PathFormat::Absolute);
    o.include_directories = Some(true);
    o.include_files = Some(false);
    o.path_query_policy = Some(PathQueryPolicy::Rooted);
    o.root_aliases = vec!["~".into()];
    o.blank_query_behavior = Some(BlankQueryBehavior::None);
    o.limit = limit;
    o.max_depth = max_depth;
    o.max_entries_scanned = max_scanned;
    o.confident_result_scan_threshold = Some(5_000);
    paths(&search_directory_entries(&o))
}

fn canon_all(paths: Vec<String>) -> Vec<PathBuf> {
    paths.into_iter().map(canon).collect()
}

#[test]
fn does_not_inspect_directories_when_the_scan_budget_is_zero() {
    let h = home_fixture();
    assert_eq!(
        run_absolute(&h.home, "documents", Some(10), None, Some(0)),
        Vec::<String>::new()
    );
}

#[test]
fn shares_the_scan_budget_fairly_between_nested_sibling_branches() {
    let h = home_fixture();
    let budget_home = h.temp_root.join("nested-budget-home");
    let project = budget_home.join("work/client/team/paseo-desktop");
    mk(&project);
    for i in 0..10 {
        mk(budget_home.join(format!("work/archive/noise-{i:02}")));
    }
    let results = run_absolute(&budget_home, "paseo-desktop", Some(10), None, Some(8));
    assert_eq!(canon_all(results), vec![canon(&project)]);
}

#[test]
fn does_not_let_a_queued_symlink_hide_the_direct_project_branch() {
    let h = home_fixture();
    let symlink_home = h.temp_root.join("symlink-budget-home");
    let project_root = symlink_home.join("b-projects/project-root");
    let project = project_root.join("paseo-desktop");
    let noisy = symlink_home.join("a-noisy");
    mk(&project);
    for i in 0..10 {
        mk(noisy.join(format!("noise-{i:02}")));
    }
    symlink(&project_root, noisy.join("zz-target-link")).unwrap();
    let results = run_absolute(&symlink_home, "paseo-desktop", Some(10), None, Some(6));
    assert_eq!(canon_all(results), vec![canon(&project)]);
}

#[test]
fn follows_visible_directory_symlinks_that_stay_inside_home() {
    let h = home_fixture();
    let symlink_home = h.temp_root.join("internal-symlink-home");
    let project = symlink_home.join(".linked/project-root/paseo-desktop");
    mk(&project);
    symlink(
        project.parent().unwrap(),
        symlink_home.join("linked-project"),
    )
    .unwrap();
    let results = run_absolute(&symlink_home, "pso", Some(10), None, None);
    assert_eq!(canon_all(results), vec![canon(&project)]);
}

#[test]
fn matches_the_visible_name_of_a_directory_symlink() {
    let h = home_fixture();
    let symlink_home = h.temp_root.join("visible-symlink-home");
    let projects = symlink_home.join("projects");
    let target = symlink_home.join("work/current");
    let visible = projects.join("paseo");
    mk(&projects);
    mk(&target);
    symlink(&target, &visible).unwrap();
    let results = run_absolute(&symlink_home, "paseo", Some(10), None, None);
    assert!(
        results.contains(&visible.to_string_lossy().into_owned()),
        "{results:?}"
    );
}

#[test]
fn keeps_scanning_past_weak_fuzzy_matches_for_a_stronger_late_result() {
    let h = home_fixture();
    let large = h.temp_root.join("large-home");
    for i in 0..8 {
        mk(large.join(format!("a-{i:02}-project-search-output")));
    }
    mk(large.join("pso"));
    let mut o = SearchOptions::new(large.to_str().unwrap(), "pso", PathFormat::Absolute);
    o.include_directories = Some(true);
    o.limit = Some(1);
    o.max_entries_scanned = Some(20);
    o.confident_result_scan_threshold = Some(5);
    assert_eq!(search_directory_entries(&o), vec![dir(large.join("pso"))]);
}

#[test]
fn supports_home_relative_path_query_syntax() {
    let h = home_fixture();
    let results = run_absolute(&h.home, "~/projects/pa", Some(10), None, None);
    assert_eq!(
        canon_all(results),
        vec![
            canon(h.home.join("projects/paseo")),
            canon(h.home.join("projects/playground"))
        ]
    );
}

#[test]
fn prioritizes_partial_matches_that_appear_earlier_in_the_path() {
    let h = home_fixture();
    mk(h.home.join("farofoo"));
    mk(h.home.join("x/y/farofoo"));
    let results = canon_all(run_absolute(&h.home, "arofo", Some(30), None, None));
    let earlier = results
        .iter()
        .position(|p| *p == canon(h.home.join("farofoo")))
        .unwrap();
    let later = results
        .iter()
        .position(|p| *p == canon(h.home.join("x/y/farofoo")))
        .unwrap();
    assert!(earlier < later);
}

#[test]
fn does_not_return_paths_that_escape_home_through_symlinks() {
    let h = home_fixture();
    let results = run_absolute(&h.home, "outside", Some(20), None, None);
    assert!(!results.contains(&h.home.join("outside-link").to_string_lossy().into_owned()));
    assert!(!results.contains(
        &h.outside
            .join("outside-match")
            .to_string_lossy()
            .into_owned()
    ));
}

#[test]
fn respects_the_result_limit() {
    let h = home_fixture();
    assert_eq!(run_absolute(&h.home, "p", Some(1), None, None).len(), 1);
}

struct Excluded {
    _guard: tempfile::TempDir,
    home: PathBuf,
    library: PathBuf,
}

fn excluded_fixture() -> Excluded {
    let guard = tempfile::tempdir().unwrap();
    let home = canon(guard.path());
    let library = home.join("Library");
    mk(library.join("sub/match-me"));
    mk(home.join("projects/other"));
    Excluded {
        _guard: guard,
        home,
        library,
    }
}

fn search_home(e: &Excluded, query: &str) -> Vec<String> {
    let mut o = SearchOptions::new(e.home.to_str().unwrap(), query, PathFormat::Absolute);
    o.include_directories = Some(true);
    o.include_files = Some(false);
    o.path_query_policy = Some(PathQueryPolicy::Rooted);
    o.root_aliases = vec!["~".into()];
    o.blank_query_behavior = Some(BlankQueryBehavior::None);
    o.confident_result_scan_threshold = Some(5_000);
    o.excluded_discovery_paths = vec![e.library.to_string_lossy().into_owned()];
    paths(&search_directory_entries(&o))
}

fn s(p: PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

#[test]
fn does_not_discover_entries_inside_an_excluded_path() {
    let e = excluded_fixture();
    assert_eq!(search_home(&e, "match"), Vec::<String>::new());
    assert_eq!(search_home(&e, "~"), vec![s(e.home.join("projects"))]);
}

#[test]
fn excludes_by_path_so_a_nested_directory_with_the_same_name_is_still_discovered() {
    let e = excluded_fixture();
    let nested = e.home.join("projects/game/Library/match-me");
    mk(&nested);
    assert_eq!(search_home(&e, "match"), vec![s(nested)]);
}

#[test]
fn does_not_discover_an_excluded_path_through_a_symlink() {
    let e = excluded_fixture();
    symlink(e.library.join("sub"), e.home.join("link")).unwrap();
    assert_eq!(search_home(&e, "match"), Vec::<String>::new());
}

#[test]
fn resolves_an_exact_typed_path_inside_an_excluded_path() {
    let e = excluded_fixture();
    assert_eq!(
        search_home(&e, "~/Library/sub/match-me"),
        vec![s(e.library.join("sub/match-me"))]
    );
}

#[test]
fn lists_one_level_when_browsing_inside_an_excluded_path() {
    let e = excluded_fixture();
    assert_eq!(
        search_home(&e, "~/Library/"),
        vec![s(e.library.clone()), s(e.library.join("sub"))]
    );
    assert_eq!(
        search_home(&e, "~/Library/su"),
        vec![s(e.library.join("sub"))]
    );
}

fn typed_parent_home() -> (tempfile::TempDir, PathBuf) {
    let guard = tempfile::tempdir().unwrap();
    let home = canon(guard.path());
    mk(home.join("Developer/pasta"));
    mk(home.join("Archive/Developer/pasta"));
    (guard, home)
}

#[test]
fn walks_only_under_the_typed_parent_when_it_exists() {
    let (_g, home) = typed_parent_home();
    assert_eq!(
        run_absolute(&home, "~/Developer/pas", None, None, None),
        vec![s(home.join("Developer/pasta"))]
    );
}

#[test]
fn matches_the_whole_tree_when_the_typed_parent_does_not_exist() {
    let (_g, home) = typed_parent_home();
    assert_eq!(
        run_absolute(&home, "~/Devel/pas", None, None, None),
        vec![
            s(home.join("Developer/pasta")),
            s(home.join("Archive/Developer/pasta"))
        ]
    );
}

#[test]
fn returns_the_typed_directory_first() {
    let (_g, home) = typed_parent_home();
    mk(home.join("Developer/pasta-extra"));
    let results = run_absolute(&home, "~/Developer/pasta", None, None, None);
    assert_eq!(results[0], s(home.join("Developer/pasta")));
}

struct Ws {
    _guard: tempfile::TempDir,
    root: PathBuf,
    dir: PathBuf,
}

fn ws_fixture() -> Ws {
    let guard = tempfile::tempdir().unwrap();
    let root = canon(guard.path());
    let dir = root.join("workspace");
    mk(dir.join("src/components"));
    mk(dir.join("docs"));
    fs::write(dir.join("README.md"), "# paseo\n").unwrap();
    fs::write(dir.join("src/components/chat-input.tsx"), "x").unwrap();
    fs::write(dir.join("docs/search-notes.md"), "notes\n").unwrap();
    Ws {
        _guard: guard,
        root,
        dir,
    }
}

fn ws_search(
    w: &Ws,
    query: &str,
    files: bool,
    dirs: bool,
    mode: Option<MatchMode>,
) -> Vec<DirectorySuggestionEntry> {
    let mut o = workspace(&w.dir, query);
    o.include_files = Some(files);
    o.include_directories = Some(dirs);
    o.match_mode = mode;
    o.limit = Some(20);
    search_directory_entries(&o)
}

#[test]
fn ranks_fuzzy_basename_matches_after_exact_prefix_and_substring_matches() {
    let w = ws_fixture();
    for n in [
        "msgrndr",
        "msgrndr-panel.tsx",
        "use-msgrndr.ts",
        "message-renderer.tsx",
    ] {
        touch(w.dir.join("src/components").join(n));
    }
    let results = ws_search(&w, "msgrndr", true, false, None);
    assert_eq!(
        paths(&results),
        vec![
            "src/components/msgrndr",
            "src/components/msgrndr-panel.tsx",
            "src/components/use-msgrndr.ts",
            "src/components/message-renderer.tsx"
        ]
    );
}

#[test]
fn suffix_mode_matches_whole_path_segment_suffixes_without_fuzzy_matches() {
    let w = ws_fixture();
    mk(w.dir.join("packages/app/src"));
    touch(w.dir.join("src/file.ts"));
    touch(w.dir.join("packages/app/src/file.ts"));
    touch(w.dir.join("src/paseo-config-file.ts"));
    let expected = vec![file("src/file.ts"), file("packages/app/src/file.ts")];
    assert_eq!(
        ws_search(&w, "file.ts", true, false, Some(MatchMode::Suffix)),
        expected
    );
    assert_eq!(
        ws_search(&w, "src/file.ts", true, false, Some(MatchMode::Suffix)),
        expected
    );
}

#[test]
fn suffix_mode_resolves_exact_workspace_file_paths_before_broad_traversal() {
    let w = ws_fixture();
    touch(w.dir.join("plugins/usage-sources/providers/local.ts"));
    let mut o = workspace(&w.dir, "plugins/usage-sources/providers/local.ts");
    o.include_files = Some(true);
    o.include_directories = Some(false);
    o.match_mode = Some(MatchMode::Suffix);
    o.max_entries_scanned = Some(1);
    assert_eq!(
        search_directory_entries(&o),
        vec![file("plugins/usage-sources/providers/local.ts")]
    );
}

#[test]
fn suffix_mode_resolves_explicit_hidden_file_paths_without_broad_hidden_traversal() {
    let w = ws_fixture();
    fs::create_dir_all(w.dir.join(".dev/paseo-home")).unwrap();
    fs::write(w.dir.join(".dev/paseo-home/daemon.log"), "daemon log\n").unwrap();
    let mut o = workspace(&w.dir, ".dev/paseo-home/daemon.log");
    o.include_files = Some(true);
    o.include_directories = Some(false);
    o.match_mode = Some(MatchMode::Suffix);
    o.max_entries_scanned = Some(1);
    assert_eq!(
        search_directory_entries(&o),
        vec![file(".dev/paseo-home/daemon.log")]
    );
}

#[test]
fn resolves_an_exact_gitignored_path_while_keeping_it_out_of_discovery_results() {
    let w = ws_fixture();
    init_git_repo(&w.dir, "generated/\n");
    fs::create_dir_all(w.dir.join("generated")).unwrap();
    fs::write(w.dir.join("generated/search-notes.md"), "generated notes\n").unwrap();
    let mut exact = workspace(&w.dir, "generated/search-notes.md");
    exact.include_files = Some(true);
    exact.include_directories = Some(false);
    exact.match_mode = Some(MatchMode::Suffix);
    exact.respect_git_ignore = true;
    exact.limit = Some(1);
    let mut fuzzy = workspace(&w.dir, "search-notes");
    fuzzy.include_files = Some(true);
    fuzzy.include_directories = Some(false);
    fuzzy.respect_git_ignore = true;
    fuzzy.limit = Some(20);
    assert_eq!(
        search_directory_entries(&exact),
        vec![file("generated/search-notes.md")]
    );
    assert_eq!(
        search_directory_entries(&fuzzy),
        vec![file("docs/search-notes.md")]
    );
}

#[test]
fn refuses_an_exact_path_that_escapes_the_root_through_a_symlink() {
    let w = ws_fixture();
    let outside = w.root.join("outside-workspace");
    mk(&outside);
    fs::write(outside.join("secret.md"), "outside\n").unwrap();
    symlink(&outside, w.dir.join("escape-link")).unwrap();
    symlink(w.dir.join("docs"), w.dir.join("inside-link")).unwrap();
    let run = |q: &str| {
        let mut o = workspace(&w.dir, q);
        o.include_files = Some(true);
        o.include_directories = Some(false);
        o.match_mode = Some(MatchMode::Suffix);
        o.limit = Some(1);
        search_directory_entries(&o)
    };
    assert_eq!(run("escape-link/secret.md"), vec![]);
    assert_eq!(
        run("inside-link/search-notes.md"),
        vec![file("inside-link/search-notes.md")]
    );
}

#[test]
fn traverses_only_allowlisted_hidden_directories_without_suggesting_the_directories() {
    let w = ws_fixture();
    touch(w.dir.join(".claude/settings.local.json"));
    touch(w.dir.join(".dev/cache/settings.local.json"));
    let expected = vec![file(".claude/settings.local.json")];
    assert_eq!(
        ws_search(
            &w,
            "settings.local.json",
            true,
            true,
            Some(MatchMode::Suffix)
        ),
        expected
    );
    assert_eq!(
        ws_search(&w, "claude", true, true, Some(MatchMode::Fuzzy)),
        expected
    );
}

#[test]
fn supports_slash_style_path_queries() {
    let w = ws_fixture();
    assert_eq!(
        ws_search(&w, "src/co", true, true, None),
        vec![dir("src/components"), file("src/components/chat-input.tsx")]
    );
}

#[test]
fn matches_a_path_fragment_anywhere_in_the_full_file_path() {
    let w = ws_fixture();
    touch(
        w.dir
            .join("something/something-else/skills/paseo-advisor/SKILL.md"),
    );
    assert_eq!(
        ws_search(&w, "skills/", true, false, None),
        vec![file(
            "something/something-else/skills/paseo-advisor/SKILL.md"
        )]
    );
}

#[test]
fn fuzzy_matches_natural_language_against_the_full_path() {
    let w = ws_fixture();
    mk(w.dir.join("blankpage/editor"));
    assert_eq!(
        ws_search(&w, "blank page editor", false, true, None),
        vec![dir("blankpage/editor")]
    );
}

#[test]
fn blank_workspace_query_lists_visible_children_only() {
    let w = ws_fixture();
    touch(w.dir.join(".env"));
    mk(w.dir.join("node_modules"));
    let results = ws_search(&w, "", true, true, None);
    let names = paths(&results);
    assert!(names.contains(&"src".to_string()) && names.contains(&"README.md".to_string()));
    assert!(!names
        .iter()
        .any(|n| n.starts_with('.') || n == "node_modules"));
}

#[test]
fn neither_kind_requested_returns_nothing_and_missing_root_returns_nothing() {
    let w = ws_fixture();
    assert_eq!(ws_search(&w, "src", false, false, None), vec![]);
    let o = SearchOptions::new("/definitely/not/here", "x", PathFormat::Relative);
    assert_eq!(search_directory_entries(&o), vec![]);
}

#[test]
fn limit_is_clamped_to_one_through_one_hundred() {
    let w = ws_fixture();
    for i in 0..120 {
        mk(w.dir.join(format!("many/item-{i:03}")));
    }
    let mut o = workspace(&w.dir, "item");
    o.limit = Some(1000);
    assert_eq!(search_directory_entries(&o).len(), 100);
    o.limit = Some(0);
    assert_eq!(search_directory_entries(&o).len(), 1);
}
