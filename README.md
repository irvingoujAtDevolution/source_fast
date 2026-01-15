# source_fast

Fast, persistent trigram-based code search with MCP server support.

`source_fast` builds and maintains an on-disk SQLite index of your source code, enabling rapid substring search across large codebases. It's designed for git repositories and keeps the index synchronized with file changes.

## Features

- **Trigram-based search**: Fast substring matching using 3-character trigrams
- **Git-aware incremental indexing**: Only re-indexes changed files using git diff
- **Real-time synchronization**: Background file watcher keeps index up-to-date
- **MCP server**: Integrates with Claude and other AI tools via Model Context Protocol
- **Cross-platform**: Works on Windows, macOS, and Linux

## Installation

### From source

```bash
cargo install --path app
```

### Build from source

```bash
git clone https://github.com/user/source_fast
cd source_fast
cargo build --release
```

The binary will be at `target/release/sf`.

## Usage

### Build the index

```bash
# Index current directory
sf index

# Index a specific directory
sf index --root /path/to/project
```

The index is stored at `.source_fast/index.db` in the project root.

### Search code

```bash
# Search for a string (minimum 3 characters)
sf search "function_name"

# Filter by file path regex
sf search --file-regex "\.rs$" "async fn"
```

### Search file paths

```bash
# Find files by name (case-insensitive substring)
sf search-file "config"
```

### Run MCP server

```bash
sf server
```

The server communicates via stdio using JSON-RPC (MCP protocol).

## MCP Integration

### Claude Desktop

Add to your Claude Desktop configuration (`claude_desktop_config.json`):

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

- **search_code**: Search code content with optional file path filtering
  - `query` (required): Search string (minimum 3 characters)
  - `file_regex` (optional): Regex to filter file paths

## Environment Variables

- `SOURCE_FAST_LOG_PATH`: Path to log file for MCP server (server logs are silent by default to keep stdio clean)
- `RUST_LOG`: Log level filter (e.g., `info`, `debug`, `warn`)

## How It Works

1. **Indexing**: Scans files, extracts trigrams (3-byte substrings), stores in SQLite with RoaringBitmap for efficient set operations
2. **Git integration**: Uses git diff to detect changes between runs, only re-indexing modified files
3. **Search**: Looks up trigrams from query, intersects file ID bitmaps, returns matching files with context snippets
4. **Watching**: Background file watcher (via `notify` crate) updates index in real-time

## Limitations

- Queries must be at least 3 characters (trigram requirement)
- Binary files are automatically excluded
- Deletion tracking requires git (non-git directories don't track deletions properly)
- Respects `.gitignore` patterns

## Wildcard `*` search (investigation)

SQLite can’t directly help match `foo*bar` against file contents today because the database does not store
file contents (only trigrams → file-id bitmaps and file paths). A practical approach is:

- Parse query into literal segments split by unescaped `*`.
- Choose an “anchor” segment (typically the longest / most trigrams) with length `>= 3` bytes and use the
  existing trigram index to produce candidate files.
- Verify the full `*` semantics by scanning candidate file lines and checking that segments appear in order
  on the same line (fast final filter; keeps correctness).

This keeps the index fast/simple while enabling a lightweight wildcard syntax without a full regex engine.

## Project Structure

```
source_fast/
├── core/     # Index storage, trigram search, SQLite backend
├── fs/       # Filesystem scanning, git integration, file watcher
├── app/      # CLI binary and MCP server
└── llm/      # Documentation for AI assistants
```

## License

MIT
