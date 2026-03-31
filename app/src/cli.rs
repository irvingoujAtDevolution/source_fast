use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use regex::Regex;
use source_fast_core::{
    IndexError, PersistentIndex, extract_snippet, read_meta_readonly, rewrite_root_paths,
    search_database_file_filtered, search_files_in_database,
};
use source_fast_progress::IndexProgress;
use tracing::{debug, error, info, warn};

use crate::daemon;

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

pub fn default_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn default_db_path(root: &Path) -> PathBuf {
    let mut dir = root.to_path_buf();
    dir.push(".source_fast");
    let _ = std::fs::create_dir_all(&dir);
    dir.push("index.mdb");
    dir
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

/// Strip the `\\?\` extended path prefix that Windows canonicalization adds.
fn clean_display_path(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

/// Truncate a line to `max_chars` characters, appending `...` if truncated.
fn truncate_line(line: &str, max_chars: usize) -> String {
    if line.len() <= max_chars {
        line.to_string()
    } else {
        format!("{}...", &line[..max_chars])
    }
}

// ---------------------------------------------------------------------------
// DB helpers (shared with daemon)
// ---------------------------------------------------------------------------

fn remove_db_files(db_path: &Path) {
    let _ = std::fs::remove_dir_all(db_path);
}

fn is_corrupt_db(err: &IndexError) -> bool {
    match err {
        IndexError::Db(db_err) => {
            db_err.contains("Invalid")
                || db_err.contains("corrupted")
                || db_err.contains("MDB_INVALID")
                || db_err.contains("MDB_VERSION_MISMATCH")
        }
        _ => false,
    }
}

fn primary_worktree_root(root: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("worktree")
        .arg("list")
        .arg("--porcelain")
        .current_dir(root)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Some(PathBuf::from(trimmed));
            }
        }
    }

    None
}

fn same_path(lhs: &Path, rhs: &Path) -> bool {
    match (lhs.canonicalize(), rhs.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => lhs == rhs,
    }
}

/// Copy the LMDB data file from `source_root`'s index to `db_path`.
/// Only copies `data.mdb` (not `lock.mdb` which is process-local).
///
/// SAFETY: This copies the LMDB data file directly without coordinating with
/// any active writer. Only safe when no daemon is running on `source_root`'s
/// database, or when the caller accepts a snapshot-in-time copy (LMDB's
/// data.mdb is always in a committed-consistent state on disk).
fn copy_db_from_root(source_root: &Path, db_path: &Path) -> std::io::Result<bool> {
    let source_db = source_root.join(".source_fast").join("index.mdb");
    if !source_db.exists() {
        return Ok(false);
    }

    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::create_dir_all(db_path)?;
    let source_data = source_db.join("data.mdb");
    if source_data.exists() {
        std::fs::copy(&source_data, db_path.join("data.mdb"))?;
    }

    Ok(true)
}

fn copy_db_from_primary_worktree(root: &Path, db_path: &Path) -> Option<PathBuf> {
    let Some(primary_root) = primary_worktree_root(root) else {
        return None;
    };

    if same_path(&primary_root, root) {
        return None;
    }

    match copy_db_from_root(&primary_root, db_path) {
        Ok(true) => Some(primary_root),
        _ => None,
    }
}

pub(crate) fn open_index_with_worktree_copy(
    root: &Path,
    db_path: &Path,
) -> Result<PersistentIndex, IndexError> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(IndexError::Io)?;
    }

    if db_path.exists() {
        match PersistentIndex::open_or_create(db_path) {
            Ok(index) => return Ok(index),
            Err(err) => {
                if !is_corrupt_db(&err) {
                    return Err(err);
                }
                remove_db_files(db_path);
            }
        }
    }

    if let Some(primary_root) = copy_db_from_primary_worktree(root, db_path) {
        let _ = rewrite_root_paths(db_path, &primary_root, root);
        match PersistentIndex::open_or_create(db_path) {
            Ok(index) => return Ok(index),
            Err(err) => {
                if !is_corrupt_db(&err) {
                    return Err(err);
                }
                remove_db_files(db_path);
            }
        }
    }

    PersistentIndex::open_or_create(db_path)
}

// ---------------------------------------------------------------------------
// Tracing setup
// ---------------------------------------------------------------------------

/// Initialize tracing for CLI commands (search/stop/status).
///
/// Log destination is controlled by `SOURCE_FAST_LOG_PATH`:
/// - If set, logs go to that file (append mode).
/// - If unset, tracing is effectively disabled (no stderr noise).
///
/// Log level is controlled by `RUST_LOG` (default: `info`).
pub fn init_tracing_cli() {
    use std::fs::OpenOptions;
    use std::path::PathBuf;
    use tracing_subscriber::{EnvFilter, fmt};

    let path = match std::env::var("SOURCE_FAST_LOG_PATH") {
        Ok(p) if !p.trim().is_empty() => PathBuf::from(p),
        _ => return, // No log path → no tracing output
    };

    if OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .is_err()
    {
        return;
    }

    let make_writer = move || {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("failed to open SOURCE_FAST_LOG_PATH for logging")
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(make_writer)
        .init();
}

/// Initialize tracing for MCP server.
///
/// - Never logs to stdout (to keep stdio clean for JSON-RPC).
/// - If `SOURCE_FAST_LOG_PATH` is set, append logs to that file.
/// - If not set or file cannot be opened, logging is effectively disabled.
pub fn init_tracing_server() {
    use std::fs::OpenOptions;
    use std::path::PathBuf;
    use tracing_subscriber::{EnvFilter, fmt};

    let path = match std::env::var("SOURCE_FAST_LOG_PATH") {
        Ok(p) if !p.trim().is_empty() => PathBuf::from(p),
        _ => return,
    };

    if OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .is_err()
    {
        return;
    }

    let make_writer = move || {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("failed to open SOURCE_FAST_LOG_PATH for logging")
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(make_writer)
        .init();
}

// ---------------------------------------------------------------------------
// Search commands (daemon-aware)
// ---------------------------------------------------------------------------

pub async fn run_search_with_daemon(
    root: Option<PathBuf>,
    db: Option<PathBuf>,
    query: String,
    file_regex: Option<String>,
    wait: bool,
    limit: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let command_started = Instant::now();
    let root = root.unwrap_or_else(default_root);
    let db_path = db.unwrap_or_else(|| default_db_path(&root));

    let file_regex = if let Some(pattern) = file_regex {
        match Regex::new(&pattern) {
            Ok(re) => Some(re),
            Err(err) => {
                error!("Invalid file regex '{}': {}", pattern, err);
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    let first_time = !db_path.exists();
    info!(
        root = %root.display(),
        db = %db_path.display(),
        query = %query,
        file_regex = ?file_regex.as_ref().map(|re| re.as_str()),
        wait,
        first_time,
        "search command starting"
    );

    // Ensure a daemon (or MCP server) is keeping the index warm.
    let ensure_started = Instant::now();
    let was_running = daemon::ensure_daemon(&root, &db_path)?;
    info!(
        root = %root.display(),
        db = %db_path.display(),
        was_running,
        elapsed_ms = ensure_started.elapsed().as_millis() as u64,
        "ensure_daemon finished for search command"
    );

    if first_time {
        eprintln!("Starting index for the first time. Results will be partial until indexing completes.");
    }

    if !was_running {
        let daemon_wait_started = Instant::now();
        let confirmed = daemon::wait_for_daemon(&db_path, Duration::from_secs(3));
        info!(
            db = %db_path.display(),
            confirmed,
            elapsed_ms = daemon_wait_started.elapsed().as_millis() as u64,
            "daemon readiness wait finished for search command"
        );
        if !confirmed {
            warn!("Daemon did not confirm in 3 s, proceeding with search anyway");
        }
    }

    // If --wait, block until index is complete.
    if wait {
        let index_wait_started = Instant::now();
        let complete = daemon::wait_for_index_complete(&db_path, Duration::from_secs(120));
        info!(
            db = %db_path.display(),
            complete,
            elapsed_ms = index_wait_started.elapsed().as_millis() as u64,
            "index completion wait finished for search command"
        );
        if !complete {
            eprintln!("Timed out waiting for index to complete (120 s).");
        }
    }

    if !db_path.exists() {
        // DB hasn't been created yet (daemon just started). Nothing to search.
        info!(
            db = %db_path.display(),
            elapsed_ms = command_started.elapsed().as_millis() as u64,
            "search command finished before database directory was created"
        );
        return Ok(());
    }

    // Check completeness for the disclaimer.
    if let Ok(Some(status)) = read_meta_readonly(&db_path, daemon::meta_keys::INDEX_STATUS) {
        debug!(db = %db_path.display(), index_status = %status, "search command observed index status");
        if status != daemon::index_status::COMPLETE {
            eprintln!("Note: index is still building. Results may be incomplete.");
        }
    }

    // Get trigram search hits (fast — bitmap intersection only, no file I/O).
    let mut hits = match search_database_file_filtered(&db_path, &query, file_regex.as_ref()) {
        Ok(h) => h,
        Err(err) => {
            error!(db = %db_path.display(), query = %query, error = ?err, "search command failed");
            std::process::exit(1);
        }
    };
    hits.sort_by(|a, b| a.path.cmp(&b.path));

    let total = hits.len();
    let display_limit = if limit > 0 { limit } else { total };

    // Stream results using a channel: rayon workers extract snippets in parallel
    // and send results through a channel. The main thread prints as they arrive.
    // Workers check `done` flag to stop early when the display limit is reached.
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::sync_channel::<(String, Option<source_fast_core::Snippet>)>(32);

    let query_for_workers = query.clone();
    let done_for_workers = Arc::clone(&done);
    std::thread::spawn(move || {
        use rayon::prelude::*;
        hits.par_iter().for_each(|hit| {
            if done_for_workers.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let path = PathBuf::from(&hit.path);
            let snippet = extract_snippet(&path, &query_for_workers)
                .ok()
                .flatten();
            // Send error means receiver dropped (limit reached) — stop.
            if tx.send((hit.path.clone(), snippet)).is_err() {
                done_for_workers.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
    });

    let mut printed = 0usize;
    let mut no_snippet_paths: Vec<String> = Vec::new();

    for (path, snippet) in &rx {
        match snippet {
            Some(snippet) => {
                let path_str = snippet.path.display().to_string();
                let display_path = clean_display_path(&path_str);
                println!("\x1b[35m{display_path}\x1b[0m:{}", snippet.line_number);
                for (line_no, line) in &snippet.lines {
                    let truncated = truncate_line(line, 200);
                    if line.contains(&query) {
                        println!("\x1b[32m{line_no}\x1b[0m:{truncated}");
                    } else {
                        println!("\x1b[2m{line_no}\x1b[0m:{truncated}");
                    }
                }
                println!();
                printed += 1;
            }
            None => {
                no_snippet_paths.push(path);
            }
        }
        // Stop once we have enough snippet + no-snippet results to fill the limit.
        if printed + no_snippet_paths.len() >= display_limit {
            break;
        }
    }
    // Signal workers to stop and drop the receiver so send() fails fast.
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(rx);

    // Print remaining no-snippet results at the end (up to the limit).
    for path in &no_snippet_paths {
        if printed >= display_limit {
            break;
        }
        println!("{}", clean_display_path(path));
        printed += 1;
    }

    if total > display_limit {
        eprintln!(
            "... and {} more results (use --limit 0 for all)",
            total - display_limit
        );
    }

    Ok(())
}

pub async fn run_file_search_with_daemon(
    root: Option<PathBuf>,
    db: Option<PathBuf>,
    pattern: String,
    wait: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let command_started = Instant::now();
    let root = root.unwrap_or_else(default_root);
    let db_path = db.unwrap_or_else(|| default_db_path(&root));

    let first_time = !db_path.exists();
    info!(
        root = %root.display(),
        db = %db_path.display(),
        pattern = %pattern,
        wait,
        first_time,
        "search-file command starting"
    );
    let ensure_started = Instant::now();
    let was_running = daemon::ensure_daemon(&root, &db_path)?;
    info!(
        root = %root.display(),
        db = %db_path.display(),
        was_running,
        elapsed_ms = ensure_started.elapsed().as_millis() as u64,
        "ensure_daemon finished for search-file command"
    );

    if first_time {
        eprintln!("Starting index for the first time. Results will be partial until indexing completes.");
    }

    if !was_running {
        let daemon_wait_started = Instant::now();
        let confirmed = daemon::wait_for_daemon(&db_path, Duration::from_secs(3));
        info!(
            db = %db_path.display(),
            confirmed,
            elapsed_ms = daemon_wait_started.elapsed().as_millis() as u64,
            "daemon readiness wait finished for search-file command"
        );
        if !confirmed {
            warn!("Daemon did not confirm in 3 s, proceeding with search anyway");
        }
    }

    if wait {
        let index_wait_started = Instant::now();
        let complete = daemon::wait_for_index_complete(&db_path, Duration::from_secs(120));
        info!(
            db = %db_path.display(),
            complete,
            elapsed_ms = index_wait_started.elapsed().as_millis() as u64,
            "index completion wait finished for search-file command"
        );
        if !complete {
            eprintln!("Timed out waiting for index to complete (120 s).");
        }
    }

    if !db_path.exists() {
        info!(
            db = %db_path.display(),
            elapsed_ms = command_started.elapsed().as_millis() as u64,
            "search-file command finished before database directory was created"
        );
        return Ok(());
    }

    let hits = match search_files_in_database(&db_path, &pattern) {
        Ok(h) => h,
        Err(err) => {
            error!(db = %db_path.display(), pattern = %pattern, error = ?err, "search-file command failed");
            std::process::exit(1);
        }
    };

    info!(
        db = %db_path.display(),
        pattern = %pattern,
        hits = hits.len(),
        elapsed_ms = command_started.elapsed().as_millis() as u64,
        "search-file command completed"
    );

    for hit in hits {
        println!("{}", clean_display_path(&hit.path));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Management commands
// ---------------------------------------------------------------------------

fn format_eta(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }

    let minutes = seconds / 60;
    let secs = seconds % 60;
    if minutes < 60 {
        return format!("{minutes}m {secs}s");
    }

    let hours = minutes / 60;
    let mins = minutes % 60;
    format!("{hours}h {mins}m")
}

fn estimate_eta_seconds(progress: &IndexProgress) -> Option<u64> {
    let started_at_ms = progress.started_at_ms?;
    let elapsed_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis()
        .saturating_sub(started_at_ms as u128) as u64;

    let total_bytes = progress.total_bytes?;
    if total_bytes > 0 && progress.processed_bytes > 0 && progress.processed_bytes < total_bytes {
        let remaining_bytes = total_bytes.saturating_sub(progress.processed_bytes);
        let eta_ms = elapsed_ms
            .saturating_mul(remaining_bytes)
            .checked_div(progress.processed_bytes)?;
        return Some((eta_ms / 1000).max(1));
    }

    let total_files = progress.total_files?;
    if total_files > 0 && progress.processed_files > 0 && progress.processed_files < total_files {
        let remaining_files = total_files.saturating_sub(progress.processed_files);
        let eta_ms = elapsed_ms
            .saturating_mul(remaining_files as u64)
            .checked_div(progress.processed_files as u64)?;
        return Some((eta_ms / 1000).max(1));
    }

    None
}

fn format_remaining_lease(expires_at_ms: i64) -> Option<String> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    let remaining_ms = expires_at_ms.saturating_sub(now_ms);
    if remaining_ms <= 0 {
        return Some("expired".to_string());
    }
    Some(format_eta((remaining_ms as u64).div_ceil(1000)))
}

pub async fn run_stop(
    root: Option<PathBuf>,
    db: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = root.unwrap_or_else(default_root);
    let db_path = db.unwrap_or_else(|| default_db_path(&root));
    info!(root = %root.display(), db = %db_path.display(), "stop command requested");
    daemon::stop_daemon(&db_path)?;
    println!("Stop requested for {}", root.display());
    Ok(())
}

pub async fn run_stop_all() -> Result<(), Box<dyn std::error::Error>> {
    let daemons = daemon::list_all_daemons()?;
    if daemons.is_empty() {
        println!("No running daemons found.");
        return Ok(());
    }
    for info in &daemons {
        let db = info
            .root
            .join(".source_fast")
            .join("index.mdb");
        daemon::stop_daemon(&db)?;
        println!("Stop requested for {}", info.root.display());
    }
    Ok(())
}

pub async fn run_status(
    root: Option<PathBuf>,
    db: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = root.unwrap_or_else(default_root);
    let db_path = db.unwrap_or_else(|| default_db_path(&root));
    info!(root = %root.display(), db = %db_path.display(), "status command requested");

    match daemon::daemon_status(&db_path)? {
        Some(info) => {
            debug!(
                root = %info.root.display(),
                pid = ?info.pid,
                version = ?info.version,
                index_status = ?info.index_status,
                leader_holder = ?info.leader_holder,
                "status command loaded daemon info"
            );
            println!("Root:         {}", info.root.display());
            println!(
                "PID:          {}",
                info.pid
                    .map_or("unknown".to_string(), |p| p.to_string())
            );
            println!(
                "Version:      {}",
                info.version.unwrap_or_else(|| "unknown".to_string())
            );
            println!(
                "Index status: {}",
                info.index_status.unwrap_or_else(|| "unknown".to_string())
            );
            if let Some(progress) = info.progress {
                if let Some(mode) = progress.mode.as_deref() {
                    println!("Scan mode:    {mode}");
                }
                match progress.total_files {
                    Some(total) => {
                        println!(
                            "Progress:     {}/{} files",
                            progress.processed_files, total
                        );
                    }
                    None => {
                        println!("Progress:     {} files", progress.processed_files);
                    }
                }
                if let Some(current) = progress.current_path.as_deref() {
                    println!("Processing:   {current}");
                }
                if let Some(last) = progress.last_completed_path.as_deref() {
                    println!("Last file:    {last}");
                }
                if progress.phase == "complete" {
                    println!("ETA:          done");
                } else if let Some(eta) = estimate_eta_seconds(&progress) {
                    println!("ETA:          {}", format_eta(eta));
                }
            }
            println!(
                "Leader:       {}",
                info.leader_holder.unwrap_or_else(|| "none".to_string())
            );
            if let Some(expires_at_ms) = info.leader_expires_ms {
                if let Some(remaining) = format_remaining_lease(expires_at_ms) {
                    println!("Lease TTL:    {remaining}");
                }
            }
        }
        None => {
            debug!(db = %db_path.display(), "status command found no daemon info");
            println!("No daemon running for {}", root.display());
        }
    }

    Ok(())
}

pub async fn run_list() -> Result<(), Box<dyn std::error::Error>> {
    let daemons = daemon::list_all_daemons()?;
    if daemons.is_empty() {
        println!("No running daemons found.");
        return Ok(());
    }

    for info in &daemons {
        println!(
            "{}\tPID={}\tindex={}\tversion={}",
            info.root.display(),
            info.pid
                .map_or("?".to_string(), |p| p.to_string()),
            info.index_status
                .as_deref()
                .unwrap_or("?"),
            info.version.as_deref().unwrap_or("?"),
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Index build & watch commands
// ---------------------------------------------------------------------------

pub async fn run_index_build(
    root: Option<PathBuf>,
    db: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = root.unwrap_or_else(default_root);
    let db_path = db.unwrap_or_else(|| default_db_path(&root));

    let was_running = daemon::ensure_daemon(&root, &db_path)?;
    if was_running {
        eprintln!("Daemon already running for {}", root.display());
    } else {
        eprintln!("Daemon started for {}", root.display());
    }

    if !daemon::wait_for_daemon(&db_path, Duration::from_secs(5)) {
        eprintln!("Warning: daemon did not confirm in 5 s");
    }

    eprintln!("Index building in background. Use `sf index watch` to monitor progress.");
    Ok(())
}

pub async fn run_index_watch(
    root: Option<PathBuf>,
    db: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = root.unwrap_or_else(default_root);
    let db_path = db.unwrap_or_else(|| default_db_path(&root));

    if !db_path.exists() {
        eprintln!("No index found at {}. Run `sf index build` first.", db_path.display());
        return Ok(());
    }

    let poll_interval = Duration::from_millis(100);
    let mut last_line_len = 0usize;

    loop {
        let status = read_meta_readonly(&db_path, daemon::meta_keys::INDEX_STATUS)
            .ok()
            .flatten()
            .unwrap_or_default();

        let progress = read_meta_readonly(&db_path, daemon::meta_keys::INDEX_PROGRESS)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<IndexProgress>(&json).ok());

        let line = match &progress {
            Some(p) => format_progress_line(p, &status),
            None if status == "complete" => "\x1b[32m✓ Index complete.\x1b[0m".to_string(),
            None if status == "failed" => "\x1b[31m✗ Index build failed.\x1b[0m".to_string(),
            None => "Waiting for daemon...".to_string(),
        };

        // Overwrite the current line.
        let padding = if line.len() < last_line_len {
            " ".repeat(last_line_len - line.len())
        } else {
            String::new()
        };
        eprint!("\r{line}{padding}");
        last_line_len = line.len();

        if status == "complete" || status == "failed" {
            eprintln!();
            break;
        }

        std::thread::sleep(poll_interval);
    }

    Ok(())
}

fn format_progress_line(p: &IndexProgress, status: &str) -> String {
    let mode = p.mode.as_deref().unwrap_or("scanning");

    let files_part = match p.total_files {
        Some(total) if total > 0 => {
            let pct = (p.processed_files as f64 / total as f64 * 100.0).min(100.0);
            format!("{}/{} files ({pct:.0}%)", p.processed_files, total)
        }
        _ => format!("{} files", p.processed_files),
    };

    let bytes_part = match p.total_bytes {
        Some(total) if total > 0 => {
            format!(" {}/{}", format_bytes(p.processed_bytes), format_bytes(total))
        }
        _ if p.processed_bytes > 0 => format!(" {}", format_bytes(p.processed_bytes)),
        _ => String::new(),
    };

    let bar = match p.total_files {
        Some(total) if total > 0 => {
            let ratio = (p.processed_files as f64 / total as f64).min(1.0);
            let width = 30;
            let filled = (ratio * width as f64) as usize;
            let empty = width - filled;
            format!(" [{}{}]", "█".repeat(filled), "░".repeat(empty))
        }
        _ => String::new(),
    };

    let file_name = p
        .current_path
        .as_deref()
        .or(p.last_completed_path.as_deref())
        .map(|path| {
            let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
            if name.len() > 40 {
                format!("{}...", &name[..37])
            } else {
                name.to_string()
            }
        })
        .unwrap_or_default();

    let eta = if status == "complete" {
        " done".to_string()
    } else {
        match estimate_eta_seconds(p) {
            Some(secs) => format!(" ETA {}", format_eta(secs)),
            None => String::new(),
        }
    };

    format!(
        "\x1b[36m{mode}\x1b[0m{bar} {files_part}{bytes_part}{eta} \x1b[2m{file_name}\x1b[0m"
    )
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}
