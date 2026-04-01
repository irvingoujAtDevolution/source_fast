# Architecture & Performance Design

## Overview

source_fast is a persistent trigram-based code search engine. It builds an inverted index mapping every 3-byte sequence to the files that contain it, enabling instant substring search across large codebases. A background daemon keeps the index updated as files change.

## System Architecture

```
┌────────────────────────────────────┐
│            CLI Process             │
│  sf search / sf status / sf stop   │
│                                    │
│  Opens LMDB read-only             │
│  Trigram intersection → file IDs   │
│  Snippet extraction from filesystem│
└──────────┬─────────────────────────┘
           │ LMDB (multi-process read)
           ▼
┌────────────────────────────────────┐
│         LMDB Database              │
│  .source_fast/index.mdb/           │
│                                    │
│  files:         file_id → path     │
│  files_by_path: path → file_id    │
│  trigrams:      [u8;3] → bitmap   │
│  file_trigrams: file_id → [u8;3]s │
│  meta:          key → value        │
│  leader:        lease record       │
└──────────┬─────────────────────────┘
           │ LMDB (single writer)
           ▼
┌────────────────────────────────────┐
│         Daemon Process             │
│  sf _daemon (background)           │
│                                    │
│  Leader election (lease-based)     │
│  Initial scan (packfile or fs)     │
│  File watcher (notify crate)       │
│  Writer thread (batched txns)      │
└────────────────────────────────────┘
```

## Data Flow

### Cold Build (first index)

```
1. gix tree walk
   Traverse HEAD tree recursively, collect blob OIDs.
   ~0.7s for 52k files.

2. Packfile blob read (sequential)
   Read blob content from git pack files via gix.
   Sequential zlib decompression — gix::Repository is !Sync.
   ~13s for 52k files (978 MB). This is I/O + decompress bound.

3. Trigram extraction (parallel, rayon)
   For each file: sort+dedup byte windows of size 3.
   ~5s across 8 cores.

4. Bitmap building (sequential)
   Fixed 16M-entry array (one per possible trigram).
   Direct index: bitmaps[tri[0]<<16 | tri[1]<<8 | tri[2]].insert(file_id)
   No hashing. O(1) per insert. ~10s for 15M insertions.

5. Bulk LMDB write (single transaction)
   Write all files, file_trigrams, and trigrams tables in one commit.
   Trigram keys written in sorted order for optimal B-tree insertion.
   ~8s for 581K unique trigrams.

Total: ~33s for 52k files (RDM monorepo).
```

### Incremental Update (file changed)

```
1. File watcher (notify crate) detects change
2. index_path() reads file from filesystem
3. collect_trigrams() extracts new trigram set
4. Writer thread:
   a. Read old trigram set from file_trigrams table
   b. diff_sorted_trigrams(old, new) → removed + added
   c. For removed: read bitmap, remove file_id, write back
   d. For added: read bitmap, insert file_id, write back
   e. Update file_trigrams with new set
5. Commit batch (up to 64 MB of changes per txn)
```

### Search Query

```
1. Extract trigrams from query string
2. For each trigram: look up RoaringBitmap in LMDB
3. Intersect bitmaps (smallest first, early exit on empty)
4. Resolve file_ids to paths via files table
5. Extract snippets from filesystem (parallel, rayon)
6. Stream results to stdout as snippets are found
```

## Storage Design

### LMDB (heed)

Single LMDB environment with 6 named databases:

| Database | Key | Value | Purpose |
|----------|-----|-------|---------|
| `files` | u32 | FileRecord (bincode) | file_id → {path, last_modified} |
| `files_by_path` | &str | u32 | path → file_id (reverse index) |
| `trigrams` | &[u8] (3 bytes) | RoaringBitmap (bincode) | inverted index |
| `file_trigrams` | u32 | Vec<[u8;3]> (bincode) | per-file trigram set for delta computation |
| `meta` | &str | &str | git_head, index_status, daemon_pid, etc. |
| `leader` | &str | LeaderRecord (bincode) | writer lease for leader election |

Configuration:
- Map size: 1 GB
- Flags: `WRITE_MAP | NO_META_SYNC` (safe — index is rebuildable)
- Multi-process: daemon writes, CLI reads concurrently

### Why LMDB (not SQLite, not redb)

| Requirement | SQLite | redb | LMDB |
|------------|--------|------|------|
| Multi-process read+write | WAL mode ✓ | Single process only ✗ | MVCC ✓ |
| Zero-copy reads | No (copies to Vec) | Yes | Yes |
| Write performance | SQL parsing overhead | Good | Good |
| Crash safety | WAL + fsync | CoW B-tree | CoW B-tree |

We tried SQLite first, then redb. SQLite was the bottleneck due to SQL parsing overhead. redb doesn't support multi-process access (CLI can't read while daemon writes). LMDB supports both.

## Trigram Index Design

### Trigram Extraction

For a file with content bytes `b[0..n]`:
- Slide a 3-byte window: `b[i], b[i+1], b[i+2]` for i in 0..n-2
- Sort the resulting `Vec<[u8;3]>` and dedup (replaces HashSet — faster for small keys)
- Result: sorted, unique trigram set for the file

Operating on raw bytes, not Unicode code points. A UTF-8 multibyte character produces multiple trigrams spanning its byte boundaries. This is correct for substring search.

### Bitmap Storage

Each trigram maps to a `RoaringBitmap` of file IDs. Roaring bitmaps:
- Compress runs of consecutive IDs efficiently
- Support O(1) insert and fast intersection via bitwise AND
- Serialize compactly via bincode

### Search Algorithm

1. Query "hello" → trigrams: ["hel", "ell", "llo"]
2. Look up each trigram's bitmap
3. If any trigram is missing → no results (early exit)
4. Sort bitmaps by cardinality (smallest first)
5. Intersect sequentially: `result &= next_bitmap`
6. Early exit if intersection becomes empty
7. Result: set of file IDs containing all query trigrams

This is a **necessary but not sufficient** filter. Files in the result set contain all trigrams but may not contain the exact query substring. Snippet extraction verifies the actual match.

## Cold Build Optimization: Packfile Read

### Problem

Reading 52k individual files from NTFS: ~370 seconds (50k `open()`/`read()`/`close()` syscalls).
Each `open()` costs ~100-200μs on NTFS due to directory traversal and inode lookup.

### Solution

Read blob contents from git pack files via gix instead of the filesystem.
Pack files are large sequential files with zlib-compressed blobs.
One sequential read + decompression instead of 52k random file opens.

**Result: 370s → 13s (28x faster)**

### Trade-offs

- Pack files contain committed content only — dirty/untracked files must still be read from filesystem
- Blob content has no modification timestamp — use dummy mtime, overridden on next incremental scan
- gix::Repository is `!Sync` — packfile read is sequential (but fast due to sequential I/O)

## Cold Build Optimization: Bulk LMDB Write

### Problem

Per-file LMDB writes: 52k files × ~600 B-tree operations each = 31M operations → 5+ minutes.
Each trigram requires: get existing bitmap → insert file_id → put bitmap back.

### Solution

Build all trigram bitmaps in memory first, write to LMDB in a single transaction:

1. Allocate fixed 16M-entry `Vec<RoaringBitmap>` (~128 MB)
2. For each file: direct-index insert (no hashing, O(1))
3. Collect non-empty bitmaps, sort by trigram key
4. One `env.write_txn()`: sequential puts for all tables
5. `wtxn.commit()`: single fsync

**Result: 5+ minutes → 8 seconds (40x faster)**

### Why fixed array instead of HashMap

The trigram key space is exactly `256^3 = 16,777,216`. A direct-indexed array:
- O(1) lookup: `array[(b0 << 16) | (b1 << 8) | b2]`
- No hash computation (SipHash for [u8;3] costs ~15ns per op)
- No hash table resizing or collision handling
- Cache-friendly sequential access during the final write pass
- 128 MB memory (16M × 8-byte empty RoaringBitmap) — acceptable

HashMap was 40 seconds. Fixed array: 15 seconds.

## Daemon & Process Architecture

### Leader Election

LMDB-stored lease with TTL:
- `leader` table: `"writer" → {holder: "pid:12345:nanos", expires_at_ms: 1234567890}`
- Acquire: check-and-set in a write transaction (atomic)
- Renew: update `expires_at_ms` every 500ms loop iteration
- Release: set `expires_at_ms = 0` on graceful shutdown

Only the lease holder can write to the index. Other processes (CLI, other daemons) read concurrently.

### Daemon Lifecycle

```
sf search "query"
  └→ ensure_daemon()
      └→ if no daemon running: spawn "sf _daemon" as detached process
          └→ daemon acquires lease, runs initial scan, starts file watcher
  └→ search via LMDB read transaction (concurrent with daemon writes)
```

### Signal-File Shutdown

`sf stop` writes `.source_fast/.shutdown_requested`. The daemon polls for this file every 500ms. This avoids needing a write transaction to signal shutdown (which would block on the daemon's write lock).

### Foreground Indexing (`sf index watch`)

Runs the scan in-process with a 60fps progress display:
- Acquires writer lease (stops daemon first if needed)
- Runs `smart_scan_with_progress` directly
- Lock-free `WatchState` struct: atomics for counters, mutex for strings
- Render thread reads at 60fps, progress callback writes at file-processing speed
- Braille spinner: `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`

## Performance Summary

### RDM Monorepo (52k files, 981 MB)

| Operation | Before | After | Speedup |
|-----------|-------:|------:|--------:|
| Cold build (filesystem read) | 6m 15s | — | baseline |
| Cold build (packfile + bulk LMDB) | — | 33s | 11x |
| Incremental (1 file changed) | ~2s | ~2s | — |
| Warm search (174 results) | ~2.5s | ~1.5s | 1.7x |
| Count (`-c` flag) | — | instant | — |

### Cold Build Breakdown (52k files)

| Phase | Time | Bottleneck |
|-------|-----:|-----------|
| Tree walk | 0.7s | — |
| Packfile read | 13s | Sequential zlib decompression |
| Trigram extraction | 5s | CPU (parallelized) |
| Bitmap building | 10s | Sequential array inserts |
| LMDB bulk write | 8s | B-tree + fsync |
| **Total** | **~33s** | |

### Known Limitations

- LMDB map size: 1 GB fixed. No auto-growth on exhaustion.
- Packfile read is sequential (gix is `!Sync`). Could be parallelized with per-thread repo handles.
- Bitmap building loop is sequential. Could be parallelized with thread-local arrays + merge.
- Lease expires during bulk write if it takes >5s (TTL). Needs background renewal thread.

## File Layout

```
.source_fast/
├── index.mdb/              ← LMDB environment
│   ├── data.mdb            ← B-tree data
│   └── lock.mdb            ← Process lock
├── .shutdown_requested      ← Signal file for graceful stop
└── daemon.log               ← Daemon tracing output
```

## Crate Structure

```
source_fast/
├── core/                   ← Index engine: LMDB, trigram, search, snippets
│   ├── storage.rs          ← PersistentIndex, writer thread, bulk_cold_index
│   ├── text.rs             ← Trigram extraction, binary detection
│   ├── search.rs           ← Snippet attachment (parallel rayon)
│   ├── model.rs            ← SearchHit, Snippet, SearchResult
│   └── error.rs            ← IndexError
├── fs/                     ← Scanning: git diff, packfile read, file watcher
│   ├── scanner.rs          ← smart_scan, initial_git_scan, incremental diff
│   └── watcher.rs          ← notify-based file watcher
├── app/                    ← CLI, daemon, MCP server
│   ├── cli.rs              ← Search output, index watch, progress display
│   ├── daemon.rs           ← Daemon lifecycle, leader election
│   ├── mcp.rs              ← MCP server (search_code tool)
│   └── main.rs             ← Clap CLI dispatch
└── progress/               ← Shared progress types (ScanEvent, IndexProgress)
```

## Future Work

See `BACKLOG.md` for the full list. Key items:

- **Fused packfile read + trigram extraction**: extract trigrams during blob decompression, avoid holding 713 MB in memory
- **Tree-sitter symbol index**: parse source files for function/class definitions, enable `sf search-symbol`
- **Search result ranking**: prioritize code definitions over comments, generated files
- **Parallel packfile reads**: clone gix::Repository per thread for concurrent blob decompression
- **Background lease renewal during bulk write**: prevent lease expiry on long builds
