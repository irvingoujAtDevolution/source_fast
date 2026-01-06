//! Phase 3: File System Edge Cases (F1-F5)
//!
//! These tests verify file operations and binary file handling.

mod common;

use common::TestFixture;

/// F1: Deletion
/// Delete a file and re-index.
/// Expected: Search for unique string in deleted file returns 0 results.
/// Note: Deletion tracking requires git - the tool is designed for git repos.
/// Fixed: normalize_path() now handles deleted files by canonicalizing parent directory.
#[test]
fn test_f1_deletion() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/main.rs", "fn main() {}");
    fix.add_file("src/to_delete.rs", "fn unique_deletable_function() {}");
    fix.git_commit("initial");

    fix.index();

    // Verify it's indexed
    let output = fix.search("unique_deletable_function");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("to_delete.rs"),
        "Should find to_delete.rs initially: {}",
        stdout
    );

    // Delete the file and commit
    fix.remove_file("src/to_delete.rs");
    fix.git_commit("delete file");

    // Re-index
    fix.index();

    // Should no longer find the deleted content
    let output = fix.search("unique_deletable_function");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("to_delete.rs"),
        "Deleted file should not appear in results: {}",
        stdout
    );
}

/// F2: Rename
/// Rename a file and re-index.
/// Expected: Search returns new name, not old name.
/// Note: Rename tracking requires git - the tool is designed for git repos.
/// Fixed: normalize_path() now handles deleted files by canonicalizing parent directory.
#[test]
fn test_f2_rename() {
    let fix = TestFixture::new();
    fix.git_init();
    fix.add_file("src/old_name.rs", "fn renamed_function_content() {}");
    fix.git_commit("initial");

    fix.index();

    // Verify old name is found
    let output = fix.search("renamed_function_content");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("old_name.rs"),
        "Should find old_name.rs initially: {}",
        stdout
    );

    // Rename the file (git mv)
    fix.git(&["mv", "src/old_name.rs", "src/new_name.rs"]);
    fix.git_commit("rename file");

    // Re-index
    fix.index();

    // Should find new name
    let output = fix.search("renamed_function_content");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("new_name.rs"),
        "New filename should appear in results: {}",
        stdout
    );
    assert!(
        !stdout.contains("old_name.rs"),
        "Old filename should not appear in results: {}",
        stdout
    );
}

/// F3: Binary Bomb
/// Create a binary file (e.g., PNG-like bytes).
/// Expected: Should be skipped, not crash, DB size should not explode.
#[test]
fn test_f3_binary_file() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn main() {}");

    // Create a fake binary file with null bytes (PNG-like header)
    let binary_content: Vec<u8> = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG header
        0x00, 0x00, 0x00, 0x0D, // Chunk length
        0x49, 0x48, 0x44, 0x52, // IHDR
        0x00, 0x00, 0x01, 0x00, // Width
        0x00, 0x00, 0x01, 0x00, // Height
        0x08, 0x06, 0x00, 0x00, 0x00, // Bit depth, color type, etc.
    ];
    fix.add_binary("assets/icon.png", &binary_content);

    // Should not crash during indexing
    fix.index();

    // Search should work for text files
    let output = fix.search("main");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find main.rs: {}",
        stdout
    );
}

/// F4: Null Byte Injection
/// Create a text file with null bytes in the middle.
/// Expected: Should be treated as binary and skipped.
#[test]
fn test_f4_null_byte_injection() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn main() {}");

    // Create a file that looks like text but has null bytes
    let content_with_null: Vec<u8> = b"fn looks_like_text() {\n    \x00\x00\x00\n    // hidden\n}"
        .to_vec();
    fix.add_binary("src/sneaky.rs", &content_with_null);

    // Should not crash
    fix.index();

    // The file with null bytes should be skipped (treated as binary)
    let output = fix.search("looks_like_text");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // It should NOT find content from the binary-ish file
    assert!(
        !stdout.contains("sneaky.rs"),
        "File with null bytes should be treated as binary: {}",
        stdout
    );
}

/// F5: Empty File
/// Create an empty file.
/// Expected: Should not crash, file is indexed (with no trigrams).
#[test]
fn test_f5_empty_file() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn main() {}");
    fix.add_file("src/empty.rs", ""); // Empty file

    // Should not crash
    fix.index();

    // Search should still work
    let output = fix.search("main");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find main.rs: {}",
        stdout
    );
}

/// Additional: Very small file (less than 3 chars - can't make trigrams)
#[test]
fn test_tiny_file() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn main() {}");
    fix.add_file("src/tiny.rs", "x"); // Only 1 char

    // Should not crash
    fix.index();
}

/// Additional: File with only whitespace
#[test]
fn test_whitespace_only_file() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn main() {}");
    fix.add_file("src/spaces.rs", "   \n\n\t\t  \n");

    // Should not crash
    fix.index();
}

/// Additional: Unicode content
#[test]
fn test_unicode_content() {
    let fix = TestFixture::new();
    fix.add_file(
        "src/unicode.rs",
        "fn greet() { println!(\"你好世界 🌍 مرحبا\"); }",
    );

    fix.index();

    // Should be able to search for unicode
    let output = fix.search("你好世界");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("unicode.rs"),
        "Should find unicode.rs: {}",
        stdout
    );
}

/// Additional: Deeply nested file
#[test]
fn test_deeply_nested_file() {
    let fix = TestFixture::new();
    fix.add_file(
        "src/a/b/c/d/e/deep.rs",
        "fn deeply_nested_unique_function() {}",
    );

    fix.index();

    let output = fix.search("deeply_nested_unique");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("deep.rs"),
        "Should find deep.rs: {}",
        stdout
    );
}
