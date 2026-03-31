# source_fast

Fast, persistent trigram-based source search with a daemonized CLI and MCP server.

`source_fast` keeps a persistent on-disk index of a repository and uses it for fast substring search over code. The current implementation uses Heed/LMDB for storage, `gix` for git-aware incremental scans, `notify` for background file watching, and `rmcp` for MCP integration.

## Current Capabilities

- Fast substring search over indexed source files
- Persistent LMDB-backed index stored in the repo under `.source_fast/`
- Auto-started background daemon for warm search and continuous updates
- Git-aware incremental scans plus full-scan fallback
- Live index status, progress, and ETA reporting
- Path search for file discovery
- MCP server over stdio with a stateful `search_code` tool
- Worktree-aware index bootstrapping from the primary worktree when possible

## Workspace Layout

This repository is a Cargo workspace with four crates:

- `app`: `sf` CLI binary, daemon management, and MCP server
- `core`: persistent index, trigram search, snippets, and LMDB storage
- `fs`: filesystem scanning, git diff logic, and file watcher integration
- `progress`: shared scan progress/status types

There are also benchmark scripts and design notes at the workspace root, including the `REDB_*` and performance planning documents.

## Installation

Install the CLI from the workspace:

```bash
cargo install --path app
```

Or build locally during development:

```bash
cargo build -p source_fast
```

The binary is `sf`.

## Index Storage

By default, the index lives under the repository root:

```text
.source_fast/
  index.mdb/
  daemon.log
```

`index.mdb` is an LMDB environment directory, not a single file. You can override the location with `--db`.

## CLI Overview

### Search code

`sf search` is the main entry point. If no daemon is running for the repo, it starts one automatically.

```bash
# Search current repository
sf search "function_name"

# Wait for the initial index to finish before searching
sf search --wait "async fn"

# Filter results by file path regex
sf search --file-regex "\.rs$" "PersistentIndex"

# Show all results
sf search --limit 0 "leader lease"
```

Notes:

- Content search is substring-based, not full regex search
- Queries shorter than 3 characters return no results
- Initial searches may be partial while the first index build is still running

### Search file paths

`sf search-file` does a case-insensitive substring match on indexed file paths.

```bash
sf search-file "config"
sf search-file --wait "daemon"
```

### Build and watch the index explicitly

You can start warming the index before the first search:

```bash
sf index build
sf index watch
sf index status
```

`sf index build` starts the background daemon and kicks off indexing. `sf index watch` shows a live progress line with scan mode, processed files/bytes, and ETA.

### Inspect and control daemons

```bash
sf status
sf daemon status
sf list
sf stop
sf stop --all
```

The status output includes root, PID, version, index status, scan mode, progress, current file, and leader lease information.

### Run the MCP server

```bash
sf server
```

The MCP server communicates over stdio and maintains the index in the background. Leader election ensures only one process writes to the index at a time.

## MCP Integration

Example Claude Desktop configuration:

```json
{
  "mcpServers": {
    "source_fast": {
      "command": "sf",
      "args": ["server", "--root", "/path/to/your/project"]
    }
  }
}
```

### Available MCP Tools

- `search_code`
  - `query`: required substring query
  - `file_regex`: optional regex applied to result file paths

If the index is still building, the server returns a warning that results may be stale or incomplete.

## How It Works

1. Files are scanned and converted into trigrams (3-byte substrings).
2. The index stores trigram-to-file mappings in LMDB, using Roaring bitmaps for efficient intersections.
3. On startup, the daemon performs a git-aware scan:
   - first run: git index/worktree driven initial scan when possible
   - later runs: incremental HEAD diff plus worktree changes
   - fallback: full filesystem scan if git state is unavailable or diffing fails
4. A background watcher keeps the index updated for file create/modify/delete events.
5. Search intersects trigram bitmaps, then extracts snippets from matching files for display.

## Environment Variables

- `SOURCE_FAST_LOG_PATH`: append CLI or MCP logs to a file; if unset, those commands stay quiet by default
- `RUST_LOG`: tracing filter, such as `info`, `debug`, or `warn`

Daemon logs are written to `.source_fast/daemon.log`.

## Limitations

- Content queries must be at least 3 bytes long
- Content search is substring-based; there is no regex content search
- Binary files are skipped
- Non-git directories fall back to full-scan behavior
- Results can be temporarily stale while the initial background index build is still running

## Testing

The app crate includes end-to-end coverage for:

- basic search behavior
- filesystem updates
- git-aware scanning
- leader election and daemon readiness
- MCP readiness
- worktree behavior
- resilience and edge cases

## License

MIT
