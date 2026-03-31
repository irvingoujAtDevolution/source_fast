use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod cli;
mod daemon;
mod mcp;

use crate::cli::{
    default_db_path, default_root, init_tracing_cli, init_tracing_server,
    run_file_search_with_daemon, run_index_build, run_index_watch, run_list,
    run_search_with_daemon, run_status, run_stop, run_stop_all,
};
use crate::mcp::run_server;

#[derive(Subcommand, Debug)]
enum DaemonCommand {
    /// Show daemon and index status for this repository.
    Status {
        /// Root directory
        #[arg(long)]
        root: Option<PathBuf>,
        /// Path to database file
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Stop the daemon for this repository.
    Stop {
        /// Root directory
        #[arg(long)]
        root: Option<PathBuf>,
        /// Path to database file
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// List all running daemons.
    List,
}

#[derive(Subcommand, Debug)]
enum IndexCommand {
    /// Show index build status for this repository.
    Status {
        /// Root directory
        #[arg(long)]
        root: Option<PathBuf>,
        /// Path to database file
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Build the index for this repository. Starts a background daemon.
    Build {
        /// Root directory
        #[arg(long)]
        root: Option<PathBuf>,
        /// Path to database file
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Watch the indexing progress with a live display.
    Watch {
        /// Root directory
        #[arg(long)]
        root: Option<PathBuf>,
        /// Path to database file
        #[arg(long)]
        db: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Search code content. Auto-starts a background daemon if not running.
    Search {
        /// Root directory to search
        #[arg(long)]
        root: Option<PathBuf>,
        /// Path to database file
        #[arg(long)]
        db: Option<PathBuf>,
        /// Optional regex to filter result file paths
        #[arg(long = "file-regex")]
        file_regex: Option<String>,
        /// Block until the index is fully built before returning results
        #[arg(long)]
        wait: bool,
        /// Maximum number of results to display (0 for unlimited)
        #[arg(long, default_value = "20")]
        limit: usize,
        /// Search query
        query: String,
    },
    /// Search files by path. Auto-starts a background daemon if not running.
    SearchFile {
        /// Root directory to search
        #[arg(long)]
        root: Option<PathBuf>,
        /// Path to database file
        #[arg(long)]
        db: Option<PathBuf>,
        /// Block until the index is fully built before returning results
        #[arg(long)]
        wait: bool,
        /// Pattern to match file paths (case-insensitive substring)
        pattern: String,
    },
    /// Stop the daemon for this repository.
    Stop {
        /// Root directory
        #[arg(long)]
        root: Option<PathBuf>,
        /// Path to database file
        #[arg(long)]
        db: Option<PathBuf>,
        /// Stop all known daemons
        #[arg(long)]
        all: bool,
    },
    /// Show daemon and index status for this repository, including scan progress and ETA.
    #[command(visible_aliases = ["index-status", "daemon-status"])]
    Status {
        /// Root directory
        #[arg(long)]
        root: Option<PathBuf>,
        /// Path to database file
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Daemon management commands.
    // Intentional typo-tolerant alias for users who misspell "daemon".
    #[command(visible_alias = "deamon")]
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Index management commands.
    Index {
        #[command(subcommand)]
        command: IndexCommand,
    },
    /// List all running daemons.
    List,
    /// Run MCP server over stdio.
    Server {
        /// Root directory to index and watch
        #[arg(long)]
        root: Option<PathBuf>,
        /// Path to database file
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Internal: daemon process (not user-facing).
    #[command(name = "_daemon", hide = true)]
    InternalDaemon {
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
        Command::Search {
            root,
            db,
            file_regex,
            wait,
            limit,
            query,
        } => {
            init_tracing_cli();
            run_search_with_daemon(root, db, query, file_regex, wait, limit).await?;
        }
        Command::SearchFile {
            root,
            db,
            wait,
            pattern,
        } => {
            init_tracing_cli();
            run_file_search_with_daemon(root, db, pattern, wait).await?;
        }
        Command::Stop { root, db, all } => {
            init_tracing_cli();
            if all {
                run_stop_all().await?;
            } else {
                run_stop(root, db).await?;
            }
        }
        Command::Status { root, db } => {
            init_tracing_cli();
            run_status(root, db).await?;
        }
        Command::Daemon { command } => {
            init_tracing_cli();
            match command {
                DaemonCommand::Status { root, db } => run_status(root, db).await?,
                DaemonCommand::Stop { root, db } => run_stop(root, db).await?,
                DaemonCommand::List => run_list().await?,
            }
        }
        Command::Index { command } => {
            init_tracing_cli();
            match command {
                IndexCommand::Status { root, db } => run_status(root, db).await?,
                IndexCommand::Build { root, db } => run_index_build(root, db).await?,
                IndexCommand::Watch { root, db } => run_index_watch(root, db).await?,
            }
        }
        Command::List => {
            init_tracing_cli();
            run_list().await?;
        }
        Command::Server { root, db } => {
            init_tracing_server();
            run_server(root, db).await?;
        }
        Command::InternalDaemon { root, db } => {
            // Tracing is initialized inside run_daemon (goes to daemon.log).
            let root = root.unwrap_or_else(default_root);
            let db_path = db.unwrap_or_else(|| default_db_path(&root));
            daemon::run_daemon(root, db_path).await?;
        }
    }

    Ok(())
}
