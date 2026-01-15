//! Phase 7: Single-writer leader election tests
//!
//! These tests define the desired "single writer + reader promotion" behavior.
//! Because the repository runs tests in offline mode, these tests observe behavior
//! via `SOURCE_FAST_LOG_PATH` instead of probing OS-level file locks.

mod common;

use common::TestFixture;
use common::mcp::McpServerProcess;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn wait_for_log(path: &Path, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.contains(needle) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("Timed out waiting for log to contain '{needle}' at {}", path.display());
}

fn log_path(root: &Path, name: &str) -> PathBuf {
    root.join(name)
}

/// When a server starts, it should become writer if no writer exists.
/// When it exits, a subsequent server should be able to become writer.
#[test]
fn test_writer_lock_released_allows_next_writer() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn lock_target() {}\n");

    let log_a = log_path(&fix.root(), "server_a.log");
    let mut server_a = McpServerProcess::spawn_with_log(&fix.root(), Some(log_a.clone()));
    let _ = server_a.initialize();
    wait_for_log(&log_a, "role=writer", Duration::from_secs(5));

    server_a.kill();

    let log_b = log_path(&fix.root(), "server_b.log");
    let mut server_b = McpServerProcess::spawn_with_log(&fix.root(), Some(log_b.clone()));
    let _ = server_b.initialize();
    wait_for_log(&log_b, "role=writer", Duration::from_secs(5));
}

/// With an active writer, a second server should start as reader and later promote to writer
/// when the current writer exits.
#[test]
fn test_reader_promotes_after_writer_exit() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn promote_target() {}\n");

    let log_a = log_path(&fix.root(), "server_a.log");
    let mut server_a = McpServerProcess::spawn_with_log(&fix.root(), Some(log_a.clone()));
    let _ = server_a.initialize();
    wait_for_log(&log_a, "role=writer", Duration::from_secs(5));

    let log_b = log_path(&fix.root(), "server_b.log");
    let mut server_b = McpServerProcess::spawn_with_log(&fix.root(), Some(log_b.clone()));
    let _ = server_b.initialize();
    wait_for_log(&log_b, "role=reader", Duration::from_secs(5));

    server_a.kill();

    wait_for_log(&log_b, "promoted", Duration::from_secs(10));
    wait_for_log(&log_b, "role=writer", Duration::from_secs(10));

    let _ = server_b.call_search_code(42, "promote_target", None);
}

/// If multiple servers start concurrently, exactly one should become writer (others readers).
#[test]
fn test_only_one_writer_with_multiple_servers() {
    let fix = TestFixture::new();
    fix.add_file("src/main.rs", "fn multi_start_target() {}\n");

    let log_a = log_path(&fix.root(), "server_a.log");
    let log_b = log_path(&fix.root(), "server_b.log");
    let log_c = log_path(&fix.root(), "server_c.log");

    let mut server_a = McpServerProcess::spawn_with_log(&fix.root(), Some(log_a.clone()));
    let mut server_b = McpServerProcess::spawn_with_log(&fix.root(), Some(log_b.clone()));
    let mut server_c = McpServerProcess::spawn_with_log(&fix.root(), Some(log_c.clone()));

    let _ = server_a.initialize();
    let _ = server_b.initialize();
    let _ = server_c.initialize();

    wait_for_log(&log_a, "role=", Duration::from_secs(10));
    wait_for_log(&log_b, "role=", Duration::from_secs(10));
    wait_for_log(&log_c, "role=", Duration::from_secs(10));

    let a = std::fs::read_to_string(&log_a).unwrap_or_default().contains("role=writer");
    let b = std::fs::read_to_string(&log_b).unwrap_or_default().contains("role=writer");
    let c = std::fs::read_to_string(&log_c).unwrap_or_default().contains("role=writer");

    let writers = [a, b, c].into_iter().filter(|v| *v).count();
    assert_eq!(
        writers, 1,
        "Expected exactly one writer; got a={a}, b={b}, c={c}"
    );
}
