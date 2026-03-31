use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use regex::Regex;
use source_fast_core::{
    IndexError, PersistentIndex, extract_snippet, is_leader_active_readonly, now_millis,
    read_meta_readonly, rewrite_root_paths, search_database_file_filtered,
    search_files_in_database,
};
use source_fast_fs::smart_scan_with_progress;
use source_fast_progress::{IndexProgress, ScanEvent};
use tokio::task;
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

struct WatchState {
    processed_files: AtomicUsize,
    total_files: AtomicUsize,
    processed_bytes: AtomicU64,
    total_bytes: AtomicU64,
    phase: Mutex<String>,
    current_file: Mutex<String>,
    mode: Mutex<String>,
    started_at_ms: AtomicU64,
}

#[derive(Clone, Debug)]
struct WatchSnapshot {
    processed_files: usize,
    total_files: Option<usize>,
    processed_bytes: u64,
    total_bytes: Option<u64>,
    phase: String,
    current_file: String,
    mode: String,
    started_at_ms: Option<u64>,
}

impl WatchState {
    fn new() -> Self {
        Self {
            processed_files: AtomicUsize::new(0),
            total_files: AtomicUsize::new(0),
            processed_bytes: AtomicU64::new(0),
            total_bytes: AtomicU64::new(0),
            phase: Mutex::new("building".to_string()),
            current_file: Mutex::new(String::new()),
            mode: Mutex::new("scanning".to_string()),
            started_at_ms: AtomicU64::new(0),
        }
    }

    fn apply_event(&self, event: ScanEvent) {
        match event {
            ScanEvent::Started(plan) => {
                self.processed_files.store(0, Ordering::Relaxed);
                self.total_files.store(plan.total_files, Ordering::Relaxed);
                self.processed_bytes.store(0, Ordering::Relaxed);
                self.total_bytes.store(plan.total_bytes, Ordering::Relaxed);
                self.started_at_ms.store(now_ms(), Ordering::Relaxed);
                *self.phase.lock().unwrap() = "building".to_string();
                *self.mode.lock().unwrap() = plan.mode.as_str().to_string();
                self.current_file.lock().unwrap().clear();
            }
            ScanEvent::FileStarted(path) => {
                *self.current_file.lock().unwrap() = path;
            }
            ScanEvent::FileFinished { path, bytes } => {
                self.processed_files.fetch_add(1, Ordering::Relaxed);
                self.processed_bytes.fetch_add(bytes, Ordering::Relaxed);
                *self.current_file.lock().unwrap() = path;
            }
            ScanEvent::Finished => {
                *self.phase.lock().unwrap() = "complete".to_string();
            }
            ScanEvent::Failed => {
                *self.phase.lock().unwrap() = "failed".to_string();
            }
        }
    }

    fn set_phase(&self, phase: &str) {
        *self.phase.lock().unwrap() = phase.to_string();
    }

    fn snapshot(&self) -> WatchSnapshot {
        let total_files = self.total_files.load(Ordering::Relaxed);
        let total_bytes = self.total_bytes.load(Ordering::Relaxed);
        let started_at_ms = self.started_at_ms.load(Ordering::Relaxed);

        WatchSnapshot {
            processed_files: self.processed_files.load(Ordering::Relaxed),
            total_files: (total_files > 0).then_some(total_files),
            processed_bytes: self.processed_bytes.load(Ordering::Relaxed),
            total_bytes: (total_bytes > 0).then_some(total_bytes),
            phase: self.phase.lock().unwrap().clone(),
            current_file: self.current_file.lock().unwrap().clone(),
            mode: self.mode.lock().unwrap().clone(),
            started_at_ms: (started_at_ms > 0).then_some(started_at_ms),
        }
    }
}

fn watch_snapshot_to_progress(snapshot: &WatchSnapshot) -> IndexProgress {
    IndexProgress {
        phase: snapshot.phase.clone(),
        mode: Some(snapshot.mode.clone()),
        started_at_ms: snapshot.started_at_ms,
        processed_files: snapshot.processed_files,
        total_files: snapshot.total_files,
        processed_bytes: snapshot.processed_bytes,
        total_bytes: snapshot.total_bytes,
        current_path: (!snapshot.current_file.is_empty()).then(|| snapshot.current_file.clone()),
        last_completed_path: (!snapshot.current_file.is_empty())
            .then(|| snapshot.current_file.clone()),
    }
}

fn queue_progress_meta(index: &PersistentIndex, progress: &IndexProgress) {
    if let Ok(json) = serde_json::to_string(progress) {
        let _ = index.set_meta_queued(daemon::meta_keys::INDEX_PROGRESS, &json);
    }
}

fn now_ms() -> u64 {
    now_millis().max(0) as u64
}

fn best_effort_stop_daemon(db_path: &Path) {
    if !db_path.exists() {
        return;
    }

    if !is_leader_active_readonly(db_path).unwrap_or(false) {
        return;
    }

    if let Err(err) = daemon::stop_daemon(db_path) {
        warn!(db = %db_path.display(), error = ?err, "failed to request daemon shutdown before foreground watch");
        return;
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !is_leader_active_readonly(db_path).unwrap_or(true) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn render_watch_frame(snapshot: &WatchSnapshot, tick: usize) -> (String, String) {
    const SPINNER: [char; 4] = ['|', '/', '-', '\\'];

    let progress = watch_snapshot_to_progress(snapshot);
    let bar = match snapshot.total_files {
        Some(total) if total > 0 => {
            let ratio = (snapshot.processed_files as f64 / total as f64).min(1.0);
            let width = 30usize;
            let filled = (ratio * width as f64) as usize;
            let empty = width.saturating_sub(filled);
            format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
        }
        _ => "[██████████████████████████████]".to_string(),
    };

    let files_part = match snapshot.total_files {
        Some(total) if total > 0 => {
            let pct = (snapshot.processed_files as f64 / total as f64 * 100.0).min(100.0);
            format!("{}/{} ({pct:.0}%)", snapshot.processed_files, total)
        }
        _ => format!("{} files", snapshot.processed_files),
    };

    let bytes_part = match snapshot.total_bytes {
        Some(total) if total > 0 => {
            format!(
                "{}/{}",
                format_bytes(snapshot.processed_bytes),
                format_bytes(total)
            )
        }
        _ => format_bytes(snapshot.processed_bytes),
    };

    let eta_part = if snapshot.phase == "complete" {
        "ETA done".to_string()
    } else if snapshot.phase == "failed" {
        "ETA failed".to_string()
    } else {
        estimate_eta_seconds(&progress)
            .map(|secs| format!("ETA {}", format_eta(secs)))
            .unwrap_or_else(|| "ETA --".to_string())
    };

    let throughput = snapshot
        .started_at_ms
        .and_then(|started_at_ms| {
            let elapsed_ms = now_ms().saturating_sub(started_at_ms);
            (elapsed_ms > 0)
                .then_some(snapshot.processed_files as f64 / (elapsed_ms as f64 / 1000.0))
        })
        .unwrap_or(0.0);

    let spinner = if snapshot.phase == "building" {
        SPINNER[(tick / 6) % SPINNER.len()]
    } else {
        ' '
    };

    let headline = format!(
        "\x1b[36m{} {}\x1b[0m {} {}  {}  {}  {:.0} files/sec",
        spinner, snapshot.mode, bar, files_part, bytes_part, eta_part, throughput
    );

    let file_name = if snapshot.current_file.is_empty() {
        "waiting for files...".to_string()
    } else {
        let display = clean_display_path(&snapshot.current_file);
        let name = display.rsplit(['/', '\\']).next().unwrap_or(display);
        truncate_line(name, 80)
    };
    let detail = format!("\x1b[2m  {file_name}\x1b[0m");

    (headline, detail)
}

fn print_watch_frame(lines: &(String, String), first_frame: bool) {
    if first_frame {
        eprint!("\x1b[2K{}\n\x1b[2K{}", lines.0, lines.1);
    } else {
        eprint!("\r\x1b[1A\x1b[2K{}\n\x1b[2K{}", lines.0, lines.1);
    }
    let _ = io::stderr().flush();
}

fn print_watch_summary(snapshot: &WatchSnapshot) {
    let elapsed_secs = snapshot
        .started_at_ms
        .map(|started_at_ms| now_ms().saturating_sub(started_at_ms).div_ceil(1000))
        .unwrap_or(0);
    let rate = if elapsed_secs > 0 {
        snapshot.processed_files as u64 / elapsed_secs.max(1)
    } else {
        0
    };
    let status = if snapshot.phase == "complete" {
        "✓"
    } else {
        "✗"
    };
    let summary = format!(
        "{status} Indexed {} files ({}) in {} {}",
        snapshot.processed_files,
        format_bytes(snapshot.processed_bytes),
        format_eta(elapsed_secs),
        if snapshot.phase == "complete" {
            format!("{} files/sec", rate)
        } else {
            "before failing".to_string()
        }
    );
    eprint!("\r\x1b[1A\x1b[2K{summary}\n\x1b[2K\n");
}

fn watch_progress_polling(db_path: &Path) {
    use source_fast_core::storage::open_readonly_env;

    let poll_interval = Duration::from_millis(50);
    let mut last_line_len = 0usize;

    // Open the LMDB env once and reuse it for all polls.
    // This avoids re-mapping 1 GB of virtual memory and re-acquiring the
    // write lock to open named databases on every iteration.
    let env_and_dbs = open_readonly_env(db_path);
    let (env, dbs) = match env_and_dbs {
        Ok(ed) => ed,
        Err(_) => {
            eprintln!("No index is being built.");
            return;
        }
    };

    loop {
        let (status, progress) = {
            let Ok(rtxn) = env.read_txn() else {
                std::thread::sleep(poll_interval);
                continue;
            };
            let status = dbs
                .meta
                .get(&rtxn, daemon::meta_keys::INDEX_STATUS)
                .ok()
                .flatten()
                .map(str::to_string)
                .unwrap_or_default();
            let progress = dbs
                .meta
                .get(&rtxn, daemon::meta_keys::INDEX_PROGRESS)
                .ok()
                .flatten()
                .and_then(|json| serde_json::from_str::<IndexProgress>(json).ok());
            drop(rtxn);
            (status, progress)
        };

        let line = match &progress {
            Some(p) => format_progress_line(p, &status),
            None if status == "complete" => "\x1b[32m✓ Index complete.\x1b[0m".to_string(),
            None if status == "failed" => "\x1b[31m✗ Index build failed.\x1b[0m".to_string(),
            _ if status.is_empty() || status == "building" => "Waiting for daemon...".to_string(),
            None => "No index is being built.".to_string(),
        };

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
    let primary_root = primary_worktree_root(root)?;

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
        eprintln!(
            "Starting index for the first time. Results will be partial until indexing completes."
        );
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
            let snippet = extract_snippet(&path, &query_for_workers).ok().flatten();
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
        eprintln!(
            "Starting index for the first time. Results will be partial until indexing completes."
        );
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
        let db = info.root.join(".source_fast").join("index.mdb");
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
                info.pid.map_or("unknown".to_string(), |p| p.to_string())
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
                        println!("Progress:     {}/{} files", progress.processed_files, total);
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
            if let Some(expires_at_ms) = info.leader_expires_ms
                && let Some(remaining) = format_remaining_lease(expires_at_ms)
            {
                println!("Lease TTL:    {remaining}");
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
            info.pid.map_or("?".to_string(), |p| p.to_string()),
            info.index_status.as_deref().unwrap_or("?"),
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
        let created = open_index_with_worktree_copy(&root, &db_path)?;
        drop(created);
    }

    best_effort_stop_daemon(&db_path);

    let index = Arc::new(open_index_with_worktree_copy(&root, &db_path)?);
    let holder = {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("watch:{}:{nanos}", std::process::id())
    };
    let lease_ttl = Duration::from_secs(5);

    let acquired = {
        let index = Arc::clone(&index);
        let holder = holder.clone();
        task::spawn_blocking(move || index.try_acquire_writer_lease(&holder, lease_ttl)).await??
    };

    if !acquired {
        eprintln!("Another writer is active. Attaching to persisted progress...");
        watch_progress_polling(&db_path);
        return Ok(());
    }

    index.set_write_enabled(true);
    let _ = index.set_meta_queued(
        daemon::meta_keys::INDEX_STATUS,
        daemon::index_status::BUILDING,
    );

    let state = Arc::new(WatchState::new());
    let initial_progress = watch_snapshot_to_progress(&state.snapshot());
    queue_progress_meta(&index, &initial_progress);
    let _ = index.flush();

    let render_state = Arc::clone(&state);
    let render_handle = std::thread::spawn(move || {
        let mut first_frame = true;
        let mut tick = 0usize;
        loop {
            let snapshot = render_state.snapshot();
            let frame = render_watch_frame(&snapshot, tick);
            print_watch_frame(&frame, first_frame);
            first_frame = false;
            tick = tick.wrapping_add(1);

            if snapshot.phase == "complete" || snapshot.phase == "failed" {
                break;
            }

            std::thread::sleep(Duration::from_millis(16));
        }
    });

    let renew_index = Arc::clone(&index);
    let renew_holder = holder.clone();
    let renew_failed = Arc::new(AtomicUsize::new(0));
    let renew_failed_for_thread = Arc::clone(&renew_failed);
    let renew_handle = std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(2));
            if matches!(
                renew_index.get_meta(daemon::meta_keys::INDEX_STATUS),
                Ok(Some(ref status)) if status == "complete" || status == "failed"
            ) {
                break;
            }

            match renew_index.renew_writer_lease(&renew_holder, lease_ttl) {
                Ok(true) => {}
                Ok(false) | Err(_) => {
                    renew_index.set_write_enabled(false);
                    renew_failed_for_thread.store(1, Ordering::SeqCst);
                    break;
                }
            }
        }
    });

    let callback_state = Arc::clone(&state);
    let progress_callback: Arc<dyn Fn(ScanEvent) + Send + Sync> = Arc::new(move |event| {
        callback_state.apply_event(event);
    });

    let scan_result = {
        let scan_root = root.clone();
        let scan_index = Arc::clone(&index);
        task::spawn_blocking(move || {
            smart_scan_with_progress(&scan_root, scan_index, progress_callback)
        })
        .await?
    };

    if renew_failed.load(Ordering::SeqCst) != 0 {
        state.set_phase("failed");
    } else if scan_result.is_ok() {
        state.set_phase("complete");
    } else {
        state.set_phase("failed");
    }

    let final_snapshot = state.snapshot();
    let final_progress = watch_snapshot_to_progress(&final_snapshot);
    queue_progress_meta(&index, &final_progress);
    let final_status = if final_snapshot.phase == "complete" {
        daemon::index_status::COMPLETE
    } else {
        "failed"
    };
    let _ = index.set_meta_queued(daemon::meta_keys::INDEX_STATUS, final_status);
    let _ = index.flush();

    let _ = renew_handle.join();
    let _ = render_handle.join();
    print_watch_summary(&final_snapshot);

    index.set_write_enabled(false);
    let _ = index.release_writer_lease(&holder);

    if renew_failed.load(Ordering::SeqCst) != 0 {
        return Err("foreground watch lost the writer lease before completion".into());
    }

    scan_result?;
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
            format!(
                " {}/{}",
                format_bytes(p.processed_bytes),
                format_bytes(total)
            )
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

    format!("\x1b[36m{mode}\x1b[0m{bar} {files_part}{bytes_part}{eta} \x1b[2m{file_name}\x1b[0m")
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
