use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs_layer::smart_scan;
use source_fast_core::{PersistentIndex, extract_snippet, search_database_file};
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

    let hits = match search_database_file(&db_path, &query) {
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

    let index = match PersistentIndex::open_or_create(&db_path) {
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
