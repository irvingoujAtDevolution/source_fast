use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use regex::Regex;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;
use source_fast_core::{IndexError, PersistentIndex, extract_snippets, path_is_within_root};
use source_fast_fs::{background_watcher_with_cancel, smart_scan_with_progress_cancel};
use source_fast_progress::ScanEvent;
use tokio::task;
use tracing::{error, info};

use crate::cli::{default_db_path, open_index_with_worktree_copy, resolve_root};

#[derive(Clone)]
pub struct SearchServer {
    index: Arc<PersistentIndex>,
    root: PathBuf,
    index_ready: Arc<AtomicBool>,
    tool_router: ToolRouter<SearchServer>,
}

impl SearchServer {
    fn internal_error(code: &str, message: impl Into<String>) -> McpError {
        let full = format!("{code}: {}", message.into());
        McpError::internal_error(full, None)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct SearchCodeArgs {
    /// Substring to search for (minimum 3 characters).
    pub query: String,
    /// Filter results by file extension (e.g. ["rs", "cs"]).
    #[serde(default)]
    pub ext: Vec<String>,
    /// Filter results by glob pattern (e.g. "*.rs").
    #[serde(default)]
    pub glob: Option<String>,
    /// Filter results by file path regex (advanced).
    #[serde(default)]
    pub file_regex: Option<String>,
    /// Return only file paths without snippets.
    #[serde(default)]
    pub files_only: bool,
    /// Return only the match count.
    #[serde(default)]
    pub count: bool,
    /// Maximum number of results (0 = unlimited, default 50).
    #[serde(default = "default_mcp_limit")]
    pub limit: usize,
}

fn default_mcp_limit() -> usize {
    50
}

#[tool_router]
impl SearchServer {
    pub fn new(index: Arc<PersistentIndex>, root: PathBuf, index_ready: Arc<AtomicBool>) -> Self {
        Self {
            index,
            root,
            index_ready,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Stateful code search over the current workspace using a persistent on-disk trigram index that is kept up-to-date with file changes. For large monorepos or huge codebases, prefer this tool over ad-hoc text search. Supports filtering by extension, glob, or regex. Returns snippets with context by default, or just file paths/count."
    )]
    pub async fn search_code(
        &self,
        Parameters(args): Parameters<SearchCodeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let index_building = !self.index_ready.load(Ordering::SeqCst);

        // Build file filter from ext, glob, or file_regex.
        let file_regex = build_mcp_file_filter(&args.file_regex, &args.ext, &args.glob)
            .map_err(|e| Self::internal_error("invalid_filter", e.to_string()))?;

        let query = args.query.clone();
        let index = Arc::clone(&self.index);
        let root = self.root.clone();
        let files_only = args.files_only;
        let count = args.count;
        let limit = if args.limit == 0 {
            usize::MAX
        } else {
            args.limit
        };

        let mut hits =
            task::spawn_blocking(move || index.search_filtered(&query, file_regex.as_ref()))
                .await
                .map_err(|e| Self::internal_error("search_task_failed", e.to_string()))?
                .map_err(|e| Self::internal_error("search_failed", e.to_string()))?;
        hits.retain(|hit| path_is_within_root(&hit.path, &root));

        let mut contents = Vec::new();
        if index_building {
            contents.push(Content::text(
                "Warning: index is still building. Results may be incomplete. Retry in a few seconds.\n"
                    .to_string(),
            ));
        }

        // --count mode
        if count {
            contents.push(Content::text(format!("{}", hits.len())));
            return Ok(CallToolResult::success(contents));
        }

        // --files-only mode
        if files_only {
            for (i, hit) in hits.iter().enumerate() {
                if i >= limit {
                    break;
                }
                contents.push(Content::text(format!("{}\n", clean_path(&hit.path))));
            }
            if hits.len() > limit {
                contents.push(Content::text(format!(
                    "... and {} more results\n",
                    hits.len() - limit
                )));
            }
            return Ok(CallToolResult::success(contents));
        }

        // Default: snippets with context
        let query_for_snippets = args.query.clone();
        for (i, hit) in hits.iter().enumerate() {
            if i >= limit {
                break;
            }
            let path = PathBuf::from(&hit.path);
            let display = clean_path(&hit.path);
            match extract_snippets(&path, &query_for_snippets) {
                Ok(snippets) if !snippets.is_empty() => {
                    let mut text = String::new();
                    for snippet in snippets {
                        text.push_str(&format!("{}:{}\n", display, snippet.line_number));
                        for (line_no, line) in &snippet.lines {
                            text.push_str(&format!("{line_no}: {line}\n"));
                        }
                        text.push('\n');
                    }
                    contents.push(Content::text(text));
                }
                _ => {
                    contents.push(Content::text(format!("{display}\n")));
                }
            }
        }

        if hits.len() > limit {
            contents.push(Content::text(format!(
                "... and {} more results\n",
                hits.len() - limit
            )));
        }

        Ok(CallToolResult::success(contents))
    }
}

#[tool_handler]
impl ServerHandler for SearchServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Stateful source code search server. It maintains a persistent trigram index on disk and keeps it in sync with file changes. For huge codebases or monorepos, prefer using the `search_code` tool first before falling back to raw text search."
                    .to_string(),
            ),
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
        }
    }
}

/// Strip the `\\?\` extended path prefix on Windows.
fn clean_path(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

/// Build a file-filter regex from MCP args (same logic as CLI).
fn build_mcp_file_filter(
    file_regex: &Option<String>,
    ext: &[String],
    glob: &Option<String>,
) -> Result<Option<Regex>, String> {
    if let Some(pattern) = file_regex {
        return Regex::new(pattern).map(Some).map_err(|e| e.to_string());
    }
    if !ext.is_empty() {
        let alts: Vec<String> = ext.iter().map(|e| regex::escape(e)).collect();
        let pattern = format!(r"\.({})$", alts.join("|"));
        return Regex::new(&pattern).map(Some).map_err(|e| e.to_string());
    }
    if let Some(g) = glob {
        let re_str = crate::cli::glob_to_regex(g);
        return Regex::new(&re_str).map(Some).map_err(|e| e.to_string());
    }
    Ok(None)
}

pub async fn run_server(root: Option<PathBuf>, db: Option<PathBuf>) -> Result<(), Box<dyn Error>> {
    let root = resolve_root(root);
    let db_path = db.unwrap_or_else(|| default_db_path(&root));

    info!("source_fast MCP server starting");
    info!("root: {}", root.display());
    info!("db: {}", db_path.display());

    let index = Arc::new(open_index_with_worktree_copy(&root, &db_path)?);
    let index_ready = Arc::new(AtomicBool::new(false));

    // Leader election: ensure only one process writes to the index at a time.
    // If we are not the writer, we still serve best-effort searches.
    let holder = crate::daemon::writer_holder_id();
    let holder_for_cleanup = holder.clone();
    let lease_ttl = Duration::from_secs(5);
    let election_index = Arc::clone(&index);
    let election_root = root.clone();
    let election_ready = Arc::clone(&index_ready);
    let is_writer = Arc::new(AtomicBool::new(false));
    let is_writer_for_task = Arc::clone(&is_writer);

    task::spawn(async move {
        let mut role_logged: Option<&'static str> = None;
        let mut writer_started = false;
        let mut writer_cancel: Option<Arc<AtomicBool>> = None;

        loop {
            if !is_writer_for_task.load(Ordering::SeqCst) {
                let acquired = crate::daemon::try_acquire_writer_lease(
                    Arc::clone(&election_index),
                    holder.clone(),
                    lease_ttl,
                    "mcp",
                )
                .await;

                if acquired {
                    election_index.set_write_enabled(true);
                    is_writer_for_task.store(true, Ordering::SeqCst);
                    role_logged = None;
                    info!(role = "writer", "promoted role=writer");
                } else {
                    election_index.set_write_enabled(false);
                    if role_logged != Some("reader") {
                        info!(role = "reader", "role selected role=reader");
                        role_logged = Some("reader");
                    }
                }
            }

            if is_writer_for_task.load(Ordering::SeqCst) {
                if role_logged != Some("writer") {
                    info!(role = "writer", "role selected role=writer");
                    role_logged = Some("writer");
                }

                if !writer_started {
                    writer_started = true;
                    let cancel = Arc::new(AtomicBool::new(false));
                    writer_cancel = Some(Arc::clone(&cancel));
                    // Kick off initial indexing in the background so the MCP server can start
                    // responding to requests immediately.
                    let index_for_scan = Arc::clone(&election_index);
                    let root_for_scan = election_root.clone();
                    let ready_for_scan = Arc::clone(&election_ready);
                    let cancel_for_scan = Arc::clone(&cancel);
                    task::spawn(async move {
                        let res = task::spawn_blocking(move || {
                            smart_scan_with_progress_cancel(
                                &root_for_scan,
                                index_for_scan,
                                Arc::new(|_: ScanEvent| {}),
                                cancel_for_scan,
                            )
                        })
                        .await;
                        match res {
                            Ok(Ok(())) => {
                                ready_for_scan.store(true, Ordering::SeqCst);
                                info!("MCP server: initial index build completed");
                            }
                            Ok(Err(IndexError::Cancelled)) => {
                                info!("MCP server: initial index build cancelled");
                            }
                            Ok(Err(err)) => {
                                error!("MCP server: initial index build failed: {err}");
                            }
                            Err(join_err) => {
                                error!("MCP server: initial index task panicked: {join_err}");
                            }
                        }
                    });

                    // Start background file watcher to keep the index up-to-date.
                    let index_for_watcher = Arc::clone(&election_index);
                    let root_for_watcher = election_root.clone();
                    let cancel_for_watcher = Arc::clone(&cancel);
                    task::spawn(async move {
                        if let Err(err) = background_watcher_with_cancel(
                            root_for_watcher,
                            index_for_watcher,
                            cancel_for_watcher,
                        )
                        .await
                        {
                            error!("file watcher stopped: {err}");
                        }
                    });
                }

                // Renew lease.
                let renewed = crate::daemon::renew_writer_lease(
                    Arc::clone(&election_index),
                    holder.clone(),
                    lease_ttl,
                    "mcp",
                )
                .await;

                if !renewed {
                    // Lost leadership; immediately disable writes and revert to reader.
                    if let Some(cancel) = writer_cancel.take() {
                        cancel.store(true, Ordering::SeqCst);
                    }
                    election_index.set_write_enabled(false);
                    is_writer_for_task.store(false, Ordering::SeqCst);
                    writer_started = false;
                    election_ready.store(false, Ordering::SeqCst);
                    role_logged = None;
                    info!(role = "reader", "demoted role=reader");
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });

    // Start rmcp-based MCP server on stdio.
    let server = SearchServer::new(index.clone(), root.clone(), index_ready);

    let service = server
        .serve(stdio())
        .await
        .inspect_err(|e| error!("source_fast MCP serve error: {e:?}"))?;

    service.waiting().await?;

    // Release the writer lease so other processes can acquire it immediately.
    let _ = index.release_writer_lease(&holder_for_cleanup);
    info!("MCP server shut down, writer lease released");

    Ok(())
}
