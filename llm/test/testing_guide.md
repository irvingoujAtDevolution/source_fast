# E2E Testing Guide for source_fast

## Overview

This document covers the E2E test system implementation, best practices, pitfalls, and lessons learned.

## Test Stack

| Crate | Purpose |
|-------|---------|
| `assert_cmd` | Run CLI binary and assert on output |
| `assert_fs` | Create temp directories and files |
| `predicates` | Flexible assertions for stdout/stderr |
| `insta` | Snapshot testing (available but not heavily used yet) |

## Test Structure

```
app/tests/
├── common/mod.rs          # TestFixture helper
├── e2e_basic.rs           # Phase 1: Basic functionality
├── e2e_filesystem.rs      # Phase 3: File system edge cases
├── e2e_git.rs             # Phase 2: Git integration
├── e2e_resilience.rs      # Phase 4: Recovery & error handling
├── e2e_search.rs          # Phase 5: Search quality & MCP
```

## How to Add New Tests

### 1. Use TestFixture

```rust
#[test]
fn test_my_feature() {
    let fix = TestFixture::new()
        .git_init()                                    // Optional: init git repo
        .add_file("src/main.rs", "fn my_code() {}")    // Add test files
        .git_commit("initial");                        // Optional: commit

    fix.index();  // Run sf index

    let output = fix.search("my_code");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("main.rs"), "Should find file: {}", stdout);
}
```

### 2. Available TestFixture Methods

**File Operations:**
- `add_file(path, content)` - Create text file
- `add_binary(path, bytes)` - Create binary file
- `remove_file(path)` - Delete file
- `rename_file(from, to)` - Rename/move file

**Git Operations:**
- `git_init()` - Initialize git repo with test user config
- `git_commit(msg)` - Stage all (including deletions) and commit
- `git_checkout_new(branch)` - Create and switch to new branch
- `git_checkout(branch)` - Switch to existing branch
- `git_reset_hard(ref)` - Hard reset to ref
- `git_ignore(pattern)` - Add pattern to .gitignore
- `git(args)` - Run arbitrary git command

**sf CLI:**
- `index()` - Run `sf index` and assert success
- `search(query)` - Run `sf search` and return output
- `search_file(pattern)` - Run `sf search-file` and return output
- `sf()` - Get Command for custom args

### 3. Test Naming Convention

```rust
// Format: test_{phase_id}_{description}
fn test_b1_fresh_init() { }      // Basic test B1
fn test_g3_dirty_state() { }     // Git test G3
fn test_f2_rename() { }          // Filesystem test F2
```

## Pitfalls & Lessons Learned

### 1. CLI Argument Order

**WRONG:**
```rust
fix.sf().args(["--root", path, "index"])  // --root before subcommand
```

**CORRECT:**
```rust
fix.sf().args(["index", "--root", path])  // --root after subcommand
```

The `--root` flag is per-subcommand, not global. Each subcommand (index, search, server) has its own `--root` option.

### 2. Git Staging Deletions

**WRONG:**
```rust
self.git(&["add", "."]);  // Doesn't stage deletions!
```

**CORRECT:**
```rust
self.git(&["add", "-A"]);  // -A stages deletions too
```

Using `git add .` only stages new/modified files. Deleted files require `-A` flag.

### 3. Cross-Platform Path Handling

**WRONG:**
```rust
.arg("--file-regex")
.arg("src/.*")  // Unix paths don't match Windows backslashes
```

**CORRECT:**
```rust
.arg("--file-regex")
.arg("main")  // Use filename patterns that work on both
```

On Windows, paths use backslashes (`src\main.rs`), so regex like `src/.*` won't match. Use patterns that match regardless of separator.

### 4. Git Branch Names

Different git versions use different default branch names:
- Older git: `master`
- Newer git: `main`

**Handle both:**
```rust
let result = fix.git(&["checkout", "main"]);
if !result.status.success() {
    fix.git(&["checkout", "master"]);
}
```

### 5. Non-Git Directories

The tool is designed for git repos. File deletion/rename tracking **only works with git**:
- `smart_scan()` uses git diff to detect changes
- Non-git directories don't track deletions properly

**Always use `git_init()` for tests that involve file mutations.**

### 6. Search Output Contains ANSI Codes

The search output includes tracing logs with ANSI color codes:
```
[2m2026-01-06T18:22:36.189017Z[0m [33m WARN[0m ...
```

When checking output, be aware these codes are present. Use `contains()` rather than exact matching.

## Previously Known Bugs (Now Fixed)

The following tests were previously `#[ignore]` but are now enabled after fixes in commit `6b11557`:

| Test | Bug | Fix |
|------|-----|-----|
| `test_f1_deletion` | Deleted files not removed from index | `normalize_path()` now handles deleted files by canonicalizing parent directory |
| `test_f2_rename` | Renamed files leave ghost entries | Scanner now adds both source and destination paths for Rewrite changes |
| `test_g4_branch_switch` | Branch files not cleaned up | Same fix as above |
| `test_g5_git_reset` | Reset files remain indexed | Same fix as above |

**Root Cause (Fixed):** Two issues were causing deleted files to remain in the index:
1. `normalize_path()` in `core/src/text.rs` used `path.canonicalize()` which fails for deleted files
2. Git rename operations (Rewrite changes) only added the new path, missing the old path

## Running Tests

```bash
# All tests
cargo test --workspace

# Specific test file
cargo test --package app --test e2e_basic

# Single test
cargo test --package app --test e2e_git test_g1_new_commit

# With output
cargo test --package app -- --nocapture

# Include ignored tests
cargo test --package app -- --ignored
```

## CI/CD

GitHub Actions runs on every push/PR:
- Ubuntu, Windows, macOS
- Runs `cargo test --workspace`
- Runs `cargo clippy` and `cargo fmt --check`

See `.github/workflows/test.yml`

## Best Practices Summary

1. **Always use TestFixture** - Don't create temp dirs manually
2. **Use git for mutation tests** - Deletion/rename tracking needs git
3. **Test one thing per test** - Keep tests focused
4. **Use descriptive assertions** - Include actual output in failure messages
5. **Handle platform differences** - Paths, line endings, branch names
6. **Document bugs with ignored tests** - Don't delete failing tests, mark them
7. **Keep tests fast** - Avoid large files, unnecessary commits
