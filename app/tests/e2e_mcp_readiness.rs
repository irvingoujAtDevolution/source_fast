//! Phase 6: MCP readiness / always-return tests
//!
//! These tests define the desired behavior for MCP search when the index is still
//! building: return best-effort results (possibly empty) plus a clear warning,
//! instead of returning an error.

mod common;

use common::TestFixture;
use common::mcp::McpServerProcess;
use std::time::{Duration, Instant};

fn response_has_error(resp: &serde_json::Value) -> bool {
    resp.get("error").is_some()
}

fn response_text_blob(resp: &serde_json::Value) -> String {
    let mut out = String::new();
    let Some(contents) = resp
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
    else {
        return out;
    };

    for item in contents {
        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
            out.push_str(text);
            out.push('\n');
        }
    }
    out
}

/// MCP readiness: search_code should not hard-fail while initial indexing is running.
#[test]
fn test_mcp_search_code_returns_warning_not_error_while_building() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn readiness_target() {}\n");

    // Make initial indexing take noticeable time (best-effort; should still be safe if fast).
    for i in 0..2000 {
        fix.add_file(
            &format!("src/gen_{i}.rs"),
            &format!("pub fn filler_{i}() {{}}\n"),
        );
    }

    let mut server = McpServerProcess::spawn(&fix.root());
    let _init = server.initialize();

    let resp = server.call_search_code(2, "readiness_target", None);

    // Desired behavior (new mechanism): never return JSON-RPC error for "index building".
    assert!(
        !response_has_error(&resp),
        "Expected no JSON-RPC error while building, got: {resp}"
    );

    let text = response_text_blob(&resp);
    assert!(
        text.to_lowercase().contains("index")
            && (text.to_lowercase().contains("building")
                || text.to_lowercase().contains("updating")
                || text.to_lowercase().contains("stale")),
        "Expected a clear readiness warning in response content, got: {text}"
    );
}

/// MCP readiness: once indexing is complete, the warning should disappear.
#[test]
fn test_mcp_search_code_eventually_no_warning() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn readiness_done_target() {}\n");

    let mut server = McpServerProcess::spawn(&fix.root());
    let _init = server.initialize();

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = None;

    let mut id = 10u64;
    while Instant::now() < deadline {
        let resp = server.call_search_code(id, "readiness_done_target", None);
        id += 1;
        last = Some(resp);

        if let Some(resp) = &last {
            if response_has_error(resp) {
                continue;
            }
            let text = response_text_blob(resp).to_lowercase();
            let has_warning = text.contains("building") || text.contains("stale");
            if !has_warning {
                return;
            }
        }

        std::thread::sleep(Duration::from_millis(200));
    }

    panic!("Expected readiness warning to disappear; last response: {last:?}");
}

