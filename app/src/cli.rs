use std::path::{Path, PathBuf};
use std::time::Duration;

use regex::Regex;
use source_fast_core::{
    IndexError, PersistentIndex, rewrite_root_paths, search_database_file_with_snippets_filtered,
    search_files_in_database,
};
use tracing::{error, warn};

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
    dir.push("index.db");
    dir
}

// ---------------------------------------------------------------------------
// DB helpers (shared with daemon)
// ---------------------------------------------------------------------------

fn remove_db_files(db_path: &Path) {
    let wal = db_path.with_extension("db-wal");
    let shm = db_path.with_extension("db-shm");
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(wal);
    let _ = std::fs::remove_file(shm);
}

fn is_corrupt_db(err: &IndexError) -> bool {
    match err {
        IndexError::Db(db_err) => db_err.to_string().contains("file is not a database"),
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

fn copy_db_from_root(source_root: &Path, db_path: &Path) -> std::io::Result<bool> {
    let source_db = source_root.join(".source_fast").join("index.db");
    if !source_db.exists() {
        return Ok(false);
    }

    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::copy(&source_db, db_path)?;

    let source_wal = source_db.with_extension("db-wal");
    let source_shm = source_db.with_extension("db-shm");
    let dest_wal = db_path.with_extension("db-wal");
    let dest_shm = db_path.with_extension("db-shm");

    if source_wal.exists() {
        let _ = std::fs::copy(&source_wal, &dest_wal);
    }
    if source_shm.exists() {
        let _ = std::fs::copy(&source_shm, &dest_shm);
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
/// Logs go to stderr, and respect RUST_LOG or default to `info`.
pub fn init_tracing_cli() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
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
) -> Result<(), Box<dyn std::error::Error>> {
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

    // Ensure a daemon (or MCP server) is keeping the index warm.
    let was_running = daemon::ensure_daemon(&root, &db_path)?;

    if first_time {
        eprintln!("Starting index for the first time. Results will be partial until indexing completes.");
    }

    if !was_running {
        let confirmed = daemon::wait_for_daemon(&db_path, Duration::from_secs(3));
        if !confirmed {
            warn!("Daemon did not confirm in 3 s, proceeding with search anyway");
        }
    }

    // If --wait, block until index is complete.
    if wait {
        if !daemon::wait_for_index_complete(&db_path, Duration::from_secs(120)) {
            eprintln!("Timed out waiting for index to complete (120 s).");
        }
    }

    if !db_path.exists() {
        // DB hasn't been created yet (daemon just started). Nothing to search.
        return Ok(());
    }

    // Check completeness for the disclaimer.
    let index = PersistentIndex::open_or_create(&db_path)?;
    if let Ok(Some(status)) = index.get_meta(daemon::meta_keys::INDEX_STATUS) {
        if status != daemon::index_status::COMPLETE {
            eprintln!("Note: index is still building. Results may be incomplete.");
        }
    }
    drop(index);

    // Execute the search.
    let results =
        match search_database_file_with_snippets_filtered(&db_path, &query, file_regex.as_ref()) {
            Ok(r) => r,
            Err(err) => {
                error!("Search failed: {:?}", err);
                std::process::exit(1);
            }
        };

    for result in results {
        let path = PathBuf::from(&result.path);

        if let Some(err) = result.snippet_error.as_ref() {
            warn!(path = %path.display(), error = %err, "Failed to extract snippet");
        }

        match result.snippet {
            Some(snippet) => {
                println!("File: {}:{}", snippet.path.display(), snippet.line_number);
                for (line_no, line) in snippet.lines {
                    println!("{line_no}: {line}");
                }
                println!();
            }
            None => println!("File: {}", path.display()),
        }
    }

    Ok(())
}

pub async fn run_file_search_with_daemon(
    root: Option<PathBuf>,
    db: Option<PathBuf>,
    pattern: String,
    wait: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = root.unwrap_or_else(default_root);
    let db_path = db.unwrap_or_else(|| default_db_path(&root));

    let first_time = !db_path.exists();
    let was_running = daemon::ensure_daemon(&root, &db_path)?;

    if first_time {
        eprintln!("Starting index for the first time. Results will be partial until indexing completes.");
    }

    if !was_running {
        let confirmed = daemon::wait_for_daemon(&db_path, Duration::from_secs(3));
        if !confirmed {
            warn!("Daemon did not confirm in 3 s, proceeding with search anyway");
        }
    }

    if wait {
        if !daemon::wait_for_index_complete(&db_path, Duration::from_secs(120)) {
            eprintln!("Timed out waiting for index to complete (120 s).");
        }
    }

    if !db_path.exists() {
        return Ok(());
    }

    let hits = match search_files_in_database(&db_path, &pattern) {
        Ok(h) => h,
        Err(err) => {
            error!("File search failed: {:?}", err);
            std::process::exit(1);
        }
    };

    for hit in hits {
        println!("{}", hit.path);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Management commands
// ---------------------------------------------------------------------------

pub async fn run_stop(
    root: Option<PathBuf>,
    db: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = root.unwrap_or_else(default_root);
    let db_path = db.unwrap_or_else(|| default_db_path(&root));
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
            .join("index.db");
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

    match daemon::daemon_status(&db_path)? {
        Some(info) => {
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
            println!(
                "Leader:       {}",
                info.leader_holder.unwrap_or_else(|| "none".to_string())
            );
        }
        None => {
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
