//! Phase 5: Git Worktree Tests (WT1-WT20)
//!
//! These tests cover the worktree bootstrap behavior that should copy a
//! prebuilt DB and then run smart_scan.

mod common;

use assert_cmd::Command;
use assert_fs::TempDir;
use common::TestFixture;
use source_fast_core::PersistentIndex;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

fn db_path(root: &Path) -> PathBuf {
    root.join(".source_fast").join("index.db")
}

fn sf_index(root: &Path) {
    Command::cargo_bin("sf")
        .unwrap()
        .current_dir(root)
        .arg("index")
        .arg("--root")
        .arg(root)
        .assert()
        .success();
}

fn sf_search(root: &Path, query: &str) -> String {
    let output = Command::cargo_bin("sf")
        .unwrap()
        .current_dir(root)
        .arg("search")
        .arg("--root")
        .arg(root)
        .arg(query)
        .output()
        .expect("sf search failed");
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn set_meta(root: &Path, key: &str, value: &str) {
    let path = db_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let index = PersistentIndex::open_or_create(&path).unwrap();
    index.set_meta(key, value).unwrap();
}

fn get_meta(root: &Path, key: &str) -> Option<String> {
    let index = PersistentIndex::open_or_create(&db_path(root)).unwrap();
    index.get_meta(key).unwrap()
}

fn write_file(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, content).unwrap();
}

fn remove_file(root: &Path, rel: &str) {
    let path = root.join(rel);
    if path.exists() {
        std::fs::remove_file(path).unwrap();
    }
}

fn git_in(dir: &Path, args: &[&str]) -> Output {
    StdCommand::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git command failed")
}

fn assert_git_ok(output: &Output, context: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{context} failed: {stderr}");
}

fn git_head(dir: &Path) -> String {
    let output = git_in(dir, &["rev-parse", "HEAD"]);
    assert_git_ok(&output, "git rev-parse HEAD");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn add_worktree(fix: &TestFixture, target: &Path, git_ref: &str) {
    let target_str = target
        .to_str()
        .expect("worktree path must be utf-8");
    let output = fix.git(&["worktree", "add", target_str, git_ref]);
    assert_git_ok(&output, "git worktree add");
}

fn assert_search_contains(root: &Path, query: &str, needle: &str) {
    let stdout = sf_search(root, query);
    assert!(
        stdout.contains(needle),
        "Expected search output to contain '{needle}', got: {stdout}"
    );
}

fn assert_search_not_contains(root: &Path, query: &str, needle: &str) {
    let stdout = sf_search(root, query);
    assert!(
        !stdout.contains(needle),
        "Expected search output to omit '{needle}', got: {stdout}"
    );
}

/// WT1: Worktree copy preserves meta marker from main DB.
#[test]
fn test_wt1_copy_preserves_meta_marker() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn main() { /* wt1 */ }");
    fix.git_commit("initial");

    fix.index();

    set_meta(&fix.root(), "worktree_copy_marker", "main");

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "HEAD");

    sf_index(worktree_root);

    let marker = get_meta(worktree_root, "worktree_copy_marker");
    assert_eq!(
        marker.as_deref(),
        Some("main"),
        "worktree DB should preserve meta marker"
    );
}

/// WT2: Worktree copy + smart_scan removes stale entries.
#[test]
fn test_wt2_copy_removes_stale_entries() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/stale.rs", "fn stale_unique_wt2() {}");
    fix.git_commit("initial");
    fix.index();

    let output = fix.git(&["branch", "feature"]);
    assert_git_ok(&output, "git branch feature");

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "feature");

    let stale_path = worktree_root.join("src").join("stale.rs");
    std::fs::remove_file(&stale_path).unwrap();

    let output = git_in(worktree_root, &["add", "-A"]);
    assert_git_ok(&output, "git add -A in worktree");

    let output = git_in(worktree_root, &["commit", "-m", "remove stale"]);
    assert_git_ok(&output, "git commit in worktree");

    sf_index(worktree_root);

    let stdout = sf_search(worktree_root, "stale_unique_wt2");
    assert!(
        !stdout.contains("stale.rs"),
        "stale file should be removed from worktree index: {}",
        stdout
    );
}

/// WT3: Worktree copy + smart_scan picks up untracked files.
#[test]
fn test_wt3_copy_picks_up_untracked_files() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn base_unique_wt3() {}");
    fix.git_commit("initial");
    fix.index();

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "HEAD");

    let untracked_path = worktree_root.join("src").join("untracked.rs");
    std::fs::create_dir_all(untracked_path.parent().unwrap()).unwrap();
    std::fs::write(&untracked_path, "fn untracked_unique_wt3() {}").unwrap();

    sf_index(worktree_root);

    let stdout = sf_search(worktree_root, "untracked_unique_wt3");
    assert!(
        stdout.contains("untracked.rs"),
        "untracked file should be indexed in worktree: {}",
        stdout
    );
}

/// WT4: Worktree copy + smart_scan updates modified tracked files.
#[test]
fn test_wt4_copy_updates_modified_tracked_file() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn old_content_wt4() {}");
    fix.git_commit("initial");
    fix.index();

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "HEAD");

    #[cfg(windows)]
    std::thread::sleep(std::time::Duration::from_secs(2));
    #[cfg(not(windows))]
    std::thread::sleep(std::time::Duration::from_millis(100));

    write_file(worktree_root, "src/main.rs", "fn new_content_wt4() {}");

    sf_index(worktree_root);

    assert_search_contains(worktree_root, "new_content_wt4", "main.rs");
    assert_search_not_contains(worktree_root, "old_content_wt4", "main.rs");
}

/// WT5: Worktree copy + smart_scan handles mixed dirty state.
#[test]
fn test_wt5_copy_handles_mixed_dirty_state() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/keep.rs", "fn keep_unique_wt5() {}");
    fix.add_file("src/delete.rs", "fn delete_unique_wt5() {}");
    fix.git_commit("initial");
    fix.index();

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "HEAD");

    write_file(worktree_root, "src/keep.rs", "fn keep_modified_wt5() {}");
    write_file(worktree_root, "src/new.rs", "fn new_unique_wt5() {}");
    remove_file(worktree_root, "src/delete.rs");

    sf_index(worktree_root);

    assert_search_contains(worktree_root, "keep_modified_wt5", "keep.rs");
    assert_search_contains(worktree_root, "new_unique_wt5", "new.rs");
    assert_search_not_contains(worktree_root, "delete_unique_wt5", "delete.rs");
}

/// WT6: Search paths should reflect the worktree root, not the main root.
#[test]
fn test_wt6_paths_resolved_to_worktree_root() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/path.rs", "fn path_unique_wt6() {}");
    fix.git_commit("initial");
    fix.index();

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "HEAD");

    sf_index(worktree_root);

    let stdout = sf_search(worktree_root, "path_unique_wt6");
    let main_root = fix.root().display().to_string();
    let worktree_root_str = worktree_root.display().to_string();
    assert!(
        stdout.contains(&worktree_root_str),
        "Expected worktree path in output: {stdout}"
    );
    assert!(
        !stdout.contains(&main_root),
        "Output should not reference main root path: {stdout}"
    );
}

/// WT7: Renames in a worktree should update paths.
#[test]
fn test_wt7_copy_handles_rename() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/old_name.rs", "fn rename_unique_wt7() {}");
    fix.git_commit("initial");
    fix.index();

    let output = fix.git(&["branch", "rename-branch"]);
    assert_git_ok(&output, "git branch rename-branch");

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "rename-branch");

    let old_path = worktree_root.join("src").join("old_name.rs");
    let new_path = worktree_root.join("src").join("new_name.rs");
    std::fs::rename(old_path, new_path).unwrap();

    let output = git_in(worktree_root, &["add", "-A"]);
    assert_git_ok(&output, "git add -A rename");
    let output = git_in(worktree_root, &["commit", "-m", "rename file"]);
    assert_git_ok(&output, "git commit rename");

    sf_index(worktree_root);

    let stdout = sf_search(worktree_root, "rename_unique_wt7");
    assert!(stdout.contains("new_name.rs"), "Expected new path: {stdout}");
    assert!(
        !stdout.contains("old_name.rs"),
        "Old path should not appear: {stdout}"
    );
}

/// WT8: No source DB available should still allow indexing in worktree.
#[test]
fn test_wt8_no_source_db_fallback() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn fallback_unique_wt8() {}");
    fix.git_commit("initial");

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "HEAD");

    sf_index(worktree_root);
    assert_search_contains(worktree_root, "fallback_unique_wt8", "main.rs");
}

/// WT9: Corrupted DB should be rebuilt and still allow search.
#[test]
fn test_wt9_corrupted_db_rebuilds() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn corrupt_unique_wt9() {}");
    fix.git_commit("initial");
    fix.index();

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "HEAD");

    let db = db_path(worktree_root);
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&db, b"not a sqlite db").unwrap();

    sf_index(worktree_root);
    assert_search_contains(worktree_root, "corrupt_unique_wt9", "main.rs");
}

/// WT10: Missing schema should be repaired and indexing should succeed.
#[test]
fn test_wt10_missing_schema_rebuilds() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn schema_unique_wt10() {}");
    fix.git_commit("initial");
    fix.index();

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "HEAD");

    let db = db_path(worktree_root);
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }

    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE IF NOT EXISTS dummy (id INTEGER);")
            .unwrap();
    }

    sf_index(worktree_root);
    assert_search_contains(worktree_root, "schema_unique_wt10", "main.rs");
}

/// WT11: Invalid stored git_head should be handled (full scan fallback).
#[test]
fn test_wt11_invalid_git_head_fallback() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn invalid_head_wt11() {}");
    fix.git_commit("initial");
    fix.index();

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "HEAD");

    set_meta(worktree_root, "git_head", "not-a-valid-oid");
    sf_index(worktree_root);

    assert_search_contains(worktree_root, "invalid_head_wt11", "main.rs");
}

/// WT12: Same-commit worktree should index without errors.
#[test]
fn test_wt12_same_commit_no_dirty() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn same_commit_wt12() {}");
    fix.git_commit("initial");
    fix.index();

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "HEAD");

    sf_index(worktree_root);
    assert_search_contains(worktree_root, "same_commit_wt12", "main.rs");
}

/// WT13: Different commit (reachable) should update the index.
#[test]
fn test_wt13_different_commit_reachable() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/old.rs", "fn old_wt13() {}");
    fix.git_commit("initial");
    fix.index();

    let output = fix.git(&["branch", "feature-wt13"]);
    assert_git_ok(&output, "git branch feature-wt13");

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "feature-wt13");

    write_file(worktree_root, "src/new.rs", "fn new_wt13() {}");
    remove_file(worktree_root, "src/old.rs");

    let output = git_in(worktree_root, &["add", "-A"]);
    assert_git_ok(&output, "git add -A wt13");
    let output = git_in(worktree_root, &["commit", "-m", "change wt13"]);
    assert_git_ok(&output, "git commit wt13");

    sf_index(worktree_root);

    assert_search_contains(worktree_root, "new_wt13", "new.rs");
    assert_search_not_contains(worktree_root, "old_wt13", "old.rs");
}

/// WT14: Missing old commit should fall back to full scan.
#[test]
fn test_wt14_missing_old_commit_fallback() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn missing_commit_wt14() {}");
    fix.git_commit("initial");
    fix.index();

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "HEAD");

    set_meta(worktree_root, "git_head", "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
    sf_index(worktree_root);

    assert_search_contains(worktree_root, "missing_commit_wt14", "main.rs");
}

/// WT15: Concurrent worktree indexing should not corrupt DBs.
#[test]
fn test_wt15_concurrent_worktree_indexing() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn concurrent_wt15() {}");
    fix.git_commit("initial");
    fix.index();

    let worktree_dir_a = TempDir::new().unwrap();
    let worktree_root_a = worktree_dir_a.path().to_path_buf();
    add_worktree(&fix, &worktree_root_a, "HEAD");

    let worktree_dir_b = TempDir::new().unwrap();
    let worktree_root_b = worktree_dir_b.path().to_path_buf();
    add_worktree(&fix, &worktree_root_b, "HEAD");

    let root_a = Arc::new(worktree_root_a);
    let root_b = Arc::new(worktree_root_b);

    let a = {
        let root = Arc::clone(&root_a);
        thread::spawn(move || {
            sf_index(&root);
            sf_search(&root, "concurrent_wt15")
        })
    };
    let b = {
        let root = Arc::clone(&root_b);
        thread::spawn(move || {
            sf_index(&root);
            sf_search(&root, "concurrent_wt15")
        })
    };

    let output_a = a.join().unwrap();
    let output_b = b.join().unwrap();

    assert!(output_a.contains("main.rs"), "Worktree A search failed");
    assert!(output_b.contains("main.rs"), "Worktree B search failed");
}

/// WT16: Re-indexing after copy still updates content (writer thread running).
#[test]
fn test_wt16_reindex_updates_after_copy() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn initial_wt16() {}");
    fix.git_commit("initial");
    fix.index();

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "HEAD");

    sf_index(worktree_root);

    write_file(worktree_root, "src/main.rs", "fn updated_wt16() {}");
    sf_index(worktree_root);

    assert_search_contains(worktree_root, "updated_wt16", "main.rs");
}

/// WT17: Indexing should update git_head in meta.
#[test]
fn test_wt17_git_head_updates() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn head_update_wt17() {}");
    fix.git_commit("initial");
    fix.index();

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "HEAD");

    sf_index(worktree_root);

    let head = git_head(worktree_root);
    let stored = get_meta(worktree_root, "git_head");
    assert_eq!(stored.as_deref(), Some(head.as_str()));
}

/// WT18: Path normalization on Windows should use absolute paths.
#[test]
fn test_wt18_paths_are_absolute() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn absolute_path_wt18() {}");
    fix.git_commit("initial");
    fix.index();

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "HEAD");

    sf_index(worktree_root);

    let stdout = sf_search(worktree_root, "absolute_path_wt18");
    #[cfg(windows)]
    {
        let marker = "File: ";
        let path_start = stdout.find(marker).expect("Expected File: marker");
        let line = stdout[path_start + marker.len()..]
            .lines()
            .next()
            .unwrap_or("");
        let mut parts = line.rsplitn(2, ':');
        let _line_no = parts.next();
        let file_part = parts.next().unwrap_or(line);
        let normalized = file_part.strip_prefix(r"\\?\").unwrap_or(file_part);
        assert!(
            normalized.contains(":\\"),
            "Expected absolute Windows path, got: {file_part}"
        );
        assert!(
            !file_part.contains('/'),
            "Expected no forward slashes in Windows path, got: {file_part}"
        );
    }
}

/// WT19: Performance guardrail for worktree copy (ignored by default).
#[test]
#[ignore = "Performance guardrail; run manually on large repos"]
fn test_wt19_copy_is_faster_than_full_scan() {
    let fix = TestFixture::new();
    fix.git_init();
    for i in 0..200 {
        fix.add_file(
            &format!("src/file_{i}.rs"),
            &format!("fn perf_marker_{i}() {{}}"),
        );
    }
    fix.git_commit("many files");

    let start_full = Instant::now();
    fix.index();
    let full_duration = start_full.elapsed();

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "HEAD");

    let start_copy = Instant::now();
    sf_index(worktree_root);
    let copy_duration = start_copy.elapsed();

    assert!(
        copy_duration <= full_duration * 2,
        "Expected worktree index to be faster (or comparable). full={full_duration:?}, worktree={copy_duration:?}"
    );
}

/// WT20: Copy failure should fall back to full scan.
#[test]
fn test_wt20_copy_failure_fallbacks_to_full_scan() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn copy_fail_wt20() {}");
    fix.git_commit("initial");

    let source_db = fix.root().join(".source_fast").join("index.db");
    if let Some(parent) = source_db.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    if source_db.exists() {
        std::fs::remove_file(&source_db).ok();
        std::fs::remove_dir_all(&source_db).ok();
    }
    std::fs::create_dir_all(&source_db).unwrap();

    let worktree_dir = TempDir::new().unwrap();
    let worktree_root = worktree_dir.path();
    add_worktree(&fix, worktree_root, "HEAD");

    sf_index(worktree_root);
    assert_search_contains(worktree_root, "copy_fail_wt20", "main.rs");
}
