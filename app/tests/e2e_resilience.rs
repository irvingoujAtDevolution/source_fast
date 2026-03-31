//! Phase 4: Resilience & Recovery Tests (R1-R4)
//!
//! These tests verify transaction safety and error recovery.

mod common;

use common::TestFixture;
use std::process::{Command, Stdio};
use std::time::Duration;

/// R1: Interruption Recovery
/// Note: Actual Ctrl+C interruption is hard to test reliably.
/// This test verifies that re-indexing after partial state works.
#[test]
fn test_r1_reindex_after_partial_state() {
    let fix = TestFixture::new();
    fix.git_init();

    // Create initial state
    fix.add_file("src/main.rs", "fn main() {}");
    fix.add_file("src/lib.rs", "fn lib() {}");
    fix.git_commit("initial");

    // Index once via search
    let _ = fix.search("main");

    // Add more files
    for i in 0..10 {
        fix.add_file(&format!("src/file_{}.rs", i), &format!("fn func_{}() {{}}", i));
    }
    fix.git_commit("add more files");

    // Stop daemon so next search re-scans
    fix.stop();

    // Verify search works
    let output = fix.search("func_5");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("file_5.rs"),
        "Should find file_5.rs: {}",
        stdout
    );
}

/// R2: History Rewrite (Simulated)
/// When git history is rewritten, stored hash may be invalid.
/// Expected: Tool detects and does a full re-scan.
#[test]
fn test_r2_history_rewrite() {
    let fix = TestFixture::new();
    fix.git_init();

    fix.add_file("src/main.rs", "fn before_rewrite() {}");
    fix.git_commit("initial");
    let _ = fix.search("before_rewrite");

    // Simulate rewrite by amending (changes commit hash)
    fix.add_file("src/main.rs", "fn after_rewrite_r2() {}");
    fix.git(&["add", "."]);
    fix.git(&["commit", "--amend", "-m", "amended initial"]);

    // Stop daemon so next search re-scans
    fix.stop();

    // Should find the new content
    let output = fix.search("after_rewrite_r2");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find amended content: {}",
        stdout
    );
}

/// R3: Concurrent Access
/// Start MCP server, then try CLI search.
/// Expected: Should handle gracefully (leader election prevents conflicts).
#[test]
fn test_r3_concurrent_access() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn main() {}");

    // First search to create DB and index
    let _ = fix.search("main");

    // Start server in background
    let mut server = Command::new(env!("CARGO_BIN_EXE_sf"))
        .arg("server")
        .arg("--root")
        .arg(fix.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to start server");

    // Give server time to start and acquire lease
    std::thread::sleep(Duration::from_millis(500));

    // Try to run CLI search - should work (daemon or server holds the lease,
    // search is read-only from the CLI's perspective)
    let output = fix.search("main");

    // Clean up server
    let _ = server.kill();
    let _ = server.wait();

    // The test passes if search didn't crash
    assert!(output.status.success(), "Search should succeed with concurrent server");
}

/// R4: Corrupt DB Recovery
/// Delete the database file.
/// Expected: Should transparently recreate and rebuild.
#[test]
fn test_r4_corrupt_db_recovery() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn recoverable_content_r4() {}");

    // Initial search triggers indexing
    let output = fix.search("recoverable_content_r4");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find content initially: {}",
        stdout
    );

    // Stop daemon before deleting DB
    fix.stop();

    // Delete the database (retry on Windows file locks).
    let db_path = fix.db_path();
    for attempt in 0..10 {
        if !db_path.exists() {
            break;
        }
        match std::fs::remove_file(&db_path) {
            Ok(()) => break,
            Err(_) if attempt < 9 => {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(e) => panic!("Failed to delete DB after retries: {e}"),
        }
    }

    // Search should recreate the database
    let output = fix.search("recoverable_content_r4");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find content after recovery: {}",
        stdout
    );
}

/// Additional: Missing .source_fast directory
#[test]
fn test_missing_source_fast_dir() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn test_missing_dir() {}");

    // Search creates the directory and indexes
    let _ = fix.search("test_missing_dir");

    // Stop daemon
    fix.stop();

    // Remove entire .source_fast directory (retry on Windows file locks).
    let sf_dir = fix.root().join(".source_fast");
    for attempt in 0..10 {
        if !sf_dir.exists() {
            break;
        }
        match std::fs::remove_dir_all(&sf_dir) {
            Ok(()) => break,
            Err(_) if attempt < 9 => {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(e) => panic!("Failed to remove .source_fast after retries: {e}"),
        }
    }

    // Search should recreate everything
    let output = fix.search("test_missing_dir");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find content: {}",
        stdout
    );
}

/// Additional: Search with no files (empty directory)
#[test]
fn test_search_empty_directory() {
    let fix = TestFixture::new();

    // Search empty directory should not crash
    let output = fix.search("nonexistent_query");
    assert!(output.status.success(), "Search on empty directory should not crash");
}

/// Additional: Search directory with only ignored files
#[test]
fn test_search_only_ignored() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.git_ignore("*.ignored");
    fix.add_file("test.ignored", "ignored content");
    fix.git_commit("initial");

    // Should not crash
    let output = fix.search("ignored content");
    assert!(output.status.success(), "Search with only ignored files should not crash");
}

/// Additional: Very large file handling
#[test]
fn test_large_file() {
    let fix = TestFixture::new();

    // Create a moderately large file (not too large to slow tests)
    let large_content: String = (0..1000)
        .map(|i| format!("fn function_{}() {{ /* line {} */ }}\n", i, i))
        .collect();

    fix.add_file("src/large.rs", &large_content);
    fix.add_file("src/small.rs", "fn small_marker() {}");

    // Should find content in both files
    let output = fix.search("function_500");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("large.rs"),
        "Should find large.rs: {}",
        stdout
    );

    let output = fix.search("small_marker");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("small.rs"),
        "Should find small.rs: {}",
        stdout
    );
}

/// Additional: Rapid file changes
/// Uses git to track changes properly since filesystem timestamps
/// may not have enough resolution for rapid changes.
#[test]
fn test_rapid_changes() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("main.rs", "fn initial() {}");
    fix.git_commit("initial");
    let _ = fix.search("initial");

    // Rapid changes with git commits
    for i in 0..5 {
        fix.add_file("main.rs", &format!("fn rapid_change_{}() {{}}", i));
        fix.git_commit(&format!("change {}", i));
        fix.stop();
        let _ = fix.search(&format!("rapid_change_{}", i));
    }

    // Should have the latest content
    let output = fix.search("rapid_change_4");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find latest content: {}",
        stdout
    );

    // Should not have old content
    let output = fix.search("rapid_change_0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("main.rs") || stdout.contains("No results"),
        "Old content should not be found"
    );
}

/// Signal-file shutdown: writing .shutdown_requested causes daemon to exit.
#[test]
fn test_signal_file_shutdown() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn signal_test() {}");
    fix.git_commit("initial");

    // Start daemon via search
    let _ = fix.search("signal_test");

    // Verify daemon is running
    let status_output = fix.status();
    let status_stdout = String::from_utf8_lossy(&status_output.stdout);
    assert!(
        status_stdout.contains("Leader:") && !status_stdout.contains("none"),
        "Daemon should be running: {}",
        status_stdout
    );

    // Write shutdown signal file directly
    let shutdown_file = fix.root().join(".source_fast").join(".shutdown_requested");
    std::fs::write(&shutdown_file, "true").expect("Failed to write shutdown signal");

    // Poll until daemon stops (up to 10s)
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut stopped = false;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        if let Ok(active) = source_fast_core::is_leader_active_readonly(&fix.db_path()) {
            if !active {
                stopped = true;
                break;
            }
        }
    }
    assert!(stopped, "Daemon should stop after signal file is written");
}

/// Multi-process LMDB: CLI can read while daemon writes.
#[test]
fn test_concurrent_cli_reads_while_daemon_runs() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn concurrent_test_marker() {}");
    fix.add_file("src/lib.rs", "fn lib_marker() {}");
    fix.git_commit("initial");

    // Start daemon and wait for index to complete
    let output = fix.search("concurrent_test_marker");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("main.rs"), "Initial search should work");

    // While daemon is still running, do multiple CLI operations
    let status_output = fix.status();
    assert!(
        status_output.status.success(),
        "sf status should succeed while daemon runs"
    );
    let status_stdout = String::from_utf8_lossy(&status_output.stdout);
    assert!(
        status_stdout.contains("complete"),
        "Index should be complete: {}",
        status_stdout
    );

    let search_output = fix.search("lib_marker");
    assert!(
        search_output.status.success(),
        "sf search should succeed while daemon runs"
    );
    let search_stdout = String::from_utf8_lossy(&search_output.stdout);
    assert!(
        search_stdout.contains("lib.rs"),
        "Should find lib.rs: {}",
        search_stdout
    );

    let file_output = fix.search_file("main");
    assert!(
        file_output.status.success(),
        "sf search-file should succeed while daemon runs"
    );
    let file_stdout = String::from_utf8_lossy(&file_output.stdout);
    assert!(
        file_stdout.contains("main.rs"),
        "Should find main.rs: {}",
        file_stdout
    );
}
