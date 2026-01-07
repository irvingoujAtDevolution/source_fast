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

    // Index once
    fix.index();

    // Add more files
    for i in 0..10 {
        fix.add_file(&format!("src/file_{}.rs", i), &format!("fn func_{}() {{}}", i));
    }
    fix.git_commit("add more files");

    // Re-index should work correctly
    fix.index();

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
    fix.index();

    // Simulate rewrite by amending (changes commit hash)
    fix.add_file("src/main.rs", "fn after_rewrite_r2() {}");
    fix.git(&["add", "."]);
    fix.git(&["commit", "--amend", "-m", "amended initial"]);

    // Re-index should handle the changed hash
    fix.index();

    // Should find the new content
    let output = fix.search("after_rewrite_r2");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find amended content: {}",
        stdout
    );
}

/// R3: Locked DB (Concurrent Access)
/// Start server, then try CLI index.
/// Expected: Should handle gracefully.
#[test]
fn test_r3_concurrent_access() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn main() {}");
    fix.index();

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

    // Give server time to start and lock DB
    std::thread::sleep(Duration::from_millis(500));

    // Try to run CLI index - may succeed (WAL mode allows concurrent readers)
    // or may error gracefully
    let result = fix
        .sf()
        .arg("index")
        .arg("--root")
        .arg(fix.root())
        .output();

    // Clean up server
    let _ = server.kill();
    let _ = server.wait();

    // The test passes if either:
    // 1. The command succeeded (WAL mode allows this)
    // 2. The command failed gracefully (no panic)
    match result {
        Ok(output) => {
            // Either success or graceful error
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success() || stderr.contains("locked") || stderr.contains("busy"),
                "Should either succeed or fail gracefully: {}",
                stderr
            );
        }
        Err(_) => {
            // Command failed to run, which is also acceptable
        }
    }
}

/// R4: Corrupt DB Recovery
/// Delete the database file.
/// Expected: Should transparently recreate and rebuild.
#[test]
fn test_r4_corrupt_db_recovery() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn recoverable_content_r4() {}");

    // Initial index
    fix.index();

    // Verify search works
    let output = fix.search("recoverable_content_r4");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find content initially: {}",
        stdout
    );

    // Delete the database
    let db_path = fix.db_path();
    if db_path.exists() {
        std::fs::remove_file(&db_path).expect("Failed to delete DB");
    }

    // Also remove any WAL/shm files
    let wal_path = db_path.with_extension("db-wal");
    let shm_path = db_path.with_extension("db-shm");
    let _ = std::fs::remove_file(wal_path);
    let _ = std::fs::remove_file(shm_path);

    // Re-index should recreate the database
    fix.index();

    // Search should work again
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

    // Index creates the directory
    fix.index();

    // Remove entire .source_fast directory
    let sf_dir = fix.root().join(".source_fast");
    if sf_dir.exists() {
        std::fs::remove_dir_all(&sf_dir).expect("Failed to remove .source_fast");
    }

    // Re-index should recreate everything
    fix.index();

    let output = fix.search("test_missing_dir");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find content: {}",
        stdout
    );
}

/// Additional: Index with no files
#[test]
fn test_index_empty_directory() {
    let fix = TestFixture::new();

    // Index empty directory should not crash
    fix.index();
}

/// Additional: Index directory with only ignored files
#[test]
fn test_index_only_ignored() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.git_ignore("*.ignored");
    fix.add_file("test.ignored", "ignored content");
    fix.git_commit("initial");

    // Should not crash
    fix.index();
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

    fix.index();

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
    fix.index();

    // Rapid changes with git commits
    for i in 0..5 {
        fix.add_file("main.rs", &format!("fn rapid_change_{}() {{}}", i));
        fix.git_commit(&format!("change {}", i));
        fix.index();
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
