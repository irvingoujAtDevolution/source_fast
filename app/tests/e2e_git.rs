//! Phase 2: Git Integration Tests (G1-G6)
//!
//! These tests verify git-aware incremental indexing.

mod common;

use common::TestFixture;

/// G1: New Commit
/// Modify a file, commit, and re-search.
/// Expected: Search finds new content, old content gone.
#[test]
fn test_g1_new_commit() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn old_unique_function_g1() {}");
    fix.git_commit("initial commit");

    // Verify old content is found
    let output = fix.search("old_unique_function_g1");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find main.rs with old content: {}",
        stdout
    );

    // Modify file and commit
    fix.add_file("src/main.rs", "fn new_unique_function_g1() {}");
    fix.git_commit("update function");

    // Stop daemon so next search forces a fresh re-scan
    fix.stop();

    // New content should be found
    let output = fix.search("new_unique_function_g1");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find main.rs with new content: {}",
        stdout
    );

    // Old content should be gone
    let output = fix.search("old_unique_function_g1");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("main.rs") || stdout.contains("No results"),
        "Old content should not be found: {}",
        stdout
    );
}

/// G2: Dirty State (Modified)
/// Modify a file without committing.
/// Expected: Search finds dirty content.
#[test]
fn test_g2_dirty_state_modified() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn committed_content() {}");
    fix.git_commit("initial");

    let _ = fix.search("committed_content");

    // Modify without committing
    fix.add_file("src/main.rs", "fn dirty_uncommitted_g2() {}");

    // Stop daemon so next search re-scans
    fix.stop();

    // Should find the dirty content
    let output = fix.search("dirty_uncommitted_g2");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find dirty content: {}",
        stdout
    );
}

/// G3: Dirty State (Untracked)
/// Create a new file without `git add`.
/// Expected: Search finds the untracked file.
#[test]
fn test_g3_dirty_state_untracked() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn main() {}");
    fix.git_commit("initial");

    let _ = fix.search("main");

    // Create new untracked file
    fix.add_file("src/untracked.rs", "fn untracked_unique_g3() {}");

    // Stop daemon so next search re-scans
    fix.stop();

    // Should find the untracked file
    let output = fix.search("untracked_unique_g3");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("untracked.rs"),
        "Should find untracked file: {}",
        stdout
    );
}

/// G4: Branch Switch
/// Create branch, make changes, commit, switch back.
/// Expected: Index reflects current branch state after each search.
#[test]
fn test_g4_branch_switch() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn main_branch_content() {}");
    fix.git_commit("initial on main");

    let _ = fix.search("main_branch_content");

    // Create feature branch
    fix.git_checkout_new("feature");
    fix.add_file("src/feature.rs", "fn feature_only_g4() {}");
    fix.git_commit("feature commit");

    // Stop + search on feature branch
    fix.stop();
    let output = fix.search("feature_only_g4");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("feature.rs"),
        "Should find feature file: {}",
        stdout
    );

    // Switch back to main/master
    let result = fix.git(&["checkout", "main"]);
    if !result.status.success() {
        fix.git(&["checkout", "master"]);
    }

    // Stop + search
    fix.stop();
    let output = fix.search("feature_only_g4");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("feature.rs"),
        "Feature branch content should not be found on main: {}",
        stdout
    );
}

/// G5: Git Reset
/// Do git reset --hard HEAD~1 to remove recent work.
/// Expected: Deleted files disappear from search results.
#[test]
fn test_g5_git_reset() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn original_g5() {}");
    fix.git_commit("initial");

    let _ = fix.search("original_g5");

    // Add new file and commit
    fix.add_file("src/to_be_reset.rs", "fn will_be_reset_g5() {}");
    fix.git_commit("add file to be reset");

    fix.stop();
    let output = fix.search("will_be_reset_g5");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("to_be_reset.rs"),
        "Should find file before reset: {}",
        stdout
    );

    // Reset back
    fix.git_reset_hard("HEAD~1");

    fix.stop();
    let output = fix.search("will_be_reset_g5");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("to_be_reset.rs"),
        "Reset file should not appear: {}",
        stdout
    );
}

/// G6: Git Ignore
/// Create a file and add it to .gitignore.
/// Expected: Ignored file should NOT be indexed.
#[test]
fn test_g6_git_ignore() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn main() {}");

    // Add .gitignore first
    fix.git_ignore("secret.key");
    fix.git_ignore("*.secret");

    fix.git_commit("initial with gitignore");

    // Create ignored files
    fix.add_file("secret.key", "api_key_g6_should_not_index=12345");
    fix.add_file("config.secret", "password_g6_secret=hunter2");

    // Should NOT find ignored file content
    let output = fix.search("api_key_g6_should_not_index");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("secret.key"),
        "Gitignored file should not be indexed: {}",
        stdout
    );

    let output = fix.search("password_g6_secret");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("config.secret"),
        "Gitignored pattern should not be indexed: {}",
        stdout
    );
}

/// Additional: Multiple commits incrementally
#[test]
fn test_incremental_multiple_commits() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/v1.rs", "fn version_one() {}");
    fix.git_commit("v1");
    let _ = fix.search("version_one");

    fix.add_file("src/v2.rs", "fn version_two() {}");
    fix.git_commit("v2");
    fix.stop();
    let _ = fix.search("version_two");

    fix.add_file("src/v3.rs", "fn version_three() {}");
    fix.git_commit("v3");
    fix.stop();

    // All versions should be found
    let output = fix.search("version_one");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("v1.rs"),
        "Should find v1.rs"
    );

    let output = fix.search("version_two");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("v2.rs"),
        "Should find v2.rs"
    );

    let output = fix.search("version_three");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("v3.rs"),
        "Should find v3.rs"
    );
}
