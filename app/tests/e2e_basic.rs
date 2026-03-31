//! Phase 1: Basic Functionality Tests (B1-B4)
//!
//! These tests verify the core engine works under normal conditions.

mod common;

use common::TestFixture;
use predicates::prelude::*;

/// B1: Fresh Init
/// Run `sf search --wait` on a fresh directory.
/// Expected: DB created, files indexed.
#[test]
fn test_b1_fresh_init() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn main() { println!(\"hello\"); }");
    fix.add_file("src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }");

    // Search triggers daemon start + indexing
    let _ = fix.search("main");

    // DB should be created
    assert!(fix.db_path().exists(), "Database file should be created");
}

/// B2: Basic Search
/// Run `sf search` for content that exists.
/// Expected: Returns file path and snippet.
#[test]
fn test_b2_basic_search() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn main() { println!(\"hello world\"); }");
    fix.add_file(
        "src/lib.rs",
        "pub fn calculate_sum(a: i32, b: i32) -> i32 { a + b }",
    );

    // Search for existing content
    let output = fix.search("calculate_sum");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("lib.rs"),
        "Should find lib.rs in results: {}",
        stdout
    );
}

/// B3: No Match
/// Run `sf search` for content that doesn't exist.
/// Expected: Returns "No results" or similar message.
#[test]
fn test_b3_no_match() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn main() { println!(\"hello\"); }");

    // Search for non-existent content
    let output = fix.search("xyz_nonexistent_pattern_123");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should either have empty results or "No results" message
    assert!(
        stdout.contains("No results") || stdout.trim().is_empty() || !stdout.contains("main.rs"),
        "Should not find non-existent content, got: {}",
        stdout
    );
}

/// B4: Re-search (daemon already running)
/// Run `sf search` twice.
/// Expected: Second search works fine (daemon already running).
#[test]
fn test_b4_research_daemon_running() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn main() { println!(\"hello\"); }");
    fix.add_file("src/lib.rs", "pub fn foo() {}");
    fix.add_file("src/utils.rs", "pub fn bar() {}");

    // First search (starts daemon)
    let _ = fix.search("foo");

    // Second search (daemon already running)
    let output = fix.search("foo");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("lib.rs"),
        "Should find lib.rs in results: {}",
        stdout
    );
}

/// Additional: Test search with file filter
#[test]
fn test_search_with_file_filter() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn process_data_filtered() {}");
    fix.add_file("src/lib.rs", "fn process_data_filtered() {}");
    fix.add_file("tests/test.rs", "fn process_data_filtered() {}");

    // Search with file regex filter - only .rs files containing "main"
    // Use a regex that works on both Unix (/) and Windows (\) paths
    fix.sf()
        .arg("search")
        .arg("--root")
        .arg(fix.root())
        .arg("--wait")
        .arg("--file-regex")
        .arg("main")
        .arg("process_data_filtered")
        .assert()
        .success()
        .stdout(predicate::str::contains("main.rs"));
}

/// Additional: Test search-file command
#[test]
fn test_search_file_by_path() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn main() {}");
    fix.add_file("src/lib.rs", "pub fn lib() {}");
    fix.add_file("src/utils/helpers.rs", "pub fn help() {}");

    // Search for files containing "helpers" in path
    let output = fix.search_file("helpers");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("helpers.rs"),
        "Should find helpers.rs: {}",
        stdout
    );
}

#[test]
fn test_daemon_and_index_status_commands() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn main() { println!(\"hello\"); }");
    fix.add_file("src/lib.rs", "pub fn status_probe() {}");

    let _ = fix.search("status_probe");

    fix.sf()
        .arg("daemon")
        .arg("status")
        .arg("--root")
        .arg(fix.root())
        .assert()
        .success()
        .stdout(predicate::str::contains("PID:"));

    fix.sf()
        .arg("deamon")
        .arg("status")
        .arg("--root")
        .arg(fix.root())
        .assert()
        .success()
        .stdout(predicate::str::contains("PID:"));

    fix.sf()
        .arg("index")
        .arg("status")
        .arg("--root")
        .arg(fix.root())
        .assert()
        .success()
        .stdout(predicate::str::contains("Index status:"));
}

#[test]
fn test_index_watch_foreground_scan_completes() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn main() { println!(\"watch\"); }");
    fix.add_file("src/lib.rs", "pub fn watch_probe() {}");
    fix.git_commit("initial");

    let output = fix
        .sf()
        .arg("index")
        .arg("watch")
        .arg("--root")
        .arg(fix.root())
        .output()
        .expect("sf index watch failed");

    assert!(
        output.status.success(),
        "index watch failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fix.db_path().exists(),
        "Database file should exist after watch"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Indexed") || stderr.contains("Index complete"),
        "watch output should include a completion summary: {stderr}"
    );

    let hits = source_fast_core::search_database_file_filtered(&fix.db_path(), "watch_probe", None)
        .expect("searching the foreground-built index should succeed");
    assert!(
        hits.iter().any(|hit| hit.path.contains("lib.rs")),
        "foreground watch should have indexed lib.rs, got: {hits:?}"
    );
}
