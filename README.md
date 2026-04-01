# source_fast

Fast, persistent trigram-based code search with a daemonized CLI and MCP server.

`source_fast` keeps a persistent on-disk index of a repository and uses it for instant substring search over code. Uses LMDB (via heed) for storage, `gix` for git-aware incremental scans, `notify` for background file watching, and `rmcp` for MCP integration.

## Quick Start

```bash
# Install
cargo install --path app

# Search (auto-starts daemon + indexing on first use)
sf search "function_name"

# Pre-build index with live progress
sf index watch
```

## Search

```bash
sf search "query"                       # rg-style output with snippets
sf search -e rs "query"                 # filter by extension (.rs files only)
sf search -e cs -e xaml "ViewModel"     # multiple extensions
sf search -g '*.test.ts' "describe"     # filter by glob pattern
sf search --file-regex '\.rs$' "query"  # filter by regex (advanced)
sf search -l 50 "query"                 # show 50 results (default: 20, 0=all)
sf search -w "query"                    # wait for index to finish first
```

### Output modes

```bash
sf search "query"                       # default: colored snippets with context
sf search -c "query"                    # count only (instant, no file I/O)
sf search --files-only "query"          # file paths only (like rg -l)
sf search -j "query"                    # JSON output (for scripts/AI agents)
```

### Search file paths

```bash
sf search-file "config"                 # case-insensitive substring match
sf search-file "Cargo.toml"
```

## Index Management

```bash
sf index build                          # start background daemon + indexing
sf index watch                          # foreground indexing with live progress bar
sf index status                         # show build progress and ETA
```

`sf index watch` shows a 60fps live display:
```
⠹ git-initial [████████████░░░░░░░░░░░░░░░░░░] 3450/9467 (36%)  101/257 MB  ETA 29s  315 files/sec
  DockerManagementViewModel.cs
```

## Daemon Management

```bash
sf status                               # daemon + index status
sf stop                                 # stop the background daemon
sf stop --all                           # stop all known daemons
sf daemon list                          # list all running daemons
```

## MCP Server

```bash
sf server --root /path/to/repo
```

Claude Desktop configuration:

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

The MCP server exposes a `search_code` tool with `query` and optional `file_regex` parameters. Leader election ensures only one process writes to the index at a time.

## AI Agent Integration

```bash
sf --skill                              # print LLM skill description
sf search -j "query"                    # JSON output for parsing
sf search -c "query"                    # count for quick checks
sf search --files-only "query"          # file list for iteration
```

`sf --skill` outputs a structured skill document that AI agents can read to learn the full CLI interface.

## How It Works

1. **Trigram indexing**: every 3-byte sequence in every file is stored in an inverted index
2. **LMDB storage**: Roaring bitmaps map trigrams to file IDs; LMDB provides concurrent multi-process reads
3. **Git-aware scanning**:
   - First run: git index/worktree scan
   - Later runs: incremental HEAD diff + worktree changes
   - Fallback: full filesystem scan if git is unavailable
4. **Background daemon**: file watcher keeps the index updated on create/modify/delete
5. **Search**: bitmap intersection finds candidates, then snippet extraction verifies matches

## Workspace Layout

```
app/       — sf CLI binary, daemon, MCP server
core/      — persistent index, trigram search, LMDB storage
fs/        — filesystem scanning, git diff, file watcher
progress/  — shared scan progress types
scripts/   — benchmark harness
```

## Environment Variables

| Variable | Purpose |
|----------|---------|
| `SOURCE_FAST_LOG_PATH` | Append CLI/MCP logs to this file (silent by default) |
| `RUST_LOG` | Tracing filter: `info`, `debug`, `warn` |

Daemon logs are always written to `.source_fast/daemon.log`.

## Index Storage

```
.source_fast/
├── index.mdb/          ← LMDB environment (data.mdb + lock.mdb)
├── daemon.log
└── .shutdown_requested  ← signal file for graceful stop
```

## Limitations

- Queries must be at least 3 characters
- Content search is substring-based (no regex content search)
- Binary files are skipped (null byte in first 1024 bytes)
- LMDB map size is fixed at 1 GB (covers most repositories)
- Results may be partial during initial index build

## Testing

164 tests: 64 unit + 17 fs + 83 end-to-end covering search, filesystem, git, leader election, MCP, worktree, resilience, and edge cases.

```bash
cargo test                              # run all tests
cargo test -p source_fast_core          # unit tests only
cargo test -p source_fast --test e2e_basic  # specific E2E suite
```

## License

MIT
