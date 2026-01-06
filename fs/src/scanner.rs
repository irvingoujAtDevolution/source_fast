use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
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
    _repo: &Repository,
    workdir: &Path,
) -> Result<Vec<PathBuf>, IndexError> {
    let mut paths = Vec::new();

    match Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workdir)
        .output()
    {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if line.len() <= 3 {
                    continue;
                }
                // Porcelain v1: two status chars + space, then the path.
                let path_str = &line[3..];
                let trimmed = path_str.trim();
                if trimmed.is_empty() {
                    continue;
                }
                paths.push(workdir.join(trimmed));
            }
        }
        Ok(output) => {
            warn!(
                "collect_worktree_candidates: git status --porcelain exited with status {} – treating as no worktree candidates",
                output.status
            );
        }
        Err(err) => {
            warn!(
                "collect_worktree_candidates: failed to run git status --porcelain: {err} – treating as no worktree candidates"
            );
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
        "initial_git_scan: starting ls-files/status based scan at {}",
        workdir.display()
    );

    let mut candidates: HashSet<PathBuf> = HashSet::new();

    // 1. Tracked files: equivalent to `git ls-files`.
    match Command::new("git")
        .arg("ls-files")
        .current_dir(workdir)
        .output()
    {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                candidates.insert(workdir.join(trimmed));
            }
        }
        Ok(output) => {
            warn!(
                "initial_git_scan: git ls-files exited with status {} – falling back to full walk",
                output.status
            );
            initial_scan(root, Arc::clone(&index))?;
            if let Err(err) = index.set_meta("git_head", current_head) {
                warn!("smart_scan: failed to store git_head in meta: {err}");
            } else {
                info!("smart_scan: stored git_head={} in meta", current_head);
            }
            return Ok(());
        }
        Err(err) => {
            warn!(
                "initial_git_scan: failed to run git ls-files: {err} – falling back to full walk"
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

    // 2. Dirty / untracked state: `git status --porcelain`.
    match Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workdir)
        .output()
    {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if line.len() <= 3 {
                    continue;
                }
                // Porcelain v1: two status chars + space, then the path.
                let path_str = &line[3..];
                let trimmed = path_str.trim();
                if trimmed.is_empty() {
                    continue;
                }
                candidates.insert(workdir.join(trimmed));
            }
        }
        Ok(output) => {
            warn!(
                "initial_git_scan: git status --porcelain exited with status {} – continuing without dirty-state candidates",
                output.status
            );
        }
        Err(err) => {
            warn!(
                "initial_git_scan: failed to run git status --porcelain: {err} – continuing without dirty-state candidates"
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
