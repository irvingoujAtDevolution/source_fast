# Backlog

Items deferred from the initial development sessions. Ordered roughly by impact.

## Performance

- [ ] **Sort-merge trigram index for small repos** — branch `experiment/sort-merge-mmap` has a working prototype. At 52k files (5.7 GB pairs) it's slower than LMDB due to memory pressure. Would need chunked merge (process N files at a time, merge intermediate indexes) to scale. Could still win for repos < 5000 files where everything fits in L3 cache.
- [ ] **MDB_APPEND for sorted bulk inserts** — when building from scratch, trigram keys could be inserted in sorted order to skip B-tree traversal. heed exposes `PutFlags::APPEND`. Not attempted yet.
- [ ] **Search: cache LMDB env across CLI invocations** — each `sf search` opens a new LMDB env (1 GB mmap). For agents doing 50 searches/session, a persistent connection or env cache would save ~1s per search.
- [ ] **Multiple matches per file** — currently `extract_snippet` returns only the first match. rg shows all matches. This is the biggest search quality gap.

## Robustness

- [ ] **Cancel scan/watcher tasks on demotion** — when the daemon loses its writer lease, old scan and watcher tasks continue running as zombies. Need a cancellation flag (`Arc<AtomicBool>`) passed to `smart_scan_with_progress` and `background_watcher`. TODO is in the code.
- [ ] **LMDB map size exhaustion** — hardcoded at 1 GB. No recovery path when the index exceeds this. Should catch `MDB_MAP_FULL`, reopen with larger map size.
- [ ] **Watcher debounce** — `notify` watcher sleeps 500ms per event with no path-level coalescing. Rapid saves on the same file generate redundant re-index operations.
- [ ] **File ID recycling** — deleted file IDs are never reused (monotonic counter). Not a problem in practice (u32 = 4 billion IDs) but `checked_add` will error at overflow.

## Features

- [ ] **Tree-sitter symbol extraction** — parse source files with tree-sitter to extract function/class/struct definitions. Enables `sf search-symbol "parse_config"` for precise code navigation. Design discussed but not implemented.
- [ ] **`.sfignore` file** — user-configurable ignore patterns (like `.gitignore` but for the search index). Currently relies on `.gitignore` via the `ignore` crate.
- [ ] **Search result ranking** — trigram search returns all matches unranked. Could rank by: match in filename > match in code > match in comments > match in generated files.
- [ ] **Regex content search** — current search is substring-only. Adding regex would cover the remaining rg use case gap.

## Code Quality

- [ ] **Extract shared election logic** — `daemon.rs` and `mcp.rs` have nearly identical leader election + scan + watcher code. Should be a shared helper. TODOs in both files.
- [ ] **Scanner unit tests** — `fs/src/scanner.rs` has zero unit tests. The most complex logic (git diff, incremental scan, fallbacks) is only tested via E2E.
- [ ] **Linux/macOS testing** — all development and testing happened on Windows. CI runs on all platforms but E2E tests may have platform-specific issues.
- [ ] **S3 MCP test reliability** — the raw JSON-RPC test silently passes if the server is slow (single `read_line` with 500ms timeout).

## Distribution

- [ ] **NPM wrapper** — thin npm package that downloads the right binary on `npm install -g source-fast`. Familiar install path for JS/TS developers.
- [ ] **Homebrew formula** — for macOS users.
- [ ] **Winget / Scoop manifest** — for Windows users.
