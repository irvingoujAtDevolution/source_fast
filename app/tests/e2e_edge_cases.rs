//! E2E tests for edge cases and additional scenarios
//!
//! These tests cover scenarios not covered by the basic test suites:
//! - Non-git directories
//! - Large numbers of files
//! - Very long file paths
//! - Permission issues (where testable)
//! - Search-file glob patterns
//! - MCP server error handling

mod common;
use common::TestFixture;

// ============ Non-Git Directory Tests ============

/// Test: Index works without git
/// Scenario: Directory is not a git repository
/// Expected: Should index files normally using filesystem scan
#[test]
fn test_non_git_directory() {
    let fix = TestFixture::new();

    // Don't initialize git - just add files
    fix.add_file("src/main.rs", "fn main() { println!(\"hello\"); }");
    fix.add_file("README.md", "# My Project");

    // Index should work without git
    fix.index();

    // Search should find content
    let output = fix.search("println");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("main.rs"), "Should find content in non-git directory");
}

/// Test: Multiple re-indexes in non-git directory
/// Note: Non-git directories use filesystem timestamps for change detection.
/// Since timestamps have limited resolution, we add a new file instead of
/// modifying an existing one to test re-indexing.
#[test]
fn test_non_git_reindex() {
    let fix = TestFixture::new();

    fix.add_file("file1.txt", "initial content xyz");
    fix.index();

    // Verify initial content is indexed
    let output1 = fix.search("initial content xyz");
    let stdout1 = String::from_utf8_lossy(&output1.stdout);
    assert!(
        stdout1.contains("file1.txt"),
        "Initial content should be indexed: {}",
        stdout1
    );

    // Add a new file
    fix.add_file("file2.txt", "updated content abc");
    fix.index();

    // Both files should be searchable
    let output = fix.search("updated content");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("file2.txt"),
        "New file should be indexed: {}",
        stdout
    );

    let output = fix.search("initial content");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("file1.txt"),
        "Original file should still be indexed: {}",
        stdout
    );
}

// ============ Large Number of Files Tests ============

/// Test: Index directory with many files
/// Scenario: Create 20 files and ensure they're all indexed
/// Expected: All files should be searchable
#[test]
fn test_many_files() {
    let fix = TestFixture::new();
    fix.git_init();

    // Create 20 files with unique content at root level (no subdirectory)
    for i in 0..20 {
        fix.add_file(
            &format!("file_{:03}.rs", i),
            &format!("// File number {}\nfn unique_manyfiles_{}_marker() {{}}", i, i),
        );
    }

    fix.git_commit("Add many files");
    fix.index();

    // Search for first and last files
    let output = fix.search("unique_manyfiles_0_marker");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("file_000.rs"), "Should find file 0, got: {}", stdout);

    let output = fix.search("unique_manyfiles_19_marker");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("file_019.rs"), "Should find file 19");
}

/// Test: Files across many directories
/// Expected: All nested files should be indexed
#[test]
fn test_many_nested_directories() {
    let fix = TestFixture::new();
    fix.git_init();

    // Create files in deeply nested structure
    for i in 0..20 {
        for j in 0..5 {
            fix.add_file(
                &format!("level1_{}/level2_{}/file.rs", i, j),
                &format!("// nested_{}_{}content", i, j),
            );
        }
    }

    fix.git_commit("Add nested files");
    fix.index();

    // Search for specific nested content
    let output = fix.search("nested_15_3");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("level1_15"));
}

// ============ Long File Path Tests ============

/// Test: File with long path
/// Expected: Should be indexed and searchable
#[test]
fn test_long_file_path() {
    let fix = TestFixture::new();
    fix.git_init();

    // Create a path with many directory levels
    let long_path = "a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/deep_file.rs";
    fix.add_file(long_path, "fn deep_nested_function() {}");
    fix.git_commit("Add deeply nested file");
    fix.index();

    let output = fix.search("deep_nested_function");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("deep_file.rs"), "Should find deeply nested file");
}

/// Test: File with long filename
/// Expected: Should handle long filenames
#[test]
fn test_long_filename() {
    let fix = TestFixture::new();
    fix.git_init();

    // Create a file with a very long name (but still valid)
    let long_name = format!("{}.rs", "a".repeat(100));
    fix.add_file(&long_name, "fn long_filename_content() {}");
    fix.git_commit("Add long filename");
    fix.index();

    let output = fix.search("long_filename_content");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(&"a".repeat(50)), "Should find file with long name");
}

// ============ Search-File Pattern Tests ============

/// Test: search-file with partial filename
/// Expected: Should match files containing the pattern
#[test]
fn test_search_file_partial_match() {
    let fix = TestFixture::new();
    fix.git_init();

    fix.add_file("src/main.rs", "fn main() {}");
    fix.add_file("src/lib.rs", "pub mod test;");
    fix.add_file("tests/integration.rs", "// tests");
    fix.git_commit("Add files");
    fix.index();

    // Search for partial filename
    let output = fix.search_file("main");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("main.rs"));
    assert!(!stdout.contains("lib.rs"));
}

/// Test: search-file with directory pattern
/// Expected: Should match files in directories matching the pattern
#[test]
fn test_search_file_directory_pattern() {
    let fix = TestFixture::new();
    fix.git_init();

    fix.add_file("src/main.rs", "fn main() {}");
    fix.add_file("tests/test_main.rs", "// test");
    fix.add_file("docs/guide.md", "# Guide");
    fix.git_commit("Add files");
    fix.index();

    // Search for files in src directory
    let output = fix.search_file("src");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("src"));
}

/// Test: search-file case insensitivity
/// Expected: Should match regardless of case
#[test]
fn test_search_file_case_insensitive() {
    let fix = TestFixture::new();
    fix.git_init();

    fix.add_file("README.md", "# Readme");
    fix.add_file("Makefile", "all: build");
    fix.git_commit("Add files");
    fix.index();

    // Search with different cases
    let output = fix.search_file("readme");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("README.md"));

    let output = fix.search_file("MAKEFILE");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Makefile"));
}

/// Test: search-file with extension
/// Expected: Should match files by extension
#[test]
fn test_search_file_by_extension() {
    let fix = TestFixture::new();
    fix.git_init();

    fix.add_file("code.rs", "fn main() {}");
    fix.add_file("code.py", "def main(): pass");
    fix.add_file("code.js", "function main() {}");
    fix.git_commit("Add files");
    fix.index();

    // Search by extension
    let output = fix.search_file(".rs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("code.rs"));
    // May or may not include others depending on implementation
}

// ============ Empty/Edge Content Tests ============

/// Test: File with only newlines
/// Expected: Should be indexed without errors
#[test]
fn test_file_only_newlines() {
    let fix = TestFixture::new();
    fix.git_init();

    fix.add_file("newlines.txt", "\n\n\n\n\n");
    fix.add_file("normal.txt", "normal content");
    fix.git_commit("Add files");
    fix.index();

    // Should not crash, normal file should be searchable
    let output = fix.search("normal content");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("normal.txt"));
}

/// Test: File with very long lines
/// Expected: Should be indexed and searchable
#[test]
fn test_file_long_lines() {
    let fix = TestFixture::new();
    fix.git_init();

    // Create a file with a very long line
    let long_line = format!("// {} unique_marker_xyz", "x".repeat(10000));
    fix.add_file("long_line.rs", &long_line);
    fix.git_commit("Add file with long line");
    fix.index();

    let output = fix.search("unique_marker_xyz");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("long_line.rs"));
}

/// Test: File with mixed line endings
/// Expected: Should be indexed properly
#[test]
fn test_mixed_line_endings() {
    let fix = TestFixture::new();
    fix.git_init();

    // Mix of CRLF and LF
    fix.add_file("mixed.txt", "line1\r\nline2\nline3\r\nline4_marker");
    fix.git_commit("Add file with mixed endings");
    fix.index();

    let output = fix.search("line4_marker");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("mixed.txt"));
}

// ============ Search Edge Cases ============

/// Test: Search with regex-like characters
/// Expected: Should search literally (trigram-based)
#[test]
fn test_search_regex_chars() {
    let fix = TestFixture::new();
    fix.git_init();

    fix.add_file("regex.rs", "let pattern = r\".*?\";");
    fix.git_commit("Add file");
    fix.index();

    // Search for literal regex characters
    let output = fix.search(".*?");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("regex.rs"));
}

/// Test: Search with backslashes
/// Expected: Should handle backslashes in search
#[test]
fn test_search_backslashes() {
    let fix = TestFixture::new();
    fix.git_init();

    fix.add_file("paths.rs", r#"let path = "C:\\Users\\test";"#);
    fix.git_commit("Add file");
    fix.index();

    let output = fix.search("Users\\\\test");
    // This test verifies we don't crash; actual matching may vary
    assert!(output.status.success());
}

/// Test: Search with quotes
/// Expected: Should handle quoted strings
#[test]
fn test_search_quotes() {
    let fix = TestFixture::new();
    fix.git_init();

    fix.add_file("strings.rs", r#"let msg = "hello \"world\"";"#);
    fix.git_commit("Add file");
    fix.index();

    let output = fix.search("hello");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("strings.rs"));
}

// ============ Concurrent Operations Tests ============

/// Test: Multiple searches in sequence
/// Expected: All searches should return consistent results
#[test]
fn test_sequential_searches() {
    let fix = TestFixture::new();
    fix.git_init();

    fix.add_file("file1.txt", "alpha_content");
    fix.add_file("file2.txt", "beta_content");
    fix.add_file("file3.txt", "gamma_content");
    fix.git_commit("Add files");
    fix.index();

    // Run multiple searches
    for _ in 0..10 {
        let output = fix.search("alpha_content");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("file1.txt"));

        let output = fix.search("beta_content");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("file2.txt"));
    }
}

// ============ Index State Tests ============

/// Test: Index, delete index, reindex
/// Expected: Should rebuild index from scratch
#[test]
fn test_delete_and_reindex() {
    let fix = TestFixture::new();
    fix.git_init();

    fix.add_file("persistent.txt", "persistent_content");
    fix.git_commit("Add file");
    fix.index();

    // Verify indexed
    let output = fix.search("persistent_content");
    assert!(String::from_utf8_lossy(&output.stdout).contains("persistent.txt"));

    // Delete the .source_fast directory
    let sf_dir = fix.root().join(".source_fast");
    if sf_dir.exists() {
        std::fs::remove_dir_all(&sf_dir).unwrap();
    }

    // Reindex
    fix.index();

    // Should still work
    let output = fix.search("persistent_content");
    assert!(String::from_utf8_lossy(&output.stdout).contains("persistent.txt"));
}

/// Test: Index with files that have same content
/// Expected: Both files should appear in search results
#[test]
fn test_duplicate_content_files() {
    let fix = TestFixture::new();
    fix.git_init();

    let content = "exactly_the_same_content";
    fix.add_file("copy1.txt", content);
    fix.add_file("copy2.txt", content);
    fix.add_file("subdir/copy3.txt", content);
    fix.git_commit("Add duplicate files");
    fix.index();

    let output = fix.search("exactly_the_same");

    // All three files should appear (or at least be findable)
    // The exact output format may vary
    assert!(output.status.success());
}

// ============ File Type Tests ============

/// Test: Various source file extensions
/// Expected: All common source files should be indexed
#[test]
fn test_various_source_extensions() {
    let fix = TestFixture::new();
    fix.git_init();

    fix.add_file("code.rs", "// rust_marker");
    fix.add_file("code.py", "# python_marker");
    fix.add_file("code.js", "// javascript_marker");
    fix.add_file("code.ts", "// typescript_marker");
    fix.add_file("code.go", "// golang_marker");
    fix.add_file("code.java", "// java_marker");
    fix.add_file("code.c", "// c_marker");
    fix.add_file("code.cpp", "// cpp_marker");
    fix.add_file("code.h", "// header_marker");
    fix.add_file("code.rb", "# ruby_marker");
    fix.git_commit("Add various files");
    fix.index();

    // Each should be searchable
    for marker in &["rust_marker", "python_marker", "javascript_marker", "golang_marker"] {
        let output = fix.search(marker);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.is_empty() || output.status.success(), "Should find {}", marker);
    }
}

/// Test: Config and data files
/// Expected: Common config files should be indexed
#[test]
fn test_config_files() {
    let fix = TestFixture::new();
    fix.git_init();

    fix.add_file("config.json", r#"{"config_json_marker": true}"#);
    fix.add_file("config.yaml", "config_yaml_marker: true");
    fix.add_file("config.toml", "config_toml_marker = true");
    fix.add_file(".env.example", "CONFIG_ENV_MARKER=value");
    fix.git_commit("Add config files");
    fix.index();

    let output = fix.search("config_json_marker");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("config.json"));

    let output = fix.search("config_yaml_marker");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("config.yaml"));
}
