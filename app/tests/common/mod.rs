//! Test helper module for E2E tests
//!
//! Provides `TestFixture` for easy test setup with file and git operations.

#![allow(dead_code)] // Test helpers may not be used in all test modules
#![allow(deprecated)] // cargo_bin() deprecation - the new API requires more investigation

use assert_cmd::Command;
use assert_fs::prelude::*;
use assert_fs::TempDir;
use std::path::PathBuf;
use std::process::Command as StdCommand;

/// Test fixture providing a temporary directory with helper methods
/// for file operations, git commands, and running the `sf` CLI.
pub struct TestFixture {
    pub dir: TempDir,
}

impl TestFixture {
    /// Create a new test environment with a fresh temp directory
    pub fn new() -> Self {
        Self {
            dir: TempDir::new().unwrap(),
        }
    }

    /// Get the root path of the test directory
    pub fn root(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    /// Get the expected db path (.source_fast/index.db)
    pub fn db_path(&self) -> PathBuf {
        self.root().join(".source_fast").join("index.db")
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

    /// Run sf index and assert success
    pub fn index(&self) -> &Self {
        self.sf()
            .arg("index")
            .arg("--root")
            .arg(self.root())
            .assert()
            .success();
        self
    }

    /// Run sf search and return the output
    pub fn search(&self, query: &str) -> std::process::Output {
        self.sf()
            .arg("search")
            .arg("--root")
            .arg(self.root())
            .arg(query)
            .output()
            .expect("sf search failed")
    }

    /// Run sf search-file and return the output
    pub fn search_file(&self, pattern: &str) -> std::process::Output {
        self.sf()
            .arg("search-file")
            .arg("--root")
            .arg(self.root())
            .arg(pattern)
            .output()
            .expect("sf search-file failed")
    }
}

impl Default for TestFixture {
    fn default() -> Self {
        Self::new()
    }
}
