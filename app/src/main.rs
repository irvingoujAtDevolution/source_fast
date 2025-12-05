use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod cli;
mod mcp;

use crate::cli::{init_tracing_cli, init_tracing_server, run_cli, run_index_only};
use crate::mcp::run_server;

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    match args.command {
        Command::Index { root, db } => {
            init_tracing_cli();
            run_index_only(root, db).await?;
        }
        Command::Search { root, db, query } => {
            init_tracing_cli();
            run_cli(root, db, query).await?;
        }
        Command::Server { root, db } => {
            // For MCP server, never log to stdout; optionally log to a file
            // if SOURCE_FAST_LOG_PATH is set.
            init_tracing_server();
            run_server(root, db).await?;
        }
    }

    Ok(())
}
