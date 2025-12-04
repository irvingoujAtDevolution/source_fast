use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use fs_layer::{background_watcher, initial_scan};
use source_fast_core::{PersistentIndex, extract_snippet, search_database_file};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{error, info, warn};

#[derive(Subcommand, Debug)]
enum Command {
    /// Build or update the index, then exit
    Index {
        /// Root directory to index
        #[arg(long)]
        root: Option<PathBuf>,
        /// Path to database file
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Search using an existing index
    Search {
        /// Root directory to search
        #[arg(long)]
        root: Option<PathBuf>,
        /// Path to database file
        #[arg(long)]
        db: Option<PathBuf>,
        /// Search query
        query: String,
    },
    /// Run MCP server over stdio
    Server {
        /// Root directory to index and watch
        #[arg(long)]
        root: Option<PathBuf>,
        /// Path to database file
        #[arg(long)]
        db: Option<PathBuf>,
    },
}

#[derive(Parser, Debug)]
#[command(
    name = "sf",
    about = "source_fast: persistent trigram search for source code",
    version,
    long_about = None
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

fn default_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn default_db_path(root: &std::path::Path) -> PathBuf {
    let mut dir = root.to_path_buf();
    dir.push(".source_fast");
    let _ = std::fs::create_dir_all(&dir);
    dir.push("index.db");
    dir
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt().with_env_filter(filter).with_target(false).init();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let args = Args::parse();

    match args.command {
        Command::Index { root, db } => {
            run_index_only(root, db).await?;
        }
        Command::Search { root, db, query } => {
            run_cli(root, db, query).await?;
        }
        Command::Server { root, db } => {
            run_server(root, db).await?;
        }
    }

    Ok(())
}

async fn run_cli(
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

async fn run_server(
    root: Option<PathBuf>,
    db: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = root.unwrap_or_else(default_root);
    let db_path = db.unwrap_or_else(|| default_db_path(&root));

    info!("source_fast MCP server starting");
    info!("root: {}", root.display());
    info!("db: {}", db_path.display());

    let index = Arc::new(PersistentIndex::open_or_create(&db_path)?);

    initial_scan(&root, Arc::clone(&index))?;

    let index_for_watcher = Arc::clone(&index);
    let root_for_watcher = root.clone();
    tokio::spawn(async move {
        if let Err(err) = background_watcher(root_for_watcher, index_for_watcher).await {
            error!("file watcher stopped: {err}");
        }
    });

    run_stdio_loop(index).await?;

    Ok(())
}

async fn run_index_only(
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

    if let Err(err) = initial_scan(&root, Arc::clone(&index)) {
        error!("Indexing failed: {}", err);
        std::process::exit(1);
    }

    info!("Index build completed");
    Ok(())
}

async fn run_stdio_loop(index: Arc<PersistentIndex>) -> Result<(), Box<dyn std::error::Error>> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(err) => {
                warn!("Failed to parse JSON-RPC request: {err}");
                continue;
            }
        };

        let response = handle_request(&index, value).await;
        let serialized = serde_json::to_string(&response)?;
        stdout.write_all(serialized.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }

    Ok(())
}

#[derive(serde::Deserialize)]
struct SearchParams {
    query: String,
}

async fn handle_request(
    index: &Arc<PersistentIndex>,
    value: serde_json::Value,
) -> serde_json::Value {
    let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let method = value
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or_default();

    match method {
        "initialize" => {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": true
                    }
                }
            })
        }
        "tools/list" => {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "search_code",
                            "description": "Search source code using a persistent trigram index",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "query": {
                                        "type": "string",
                                        "description": "Search query string"
                                    }
                                },
                                "required": ["query"]
                            }
                        }
                    ]
                }
            })
        }
        "tools/call" => handle_call_tool(index, id, value).await,
        _ => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("Method not found: {method}")
            }
        }),
    }
}

async fn handle_call_tool(
    index: &Arc<PersistentIndex>,
    id: serde_json::Value,
    value: serde_json::Value,
) -> serde_json::Value {
    let params = value.get("params");
    let Some(params) = params else {
        return serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32602, "message": "Missing params" }
        });
    };

    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or_default();

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    match name {
        "search_code" => {
            let parsed: Result<SearchParams, _> = serde_json::from_value(arguments);
            let query = match parsed {
                Ok(p) => p.query,
                Err(err) => {
                    return serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32602,
                            "message": format!("Invalid params: {err}")
                        }
                    });
                }
            };

            let index_clone = Arc::clone(index);
            let query_clone = query.clone();
            let hits =
                match tokio::task::spawn_blocking(move || index_clone.search(&query_clone)).await {
                    Ok(Ok(h)) => h,
                    Ok(Err(err)) => {
                        return serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32000,
                                "message": format!("Search failed: {:?}", err)
                            }
                        });
                    }
                    Err(join_err) => {
                        return serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32001,
                                "message": format!("Search task panicked: {join_err}")
                            }
                        });
                    }
                };

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
                        contents.push(serde_json::json!({
                            "type": "text",
                            "text": text
                        }));
                    }
                    Ok(None) => {
                        let text = format!("File: {}\n", path.display());
                        contents.push(serde_json::json!({
                            "type": "text",
                            "text": text
                        }));
                    }
                    Err(err) => {
                        warn!("Failed to extract snippet from {}: {err}", path.display());
                    }
                }
            }

            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": contents
                }
            })
        }
        other => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("Unknown tool: {other}")
            }
        }),
    }
}
