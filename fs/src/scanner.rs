use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use gix::Repository;
use gix::bstr::ByteSlice;
use gix::object::tree::diff::ChangeDetached;
use ignore::{WalkBuilder, WalkState};
use source_fast_core::{IndexError, PersistentIndex};
use tracing::{debug, info, warn};

/// Smart scan entry point.
///
/// - If this is the first run (no `git_head` stored) or incremental diff fails,
///   fall back to a full filesystem-based scan and then store the current HEAD
///   in `meta.git_head`.
/// - If `git_head` matches the current HEAD, we assume the index is up-to-date
///   and return immediately (no-op).
/// - If `git_head` differs and the old commit can be found, apply a tree diff
///   between the old and new HEAD trees and only touch changed paths.
pub fn smart_scan(root: &Path, index: Arc<PersistentIndex>) -> Result<(), IndexError> {
    let repo = match gix::discover(root) {
        Ok(repo) => repo,
        Err(err) => {
            debug!("smart_scan: no git repository detected: {err}, falling back to full scan");
            return initial_scan(root, index);
        }
    };

    let head = match repo.head_commit() {
        Ok(commit) => commit,
        Err(err) => {
            debug!("smart_scan: failed to read git HEAD commit: {err}, falling back to full scan");
            return initial_scan(root, index);
        }
    };

    let current_id = head.id;
    let current_str = current_id.to_string();

    let stored_head = match index.get_meta("git_head") {
        Ok(v) => v,
        Err(err) => {
            warn!("smart_scan: failed to read git_head from meta: {err}, treating as first run");
            None
        }
    };

    let workdir = repo
        .work_dir()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| root.to_path_buf());

    let mut candidates: HashSet<PathBuf> = HashSet::new();

    match stored_head {
        Some(ref stored) if stored == &current_str => {
            info!(
                "smart_scan: git_head matches current HEAD ({}), checking worktree changes",
                stored
            );
            let worktree_paths = collect_worktree_candidates(&repo, &workdir)?;
            candidates.extend(worktree_paths);
        }
        Some(ref stored) => {
            info!(
                "smart_scan: attempting incremental diff from {} to {}",
                stored, current_str
            );
            match collect_head_diff_candidates(&repo, &workdir, stored, &current_str) {
                Ok(diff_paths) => {
                    info!(
                        "smart_scan: tree diff produced {} candidate paths",
                        diff_paths.len()
                    );
                    candidates.extend(diff_paths);
                    let worktree_paths = collect_worktree_candidates(&repo, &workdir)?;
                    candidates.extend(worktree_paths);
                }
                Err(err) => {
                    warn!("smart_scan: incremental diff failed: {err}, falling back to full scan");
                    // Fallback: full scan, then store current HEAD.
                    initial_scan(root, Arc::clone(&index))?;
                    if let Err(err) = index.set_meta("git_head", &current_str) {
                        warn!("smart_scan: failed to store git_head in meta: {err}");
                    } else {
                        info!("smart_scan: stored git_head={} in meta", current_str);
                    }
                    return Ok(());
                }
            }
        }
        None => {
            info!("smart_scan: no git_head stored in index yet (first run?)");
            initial_git_scan(root, &workdir, Arc::clone(&index), &current_str)?;
            return Ok(());
        }
    }

    if candidates.is_empty() {
        debug!("smart_scan: no incremental candidates to process");
        // Even if there were no changes, make sure the HEAD checkpoint is up to date.
        if let Err(err) = index.set_meta("git_head", &current_str) {
            warn!("smart_scan: failed to store git_head in meta: {err}");
        }
        return Ok(());
    }

    apply_changes_by_files(root, &index, candidates)?;

    if let Err(err) = index.set_meta("git_head", &current_str) {
        warn!("smart_scan: failed to store git_head in meta: {err}");
    } else {
        info!("smart_scan: stored git_head={} in meta", current_str);
    }

    Ok(())
}

fn collect_worktree_candidates(
    repo: &Repository,
    workdir: &Path,
) -> Result<Vec<PathBuf>, IndexError> {
    use gix::status::index_worktree::iter::Item;

    let mut paths = Vec::new();

    // Use gix's status API to find modified/untracked files
    let status = match repo.status(gix::progress::Discard) {
        Ok(s) => s,
        Err(err) => {
            warn!(
                "collect_worktree_candidates: failed to get status: {err} – treating as no worktree candidates"
            );
            return Ok(paths);
        }
    };

    let platform = match status.into_index_worktree_iter(Vec::new()) {
        Ok(p) => p,
        Err(err) => {
            warn!(
                "collect_worktree_candidates: failed to create status iterator: {err} – treating as no worktree candidates"
            );
            return Ok(paths);
        }
    };

    for item in platform {
        let item = match item {
            Ok(i) => i,
            Err(err) => {
                warn!("collect_worktree_candidates: error iterating status: {err}");
                continue;
            }
        };

        // Get the path from the status item based on its variant
        match &item {
            Item::Modification { rela_path, .. } => {
                let rel_str = match std::str::from_utf8(rela_path.as_bytes()) {
                    Ok(s) => s,
                    Err(err) => {
                        warn!("collect_worktree_candidates: non-utf8 path: {err}");
                        continue;
                    }
                };
                paths.push(workdir.join(rel_str));
            }
            Item::DirectoryContents { entry, .. } => {
                let rel_str = match std::str::from_utf8(entry.rela_path.as_bytes()) {
                    Ok(s) => s,
                    Err(err) => {
                        warn!("collect_worktree_candidates: non-utf8 path: {err}");
                        continue;
                    }
                };
                paths.push(workdir.join(rel_str));
            }
            Item::Rewrite {
                source,
                dirwalk_entry,
                ..
            } => {
                // Add the source (old) path
                let source_path = source.rela_path();
                let source_str = match std::str::from_utf8(source_path.as_bytes()) {
                    Ok(s) => s,
                    Err(err) => {
                        warn!("collect_worktree_candidates: non-utf8 source path: {err}");
                        continue;
                    }
                };
                paths.push(workdir.join(source_str));

                // Add the destination (new) path
                let dest_str = match std::str::from_utf8(dirwalk_entry.rela_path.as_bytes()) {
                    Ok(s) => s,
                    Err(err) => {
                        warn!("collect_worktree_candidates: non-utf8 dest path: {err}");
                        continue;
                    }
                };
                paths.push(workdir.join(dest_str));
            }
        }
    }

    Ok(paths)
}

fn collect_head_diff_candidates(
    repo: &Repository,
    workdir: &Path,
    stored_head: &str,
    current_head: &str,
) -> Result<Vec<PathBuf>, IndexError> {
    use gix::hash::ObjectId;

    let old_id = ObjectId::from_hex(stored_head.as_bytes())
        .map_err(|e| IndexError::Encode(format!("invalid stored git_head {stored_head}: {e}")))?;

    let old_commit = repo.find_commit(old_id).map_err(|e| {
        IndexError::Encode(format!(
            "failed to find stored HEAD commit {stored_head}: {e}"
        ))
    })?;
    let new_commit = repo.head_commit().map_err(|e| {
        IndexError::Encode(format!("failed to read current HEAD {current_head}: {e}"))
    })?;

    let old_tree = old_commit.tree().map_err(|e| {
        IndexError::Encode(format!(
            "failed to read tree for old HEAD {stored_head}: {e}"
        ))
    })?;
    let new_tree = new_commit.tree().map_err(|e| {
        IndexError::Encode(format!(
            "failed to read tree for new HEAD {current_head}: {e}"
        ))
    })?;

    let changes = repo
        .diff_tree_to_tree(&old_tree, &new_tree, None)
        .map_err(|e| IndexError::Encode(format!("tree diff failed: {e}")))?;

    if changes.is_empty() {
        info!("smart_scan: tree diff reported no changes between heads");
        return Ok(Vec::new());
    }

    info!(
        "smart_scan: applying {} tree changes (collecting candidates)",
        changes.len()
    );

    let mut paths = Vec::with_capacity(changes.len());
    for change in changes {
        match change {
            ChangeDetached::Addition { location, .. } => {
                let rel = location.as_bstr();
                let rel_str = std::str::from_utf8(rel.as_bytes()).map_err(|e| {
                    IndexError::Encode(format!("non-utf8 path in addition {rel:?}: {e}"))
                })?;
                let abs = workdir.join(rel_str);
                paths.push(abs);
            }
            ChangeDetached::Modification { location, .. } => {
                let rel = location.as_bstr();
                let rel_str = std::str::from_utf8(rel.as_bytes()).map_err(|e| {
                    IndexError::Encode(format!("non-utf8 path in modification {rel:?}: {e}"))
                })?;
                let abs = workdir.join(rel_str);
                paths.push(abs);
            }
            ChangeDetached::Rewrite {
                source_location,
                location,
                ..
            } => {
                // For renames/rewrites, we need BOTH paths:
                // - source_location (old path) to remove from index
                // - location (new path) to add to index
                let old_rel = source_location.as_bstr();
                let old_rel_str = std::str::from_utf8(old_rel.as_bytes()).map_err(|e| {
                    IndexError::Encode(format!("non-utf8 path in rewrite source {old_rel:?}: {e}"))
                })?;
                paths.push(workdir.join(old_rel_str));

                let new_rel = location.as_bstr();
                let new_rel_str = std::str::from_utf8(new_rel.as_bytes()).map_err(|e| {
                    IndexError::Encode(format!("non-utf8 path in rewrite dest {new_rel:?}: {e}"))
                })?;
                paths.push(workdir.join(new_rel_str));
            }
            ChangeDetached::Deletion { location, .. } => {
                let rel = location.as_bstr();
                let rel_str = std::str::from_utf8(rel.as_bytes()).map_err(|e| {
                    IndexError::Encode(format!("non-utf8 path in deletion {rel:?}: {e}"))
                })?;
                let abs = workdir.join(rel_str);
                paths.push(abs);
            }
        }
    }

    Ok(paths)
}

fn initial_git_scan(
    root: &Path,
    workdir: &Path,
    index: Arc<PersistentIndex>,
    current_head: &str,
) -> Result<(), IndexError> {
    info!(
        "initial_git_scan: starting gix-based scan at {}",
        workdir.display()
    );

    let repo = match gix::discover(workdir) {
        Ok(r) => r,
        Err(err) => {
            warn!(
                "initial_git_scan: failed to open repository: {err} – falling back to full walk"
            );
            initial_scan(root, Arc::clone(&index))?;
            if let Err(err) = index.set_meta("git_head", current_head) {
                warn!("smart_scan: failed to store git_head in meta: {err}");
            } else {
                info!("smart_scan: stored git_head={} in meta", current_head);
            }
            return Ok(());
        }
    };

    let mut candidates: HashSet<PathBuf> = HashSet::new();

    // 1. Tracked files: equivalent to `git ls-files` using gix index
    match repo.index() {
        Ok(git_index) => {
            for entry in git_index.entries() {
                let rel_path = entry.path(&git_index);
                let rel_str = match std::str::from_utf8(rel_path.as_bytes()) {
                    Ok(s) => s,
                    Err(err) => {
                        warn!("initial_git_scan: non-utf8 path in index: {err}");
                        continue;
                    }
                };
                candidates.insert(workdir.join(rel_str));
            }
            info!(
                "initial_git_scan: found {} tracked files from index",
                candidates.len()
            );
        }
        Err(err) => {
            warn!(
                "initial_git_scan: failed to read git index: {err} – falling back to full walk"
            );
            initial_scan(root, Arc::clone(&index))?;
            if let Err(err) = index.set_meta("git_head", current_head) {
                warn!("smart_scan: failed to store git_head in meta: {err}");
            } else {
                info!("smart_scan: stored git_head={} in meta", current_head);
            }
            return Ok(());
        }
    }

    // 2. Dirty / untracked state using gix status
    match collect_worktree_candidates(&repo, workdir) {
        Ok(dirty_paths) => {
            let dirty_count = dirty_paths.len();
            candidates.extend(dirty_paths);
            if dirty_count > 0 {
                info!(
                    "initial_git_scan: found {} dirty/untracked files",
                    dirty_count
                );
            }
        }
        Err(err) => {
            warn!(
                "initial_git_scan: failed to collect worktree candidates: {err} – continuing without dirty-state candidates"
            );
        }
    }

    apply_changes_by_files(root, &index, candidates)?;

    if let Err(err) = index.set_meta("git_head", current_head) {
        warn!("smart_scan: failed to store git_head in meta: {err}");
    } else {
        info!("smart_scan: stored git_head={} in meta", current_head);
    }

    Ok(())
}

fn apply_changes_by_files(
    root: &Path,
    index: &PersistentIndex,
    files: impl IntoIterator<Item = PathBuf>,
) -> Result<(), IndexError> {
    let exclude_dir = root.join(".source_fast");
    let git_dir = root.join(".git");

    let mut changed = 0usize;

    for path in files {
        // Respect the requested root: only touch files under it.
        if !path.starts_with(root) {
            continue;
        }

        // Skip our own index directory and the .git directory entirely.
        if path.starts_with(&exclude_dir) || path.starts_with(&git_dir) {
            continue;
        }

        if path.exists() {
            if !path.is_file() {
                continue;
            }
            if let Err(err) = index.index_path(&path) {
                warn!("smart_scan: failed to index path {}: {err}", path.display());
            } else {
                changed += 1;
            }
        } else if let Err(err) = index.remove_path(&path) {
            warn!(
                "smart_scan: failed to remove path {} from index: {err}",
                path.display()
            );
        } else {
            changed += 1;
        }
    }

    if changed > 0 {
        index.flush()?;
        info!(
            "smart_scan: applied {} changes from unified candidate list",
            changed
        );
    } else {
        debug!("smart_scan: no changes to apply from unified candidate list");
    }

    Ok(())
}

/// Initial full scan using filesystem walk.
///
/// This is the current behaviour: walk the tree in parallel, index every file,
/// and flush at the end.
pub fn initial_scan(root: &Path, index: Arc<PersistentIndex>) -> Result<(), IndexError> {
    info!("initial_scan: starting parallel walk at {}", root.display());

    let counter = Arc::new(AtomicUsize::new(0));
    let index_for_scan = Arc::clone(&index);
    let counter_for_scan = Arc::clone(&counter);

    let exclude_dir = root.join(".source_fast");
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .filter_entry(move |entry| {
            let path = entry.path();
            if path.starts_with(&exclude_dir) {
                return false;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name == ".git"
            {
                return false;
            }
            true
        })
        .build_parallel();

    walker.run(|| {
        let index = Arc::clone(&index_for_scan);
        let counter = Arc::clone(&counter_for_scan);

        Box::new(move |entry_res| {
            let entry = match entry_res {
                Ok(e) => e,
                Err(err) => {
                    warn!("initial_scan: failed to read entry: {err}");
                    return WalkState::Continue;
                }
            };

            if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                return WalkState::Continue;
            }

            let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
            if done.is_multiple_of(500) {
                info!("initial_scan: indexed {} files so far", done);
            }

            if let Err(err) = index.index_path(entry.path()) {
                warn!(
                    "initial_scan worker: failed to index {}: {:?}",
                    entry.path().display(),
                    err
                );
            }

            WalkState::Continue
        })
    });

    debug!("initial_scan: parallel walk finished, flushing index");
    index.flush()?;
    let done = counter.load(Ordering::Relaxed);
    info!("initial_scan: completed, indexed {} files in total", done);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn create_test_index(dir: &Path) -> Arc<PersistentIndex> {
        let db_path = dir.join(".source_fast").join("index.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        Arc::new(PersistentIndex::open_or_create(&db_path).unwrap())
    }

    fn init_git_repo(dir: &Path) {
        Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .expect("git init failed");
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .expect("git config email failed");
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .expect("git config name failed");
    }

    fn git_add_commit(dir: &Path, msg: &str) {
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir)
            .output()
            .expect("git add failed");
        Command::new("git")
            .args(["commit", "-m", msg, "--allow-empty"])
            .current_dir(dir)
            .output()
            .expect("git commit failed");
    }

    // ============ Initial Scan Tests ============

    #[test]
    fn test_initial_scan_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let index = create_test_index(temp_dir.path());

        let result = initial_scan(temp_dir.path(), index);
        assert!(result.is_ok());
    }

    #[test]
    fn test_initial_scan_with_files() {
        let temp_dir = TempDir::new().unwrap();

        // Create some files
        std::fs::write(temp_dir.path().join("file1.txt"), "content one").unwrap();
        std::fs::write(temp_dir.path().join("file2.txt"), "content two").unwrap();

        let index = create_test_index(temp_dir.path());
        initial_scan(temp_dir.path(), Arc::clone(&index)).unwrap();

        // Verify files were indexed
        let hits = index.search("content one").unwrap();
        assert_eq!(hits.len(), 1);

        let hits = index.search("content two").unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn test_initial_scan_skips_source_fast_dir() {
        let temp_dir = TempDir::new().unwrap();

        // Create a file that should be indexed
        std::fs::write(temp_dir.path().join("normal.txt"), "normal_content").unwrap();

        // Create the .source_fast directory with a file that should NOT be indexed
        let sf_dir = temp_dir.path().join(".source_fast");
        std::fs::create_dir_all(&sf_dir).unwrap();
        std::fs::write(sf_dir.join("internal.txt"), "internal_content").unwrap();

        let index = create_test_index(temp_dir.path());
        initial_scan(temp_dir.path(), Arc::clone(&index)).unwrap();

        // Normal file should be indexed
        let hits = index.search("normal_content").unwrap();
        assert_eq!(hits.len(), 1);

        // Internal file should NOT be indexed
        let hits = index.search("internal_content").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_initial_scan_skips_git_dir() {
        let temp_dir = TempDir::new().unwrap();
        init_git_repo(temp_dir.path());

        // Create a normal file
        std::fs::write(temp_dir.path().join("normal.txt"), "normal_content").unwrap();

        let index = create_test_index(temp_dir.path());
        initial_scan(temp_dir.path(), Arc::clone(&index)).unwrap();

        // Normal file should be indexed
        let hits = index.search("normal_content").unwrap();
        assert_eq!(hits.len(), 1);

        // .git directory contents should NOT be indexed
        // (We don't search for git internal content since we don't know what's there)
    }

    #[test]
    fn test_initial_scan_respects_gitignore() {
        let temp_dir = TempDir::new().unwrap();
        init_git_repo(temp_dir.path());

        // Create .gitignore
        std::fs::write(temp_dir.path().join(".gitignore"), "ignored.txt\n").unwrap();

        // Create files
        std::fs::write(temp_dir.path().join("tracked.txt"), "tracked_content").unwrap();
        std::fs::write(temp_dir.path().join("ignored.txt"), "ignored_content").unwrap();

        let index = create_test_index(temp_dir.path());
        initial_scan(temp_dir.path(), Arc::clone(&index)).unwrap();

        // Tracked file should be indexed
        let hits = index.search("tracked_content").unwrap();
        assert_eq!(hits.len(), 1);

        // Ignored file should NOT be indexed
        let hits = index.search("ignored_content").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_initial_scan_nested_directories() {
        let temp_dir = TempDir::new().unwrap();

        // Create nested structure
        let nested = temp_dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("deep.txt"), "deep_content").unwrap();

        let index = create_test_index(temp_dir.path());
        initial_scan(temp_dir.path(), Arc::clone(&index)).unwrap();

        // Nested file should be indexed
        let hits = index.search("deep_content").unwrap();
        assert_eq!(hits.len(), 1);
    }

    // ============ Smart Scan Tests ============

    #[test]
    fn test_smart_scan_no_git_falls_back() {
        let temp_dir = TempDir::new().unwrap();

        // Create a file (no git repo)
        std::fs::write(temp_dir.path().join("file.txt"), "file_content").unwrap();

        let index = create_test_index(temp_dir.path());
        let result = smart_scan(temp_dir.path(), index);

        // Should fall back to initial_scan and succeed
        assert!(result.is_ok());
    }

    #[test]
    fn test_smart_scan_first_run_stores_head() {
        let temp_dir = TempDir::new().unwrap();
        init_git_repo(temp_dir.path());

        // Create and commit a file
        std::fs::write(temp_dir.path().join("file.txt"), "file_content").unwrap();
        git_add_commit(temp_dir.path(), "Initial commit");

        let index = create_test_index(temp_dir.path());

        // First run - should store git_head
        smart_scan(temp_dir.path(), Arc::clone(&index)).unwrap();

        let stored_head = index.get_meta("git_head").unwrap();
        assert!(stored_head.is_some(), "git_head should be stored after first run");
    }

    #[test]
    fn test_smart_scan_no_changes_is_noop() {
        let temp_dir = TempDir::new().unwrap();
        init_git_repo(temp_dir.path());

        // Create and commit a file
        std::fs::write(temp_dir.path().join("file.txt"), "file_content").unwrap();
        git_add_commit(temp_dir.path(), "Initial commit");

        let index = create_test_index(temp_dir.path());

        // First scan
        smart_scan(temp_dir.path(), Arc::clone(&index)).unwrap();

        // Second scan with no changes - should complete quickly
        let result = smart_scan(temp_dir.path(), Arc::clone(&index));
        assert!(result.is_ok());
    }

    #[test]
    fn test_smart_scan_detects_new_commit() {
        let temp_dir = TempDir::new().unwrap();
        init_git_repo(temp_dir.path());

        // Initial file and commit
        std::fs::write(temp_dir.path().join("file1.txt"), "content_one").unwrap();
        git_add_commit(temp_dir.path(), "First commit");

        let index = create_test_index(temp_dir.path());
        smart_scan(temp_dir.path(), Arc::clone(&index)).unwrap();

        // Verify first file is indexed
        let hits = index.search("content_one").unwrap();
        assert_eq!(hits.len(), 1);

        // Add new file and commit
        std::fs::write(temp_dir.path().join("file2.txt"), "content_two_unique").unwrap();
        git_add_commit(temp_dir.path(), "Second commit");

        // Smart scan should pick up the new file
        smart_scan(temp_dir.path(), Arc::clone(&index)).unwrap();

        let hits = index.search("content_two_unique").unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn test_smart_scan_detects_dirty_state() {
        let temp_dir = TempDir::new().unwrap();
        init_git_repo(temp_dir.path());

        // Initial commit
        std::fs::write(temp_dir.path().join("file.txt"), "original").unwrap();
        git_add_commit(temp_dir.path(), "Initial commit");

        let index = create_test_index(temp_dir.path());
        smart_scan(temp_dir.path(), Arc::clone(&index)).unwrap();

        // Modify file without committing
        std::fs::write(temp_dir.path().join("file.txt"), "modified_content_xyz").unwrap();

        // Smart scan should pick up the modification
        smart_scan(temp_dir.path(), Arc::clone(&index)).unwrap();

        let hits = index.search("modified_content_xyz").unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn test_smart_scan_detects_untracked_files() {
        let temp_dir = TempDir::new().unwrap();
        init_git_repo(temp_dir.path());

        // Initial commit
        std::fs::write(temp_dir.path().join("tracked.txt"), "tracked").unwrap();
        git_add_commit(temp_dir.path(), "Initial commit");

        let index = create_test_index(temp_dir.path());
        smart_scan(temp_dir.path(), Arc::clone(&index)).unwrap();

        // Add untracked file (not committed)
        std::fs::write(temp_dir.path().join("untracked.txt"), "untracked_content_xyz").unwrap();

        // Smart scan should pick up untracked files
        smart_scan(temp_dir.path(), Arc::clone(&index)).unwrap();

        let hits = index.search("untracked_content_xyz").unwrap();
        assert_eq!(hits.len(), 1);
    }

    // ============ Apply Changes Tests ============

    #[test]
    fn test_apply_changes_adds_new_files() {
        let temp_dir = TempDir::new().unwrap();
        let index = create_test_index(temp_dir.path());

        // Create a file
        let file_path = temp_dir.path().join("new_file.txt");
        std::fs::write(&file_path, "new_file_content").unwrap();

        // Apply changes for this file
        apply_changes_by_files(temp_dir.path(), &index, vec![file_path]).unwrap();

        let hits = index.search("new_file_content").unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn test_apply_changes_removes_deleted_files() {
        let temp_dir = TempDir::new().unwrap();
        let index = create_test_index(temp_dir.path());

        // Create and index a file
        let file_path = temp_dir.path().join("to_delete.txt");
        std::fs::write(&file_path, "delete_me_content").unwrap();
        index.index_path(&file_path).unwrap();
        index.flush().unwrap();

        // Verify it's indexed
        let hits = index.search("delete_me_content").unwrap();
        assert_eq!(hits.len(), 1);

        // Delete the file
        std::fs::remove_file(&file_path).unwrap();

        // Apply changes - should remove from index
        apply_changes_by_files(temp_dir.path(), &index, vec![file_path]).unwrap();

        let hits = index.search("delete_me_content").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_apply_changes_skips_directories() {
        let temp_dir = TempDir::new().unwrap();
        let index = create_test_index(temp_dir.path());

        // Create a directory (not a file)
        let dir_path = temp_dir.path().join("some_dir");
        std::fs::create_dir(&dir_path).unwrap();

        // Apply changes - should not error even though it's a directory
        let result = apply_changes_by_files(temp_dir.path(), &index, vec![dir_path]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_apply_changes_skips_files_outside_root() {
        let temp_dir = TempDir::new().unwrap();
        let other_dir = TempDir::new().unwrap();

        let index = create_test_index(temp_dir.path());

        // Create a file outside the root
        let outside_file = other_dir.path().join("outside.txt");
        std::fs::write(&outside_file, "outside_content").unwrap();

        // Apply changes - should skip this file
        apply_changes_by_files(temp_dir.path(), &index, vec![outside_file]).unwrap();

        // File should NOT be indexed (it's outside the root)
        let hits = index.search("outside_content").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_apply_changes_skips_source_fast_dir() {
        let temp_dir = TempDir::new().unwrap();
        let index = create_test_index(temp_dir.path());

        // Create a file inside .source_fast
        let sf_file = temp_dir.path().join(".source_fast").join("internal.txt");
        std::fs::create_dir_all(sf_file.parent().unwrap()).unwrap();
        std::fs::write(&sf_file, "internal_content").unwrap();

        // Apply changes - should skip this file
        apply_changes_by_files(temp_dir.path(), &index, vec![sf_file]).unwrap();

        // File should NOT be indexed
        let hits = index.search("internal_content").unwrap();
        assert!(hits.is_empty());
    }
}
