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
        /// Stop all known daemons across all repositories
        #[arg(long)]
        all: bool,
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
        /// Root directory to search [default: git root or cwd]
        #[arg(long)]
        root: Option<PathBuf>,
        /// Path to database file (internal, rarely needed)
        #[arg(long, hide = true)]
        db: Option<PathBuf>,
        /// Filter by file extension (e.g. -e rs -e cs)
        #[arg(short = 'e', long = "ext")]
        ext: Vec<String>,
        /// Filter files by glob pattern (e.g. -g '*.rs')
        #[arg(short, long)]
        glob: Option<String>,
        /// Filter files by regex (advanced)
        #[arg(long = "file-regex")]
        file_regex: Option<String>,
        /// Block until the index is fully built before returning results
        #[arg(short, long)]
        wait: bool,
        /// Maximum number of results to display (0 for unlimited)
        #[arg(short, long, default_value = "20")]
        limit: usize,
        /// Output as JSON (for scripts and AI agents)
        #[arg(short, long)]
        json: bool,
        /// Print only file paths, no snippets (like rg -l)
        #[arg(long)]
        files_only: bool,
        /// Print only the match count
        #[arg(short, long)]
        count: bool,
        /// Search query (minimum 3 characters)
        query: String,
    },
    /// Search files by path. Auto-starts a background daemon if not running.
    SearchFile {
        /// Root directory to search
        #[arg(long)]
        root: Option<PathBuf>,
        /// Path to database file
        #[arg(long, hide = true)]
        db: Option<PathBuf>,
        /// Block until the index is fully built before returning results
        #[arg(long)]
        wait: bool,
        /// Pattern to match file paths (case-insensitive substring)
        pattern: String,
    },
    /// Daemon management commands.
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
    /// Print the LLM skill description for sf (for AI agent integration)
    #[arg(long)]
    skill: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if args.skill {
        print!("{}", include_str!("skill.md"));
        return Ok(());
    }

    let Some(command) = args.command else {
        Args::parse_from(["sf", "--help"]);
        return Ok(());
    };

    match command {
        Command::Search {
            root,
            db,
            ext,
            glob,
            file_regex,
            wait,
            limit,
            json,
            files_only,
            count,
            query,
        } => {
            init_tracing_cli();
            let opts = cli::SearchOpts {
                root,
                db,
                query,
                ext,
                glob,
                file_regex,
                wait,
                limit,
                json,
                files_only,
                count,
            };
            run_search_with_daemon(opts).await?;
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
        Command::Daemon { command } => {
            init_tracing_cli();
            match command {
                DaemonCommand::Status { root, db } => run_status(root, db).await?,
                DaemonCommand::Stop { root, db, all } => {
                    if all {
                        run_stop_all().await?;
                    } else {
                        run_stop(root, db).await?;
                    }
                }
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
        Command::Server { root, db } => {
            init_tracing_server();
            run_server(root, db).await?;
        }
        Command::InternalDaemon { root, db } => {
            let root = root.unwrap_or_else(default_root);
            let db_path = db.unwrap_or_else(|| default_db_path(&root));
            daemon::run_daemon(root, db_path).await?;
        }
    }

    Ok(())
}
