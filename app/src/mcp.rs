use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
use source_fast_core::{PersistentIndex, extract_snippet};
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
        if !self.index_ready.load(Ordering::SeqCst) {
            return Err(Self::internal_error(
                "index_building",
                "Index is still building; try again shortly",
            ));
        }

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

        let hits = task::spawn_blocking(move || {
            index.search_filtered(&query_for_search, file_regex.as_ref())
        })
        .await
        .map_err(|e| Self::internal_error("search_task_failed", e.to_string()))?
        .map_err(|e| Self::internal_error("search_failed", e.to_string()))?;

        let mut contents = Vec::new();
        for hit in hits {
            let path = PathBuf::from(&hit.path);
            match extract_snippet(&path, &query) {
                Ok(Some(snippet)) => {
                    let mut text =
                        format!("File: {}:{}\n", snippet.path.display(), snippet.line_number);
                    for (line_no, line) in snippet.lines {
                        text.push_str(&format!("{line_no}: {line}\n"));
                    }
                    contents.push(Content::text(text));
                }
                Ok(None) => {
                    let text = format!("File: {}\n", path.display());
                    contents.push(Content::text(text));
                }
                Err(err) => {
                    warn!("Failed to extract snippet from {}: {err}", path.display());
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

    // Kick off initial indexing in the background so the MCP server can start
    // responding to requests immediately.
    let index_for_scan = Arc::clone(&index);
    let root_for_scan = root.clone();
    let ready_for_scan = Arc::clone(&index_ready);
    task::spawn(async move {
        let res = task::spawn_blocking(move || smart_scan(&root_for_scan, index_for_scan)).await;
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
    let index_for_watcher = Arc::clone(&index);
    let root_for_watcher = root.clone();
    task::spawn(async move {
        if let Err(err) = background_watcher(root_for_watcher, index_for_watcher).await {
            error!("file watcher stopped: {err}");
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
