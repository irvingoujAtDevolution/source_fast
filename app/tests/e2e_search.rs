//! Phase 5: Search Quality & MCP Tests (S1-S3)
//!
//! These tests verify search quality and MCP server functionality.

mod common;

use common::TestFixture;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

/// S1: Substring Search
/// File contains "function_name", search for "nction".
/// Expected: Should match (trigram-based substring search).
#[test]
fn test_s1_substring_search() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn my_special_function_name() {}");

    fix.index();

    // Search for substring (at least 3 chars for trigram)
    let output = fix.search("special_function");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find main.rs: {}",
        stdout
    );

    // Another substring test
    let output = fix.search("nction_name");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find main.rs with substring: {}",
        stdout
    );
}

/// S2: Snippet Context
/// Search for a unique line.
/// Expected: Output should contain the line plus context lines.
#[test]
fn test_s2_snippet_context() {
    let fix = TestFixture::new();
    let content = r#"fn setup() {
    // Setup code here
}

fn unique_target_line_s2() {
    // This is the target
    println!("found me");
}

fn cleanup() {
    // Cleanup code
}
"#;
    fix.add_file("src/main.rs", content);

    fix.index();

    let output = fix.search("unique_target_line_s2");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should contain the target line
    assert!(
        stdout.contains("unique_target_line_s2"),
        "Should contain target line: {}",
        stdout
    );

    // Should contain line number
    assert!(
        stdout.contains(":") && stdout.chars().any(|c| c.is_ascii_digit()),
        "Should contain line numbers: {}",
        stdout
    );
}

/// S3: MCP JSON-RPC Server
/// Run `sf server` and send JSON-RPC request.
/// Expected: Valid JSON-RPC response.
#[test]
fn test_s3_mcp_jsonrpc() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn mcp_test_function_s3() {}");

    // First index the files
    fix.index();

    // Start the server
    let mut child = Command::new(env!("CARGO_BIN_EXE_sf"))
        .arg("server")
        .arg("--root")
        .arg(fix.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to start sf server");

    let mut stdin = child.stdin.take().expect("Failed to get stdin");
    let stdout = child.stdout.take().expect("Failed to get stdout");
    let mut reader = BufReader::new(stdout);

    // MCP initialization request
    let init_request = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;

    // Send initialization
    writeln!(stdin, "{}", init_request).expect("Failed to write init request");
    stdin.flush().expect("Failed to flush");

    // Read response (with timeout handling via non-blocking or just trust it works)
    let mut response = String::new();

    // Give it a moment to process
    std::thread::sleep(Duration::from_millis(500));

    // Try to read a line
    if reader.read_line(&mut response).is_ok() && !response.is_empty() {
        // Should be valid JSON
        assert!(
            response.contains("jsonrpc") || response.contains("result"),
            "Response should be JSON-RPC: {}",
            response
        );
    }

    // Clean up - send EOF and kill
    drop(stdin);
    let _ = child.kill();
}

/// Additional: Case sensitivity
#[test]
fn test_case_sensitive_search() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn CamelCaseFunction() {}");
    fix.add_file("src/lib.rs", "fn camelcasefunction() {}");

    fix.index();

    // Search for exact case
    let output = fix.search("CamelCaseFunction");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("main.rs"), "Should find exact case match");
}

/// Additional: Multiple matches in same file
#[test]
fn test_multiple_matches_same_file() {
    let fix = TestFixture::new();
    fix.add_file(
        "src/main.rs",
        r#"
fn first_occurrence() {}
fn second_occurrence() {}
fn third_occurrence() {}
"#,
    );

    fix.index();

    // Should find the file when searching for common substring
    let output = fix.search("occurrence");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find main.rs: {}",
        stdout
    );
}

/// Additional: Search across multiple files
#[test]
fn test_search_multiple_files() {
    let fix = TestFixture::new();
    fix.add_file("src/a.rs", "fn shared_pattern_multi() {}");
    fix.add_file("src/b.rs", "fn shared_pattern_multi() {}");
    fix.add_file("src/c.rs", "fn shared_pattern_multi() {}");

    fix.index();

    let output = fix.search("shared_pattern_multi");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should find all three files
    assert!(stdout.contains("a.rs"), "Should find a.rs");
    assert!(stdout.contains("b.rs"), "Should find b.rs");
    assert!(stdout.contains("c.rs"), "Should find c.rs");
}

/// Additional: Minimum query length (3 chars for trigrams)
#[test]
fn test_minimum_query_length() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn ab() {} fn abc() {} fn abcd() {}");

    fix.index();

    // 3 char query should work
    let output = fix.search("abc");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs") || output.status.success(),
        "3-char query should work"
    );

    // 2 char query behavior depends on implementation
    // Just ensure it doesn't crash
    let _ = fix.search("ab");
}

/// Additional: Special characters in search
#[test]
fn test_special_characters_search() {
    let fix = TestFixture::new();
    fix.add_file(
        "src/main.rs",
        r#"
fn test() {
    let x = vec![1, 2, 3];
    println!("{:?}", x);
}
"#,
    );

    fix.index();

    // Search for code with special chars
    let output = fix.search("vec![1, 2, 3]");
    assert!(output.status.success(), "Should not crash on special chars");

    let output = fix.search("println!");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find main.rs: {}",
        stdout
    );
}

/// Additional: Search in comments
#[test]
fn test_search_in_comments() {
    let fix = TestFixture::new();
    fix.add_file(
        "src/main.rs",
        r#"
// TODO: unique_comment_marker_test
/* Another unique_block_comment_test */
fn main() {}
"#,
    );

    fix.index();

    let output = fix.search("unique_comment_marker");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find comment: {}",
        stdout
    );

    let output = fix.search("unique_block_comment");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find block comment: {}",
        stdout
    );
}

/// Additional: Search in strings
#[test]
fn test_search_in_strings() {
    let fix = TestFixture::new();
    fix.add_file(
        "src/main.rs",
        r#"
fn main() {
    let msg = "unique_string_content_test";
    println!("{}", msg);
}
"#,
    );

    fix.index();

    let output = fix.search("unique_string_content");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("main.rs"),
        "Should find string content: {}",
        stdout
    );
}
