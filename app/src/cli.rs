use std::path::{Path, PathBuf};
use std::sync::Arc;

use source_fast_fs::smart_scan;
use regex::Regex;
use source_fast_core::{
    IndexError, PersistentIndex, extract_snippet, rewrite_root_paths,
    search_database_file_filtered, search_files_in_database,
};
use tracing::{error, info, warn};

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

/// Initialize tracing for CLI commands (index/search).
///
/// Logs go to stderr, and respect RUST_LOG or default to `info`.
pub fn init_tracing_cli() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt().with_env_filter(filter).with_target(false).init();
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
        _ => {
            // No log path configured -> do not install any subscriber.
            return;
        }
    };

    // Try a first open to validate the path. If it fails, we simply
    // disable logging rather than panicking or printing to stdout/stderr.
    if OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .is_err()
    {
        return;
    }

    // Re-open the file on each log write. This keeps the MakeWriter simple
    // and avoids sharing mutable state across threads.
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

pub async fn run_cli(
    root: Option<PathBuf>,
    db: Option<PathBuf>,
    query: String,
    file_regex: Option<String>,
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

    if !db_path.exists() {
        error!(
            "Index database not found at {}. Run `sf index --root <root>` to build the index.",
            db_path.display()
        );
        std::process::exit(1);
    }

    let hits = match search_database_file_filtered(&db_path, &query, file_regex.as_ref()) {
        Ok(h) => h,
        Err(err) => {
            error!("Search failed: {:?}", err);
            std::process::exit(1);
        }
    };

    for hit in hits {
        let path = PathBuf::from(&hit.path);
        match extract_snippet(&path, &query) {
            Ok(Some(snippet)) => {
                println!("File: {}:{}", snippet.path.display(), snippet.line_number);
                for (line_no, line) in snippet.lines {
                    println!("{line_no}: {line}");
                }
                println!();
            }
            Ok(None) => {
                println!("File: {}", path.display());
            }
            Err(err) => {
                warn!("Failed to extract snippet from {}: {err}", path.display());
            }
        }
    }

    Ok(())
}

pub async fn run_file_search(
    root: Option<PathBuf>,
    db: Option<PathBuf>,
    pattern: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = root.unwrap_or_else(default_root);
    let db_path = db.unwrap_or_else(|| default_db_path(&root));

    if !db_path.exists() {
        error!(
            "Index database not found at {}. Run `sf index --root <root>` to build the index.",
            db_path.display()
        );
        std::process::exit(1);
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

pub async fn run_index_only(
    root: Option<PathBuf>,
    db: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = root.unwrap_or_else(default_root);
    let db_path = db.unwrap_or_else(|| default_db_path(&root));

    info!("Building index for {}", root.display());
    info!("Database path: {}", db_path.display());

    if let Some(parent) = db_path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        error!(
            "Failed to create database directory {}: {}",
            parent.display(),
            err
        );
        std::process::exit(1);
    }

    let index = match open_index_with_worktree_copy(&root, &db_path) {
        Ok(idx) => Arc::new(idx),
        Err(err) => {
            error!("Failed to open index database: {}", err);
            std::process::exit(1);
        }
    };

    if let Err(err) = smart_scan(&root, Arc::clone(&index)) {
        error!("Indexing failed: {}", err);
        std::process::exit(1);
    }

    info!("Index build completed");
    Ok(())
}
