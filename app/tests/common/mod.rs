//! Test helper module for E2E tests
//!
//! Provides `TestFixture` for easy test setup with file and git operations.

#![allow(dead_code)] // Test helpers may not be used in all test modules
#![allow(deprecated)] // cargo_bin() deprecation - the new API requires more investigation

use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::prelude::*;
use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::sync::{Mutex, MutexGuard};

/// Test fixture providing a temporary directory with helper methods
/// for file operations, git commands, and running the `sf` CLI.
pub struct TestFixture {
    pub dir: TempDir,
    _guard: MutexGuard<'static, ()>,
}

pub mod mcp;

// E2E tests spawn detached daemon processes and rely on filesystem observation.
// Running them concurrently inside the same test binary leads to sporadic
// partial-result failures on Windows. Serialize `TestFixture` users per binary.
static TEST_FIXTURE_MUTEX: Mutex<()> = Mutex::new(());

impl TestFixture {
    /// Create a new test environment with a fresh temp directory
    pub fn new() -> Self {
        let guard = TEST_FIXTURE_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        Self {
            dir: TempDir::new().unwrap(),
            _guard: guard,
        }
    }

    /// Get the root path of the test directory
    pub fn root(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    /// Get the expected db path (.source_fast/index.mdb)
    pub fn db_path(&self) -> PathBuf {
        self.root().join(".source_fast").join("index.mdb")
    }

    // ============ File Operations ============

    /// Add a source file with content (creates parent dirs automatically)
    pub fn add_file(&self, path: &str, content: &str) -> &Self {
        let file = self.dir.child(path);
        file.write_str(content).unwrap();
        self
    }

    /// Add a binary file with raw bytes
    pub fn add_binary(&self, path: &str, bytes: &[u8]) -> &Self {
        let file = self.dir.child(path);
        file.write_binary(bytes).unwrap();
        self
    }

    /// Delete a file from the test directory
    pub fn remove_file(&self, path: &str) -> &Self {
        std::fs::remove_file(self.root().join(path)).unwrap();
        self
    }

    /// Rename/move a file within the test directory
    pub fn rename_file(&self, from: &str, to: &str) -> &Self {
        let to_path = self.root().join(to);
        if let Some(parent) = to_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::rename(self.root().join(from), to_path).unwrap();
        self
    }

    /// Check if a file exists
    pub fn file_exists(&self, path: &str) -> bool {
        self.root().join(path).exists()
    }

    // ============ Git Operations ============

    /// Initialize as git repo with test user config
    pub fn git_init(&self) -> &Self {
        self.git(&["init"]);
        self.git(&["config", "user.email", "test@test.com"]);
        self.git(&["config", "user.name", "Test"]);
        self
    }

    /// Stage all files (including deletions) and create a commit
    pub fn git_commit(&self, msg: &str) -> &Self {
        self.git(&["add", "-A"]); // -A stages deletions too
        self.git(&["commit", "-m", msg, "--allow-empty"]);
        self
    }

    /// Create and checkout a new branch
    pub fn git_checkout_new(&self, branch: &str) -> &Self {
        self.git(&["checkout", "-b", branch]);
        self
    }

    /// Checkout an existing branch
    pub fn git_checkout(&self, branch: &str) -> &Self {
        self.git(&["checkout", branch]);
        self
    }

    /// Hard reset to a ref (e.g., "HEAD~1")
    pub fn git_reset_hard(&self, ref_: &str) -> &Self {
        self.git(&["reset", "--hard", ref_]);
        self
    }

    /// Add a pattern to .gitignore
    pub fn git_ignore(&self, pattern: &str) -> &Self {
        let gitignore_path = self.root().join(".gitignore");
        let mut content = std::fs::read_to_string(&gitignore_path).unwrap_or_default();
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(pattern);
        content.push('\n');
        std::fs::write(&gitignore_path, content).unwrap();
        self
    }

    /// Run arbitrary git command and return output
    pub fn git(&self, args: &[&str]) -> std::process::Output {
        StdCommand::new("git")
            .args(args)
            .current_dir(self.root())
            .output()
            .expect("git command failed")
    }

    // ============ sf CLI ============

    /// Create a Command for running `sf` - note: --root must come AFTER subcommand
    pub fn sf(&self) -> Command {
        let mut cmd = Command::cargo_bin("sf").unwrap();
        cmd.current_dir(self.root());
        cmd
    }

    /// Helper to get --root args for subcommands
    fn root_args(&self) -> [std::ffi::OsString; 2] {
        ["--root".into(), self.root().into()]
    }

    /// Run sf search with --wait (blocks until index is complete).
    /// This auto-starts the daemon if not running.
    pub fn search(&self, query: &str) -> std::process::Output {
        self.sf()
            .arg("search")
            .arg("--root")
            .arg(self.root())
            .arg("--wait")
            .arg(query)
            .output()
            .expect("sf search failed")
    }

    /// Run sf search-file with --wait (blocks until index is complete).
    pub fn search_file(&self, pattern: &str) -> std::process::Output {
        self.sf()
            .arg("search-file")
            .arg("--root")
            .arg(self.root())
            .arg("--wait")
            .arg(pattern)
            .output()
            .expect("sf search-file failed")
    }

    /// Stop the daemon for this repo. Use between test phases that modify files
    /// to force a fresh daemon re-scan on the next search.
    ///
    /// Polls the DB to confirm the leader lease is released, with a 10 s
    /// timeout. This is more reliable under load than a fixed sleep.
    pub fn stop(&self) {
        let _ = self
            .sf()
            .arg("daemon")
            .arg("stop")
            .arg("--root")
            .arg(self.root())
            .output();

        // Poll until the lease is released (or 10 s timeout).
        let db_path = self.db_path();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if !db_path.exists() {
                break;
            }
            if let Ok(false) = source_fast_core::is_leader_active_readonly(&db_path) {
                // Lease released — add brief extra sleep for Windows file handle cleanup.
                std::thread::sleep(std::time::Duration::from_millis(500));
                break;
            }
        }
    }

    /// Get the daemon status for this repo.
    pub fn status(&self) -> std::process::Output {
        self.sf()
            .arg("daemon")
            .arg("status")
            .arg("--root")
            .arg(self.root())
            .output()
            .expect("sf daemon status failed")
    }
}

impl Drop for TestFixture {
    fn drop(&mut self) {
        // Clean up any daemon process running for this repo.
        let _ = StdCommand::new(env!("CARGO_BIN_EXE_sf"))
            .arg("daemon")
            .arg("stop")
            .arg("--root")
            .arg(self.dir.path())
            .output();
        // Wait for daemon to release DB files. Poll leader table if DB exists.
        let db_path = self.dir.path().join(".source_fast").join("index.mdb");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if !db_path.exists() {
                break;
            }
            if let Ok(false) = source_fast_core::is_leader_active_readonly(&db_path) {
                std::thread::sleep(std::time::Duration::from_millis(300));
                break;
            }
        }
    }
}

impl Default for TestFixture {
    fn default() -> Self {
        Self::new()
    }
}
