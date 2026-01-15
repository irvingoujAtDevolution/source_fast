use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use source_fast_fs::{background_watcher, smart_scan};
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
use source_fast_core::PersistentIndex;
use tokio::task;
use tracing::{error, info, warn};

use crate::cli::{default_db_path, default_root, open_index_with_worktree_copy};

#[derive(Clone)]
pub struct SearchServer {
    index: Arc<PersistentIndex>,
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
    pub query: String,
    #[serde(default)]
    pub file_regex: Option<String>,
}

#[tool_router]
impl SearchServer {
    pub fn new(index: Arc<PersistentIndex>, index_ready: Arc<AtomicBool>) -> Self {
        Self {
            index,
            index_ready,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Stateful code search over the current workspace using a persistent on-disk trigram index that is kept up-to-date with file changes. For large monorepos or huge codebases, prefer this tool over ad-hoc text search."
    )]
    pub async fn search_code(
        &self,
        Parameters(args): Parameters<SearchCodeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let index_building = !self.index_ready.load(Ordering::SeqCst);

        let query = args.query.clone();
        let query_for_search = query.clone();
        let index = Arc::clone(&self.index);
        let file_regex = args
            .file_regex
            .as_ref()
            .map(|pattern| {
                Regex::new(pattern)
                    .map_err(|e| Self::internal_error("invalid_file_regex", e.to_string()))
            })
            .transpose()?;

        let results = task::spawn_blocking(move || {
            index.search_with_snippets_filtered(&query_for_search, file_regex.as_ref())
        })
        .await
        .map_err(|e| Self::internal_error("search_task_failed", e.to_string()))?
        .map_err(|e| Self::internal_error("search_failed", e.to_string()))?;

        let mut contents = Vec::new();
        if index_building {
            contents.push(Content::text(
                "Warning (source_fast): index is still building.\n- Returned results come from the existing on-disk index and may be stale/incomplete vs the current working tree.\n- New/modified/deleted files since the index build started might be missing or still present.\n- Retry the same search in a few seconds for up-to-date results.\n"
                    .to_string(),
            ));
        }

        for result in results {
            let path = PathBuf::from(&result.path);

            if let Some(err) = result.snippet_error.as_ref() {
                warn!(path = %path.display(), error = %err, "Failed to extract snippet");
            }

            match result.snippet {
                Some(snippet) => {
                    let mut text =
                        format!("File: {}:{}\n", snippet.path.display(), snippet.line_number);
                    for (line_no, line) in snippet.lines {
                        text.push_str(&format!("{line_no}: {line}\n"));
                    }
                    contents.push(Content::text(text));
                }
                None => {
                    let text = format!("File: {}\n", path.display());
                    contents.push(Content::text(text));
                }
            }
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

pub async fn run_server(root: Option<PathBuf>, db: Option<PathBuf>) -> Result<(), Box<dyn Error>> {
    let root = root.unwrap_or_else(default_root);
    let db_path = db.unwrap_or_else(|| default_db_path(&root));

    info!("source_fast MCP server starting");
    info!("root: {}", root.display());
    info!("db: {}", db_path.display());

    let index = Arc::new(open_index_with_worktree_copy(&root, &db_path)?);
    let index_ready = Arc::new(AtomicBool::new(false));

    // Leader election: ensure only one process writes to the index at a time.
    // If we are not the writer, we still serve best-effort searches.
    let holder = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("pid:{}:{nanos}", std::process::id())
    };
    let lease_ttl = Duration::from_secs(5);
    let election_index = Arc::clone(&index);
    let election_root = root.clone();
    let election_ready = Arc::clone(&index_ready);
    let is_writer = Arc::new(AtomicBool::new(false));
    let is_writer_for_task = Arc::clone(&is_writer);

    task::spawn(async move {
        let mut role_logged: Option<&'static str> = None;
        let mut writer_started = false;

        loop {
            if !is_writer_for_task.load(Ordering::SeqCst) {
                let idx = Arc::clone(&election_index);
                let holder_clone = holder.clone();
                let acquired = match task::spawn_blocking(move || idx.try_acquire_writer_lease(&holder_clone, lease_ttl)).await
                {
                    Ok(Ok(v)) => v,
                    Ok(Err(err)) => {
                        warn!("leader_election: acquire failed: {err}");
                        false
                    }
                    Err(join_err) => {
                        warn!("leader_election: acquire task panicked: {join_err}");
                        false
                    }
                };

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
                    // Kick off initial indexing in the background so the MCP server can start
                    // responding to requests immediately.
                    let index_for_scan = Arc::clone(&election_index);
                    let root_for_scan = election_root.clone();
                    let ready_for_scan = Arc::clone(&election_ready);
                    task::spawn(async move {
                        let res =
                            task::spawn_blocking(move || smart_scan(&root_for_scan, index_for_scan))
                                .await;
                        match res {
                            Ok(Ok(())) => {
                                ready_for_scan.store(true, Ordering::SeqCst);
                                info!("MCP server: initial index build completed");
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
                    task::spawn(async move {
                        if let Err(err) = background_watcher(root_for_watcher, index_for_watcher).await {
                            error!("file watcher stopped: {err}");
                        }
                    });
                }

                // Renew lease.
                let idx = Arc::clone(&election_index);
                let holder_clone = holder.clone();
                let renewed = match task::spawn_blocking(move || idx.renew_writer_lease(&holder_clone, lease_ttl)).await
                {
                    Ok(Ok(v)) => v,
                    Ok(Err(err)) => {
                        warn!("leader_election: renew failed: {err}");
                        false
                    }
                    Err(join_err) => {
                        warn!("leader_election: renew task panicked: {join_err}");
                        false
                    }
                };

                if !renewed {
                    // Lost leadership; immediately disable writes and revert to reader.
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
    let server = SearchServer::new(index, index_ready);

    let service = server
        .serve(stdio())
        .await
        .inspect_err(|e| error!("source_fast MCP serve error: {e:?}"))?;

    service.waiting().await?;

    Ok(())
}
