---
name: sf
description: Fast trigram-based code search with persistent index. Use instead of grep/rg on large repos where index speed matters.
---

# sf — Source Fast

A persistent trigram-based code search engine. Builds an index once, searches instantly. Background daemon keeps the index updated as files change.

## When to use sf instead of grep/rg

- Repository has >1000 files (index pays for itself after 2-3 searches)
- You need repeated searches on the same codebase
- You want sub-second search on 50k+ file monorepos

## Quick reference

### Search code content
```bash
sf search "function_name"              # search with snippets (default limit 20)
sf search -e rs "function_name"        # filter by extension
sf search -e cs -e xaml "ViewModel"    # multiple extensions
sf search -g '*.test.ts' "describe"    # filter by glob
sf search -c "TODO"                    # just the count (instant)
sf search --files-only "import"        # file paths only (like rg -l)
sf search -j "query"                   # JSON output (structured, for parsing)
sf search -l 50 "query"               # show 50 results (default 20, 0=all)
sf search -w "query"                   # wait for index to finish first
```

### Search file paths
```bash
sf search-file "main.rs"               # find files by name substring
sf search-file "Cargo"                  # case-insensitive
```

### Index management
```bash
sf index build                          # start background daemon + indexing
sf index watch                          # foreground indexing with live progress
sf index status                         # show index build progress
```

### Daemon management
```bash
sf status                               # daemon + index status
sf stop                                 # stop the background daemon
sf daemon list                          # list all running daemons
```

## Output formats

### Default (rg-style, colored)
```
path/to/file.rs:42
40: fn previous_line() {}
41:
42: fn matching_line() {}
43: {
44:     body();
```

### JSON (-j)
```json
{
  "query": "matching_line",
  "total": 5,
  "results": [
    {
      "path": "path/to/file.rs",
      "file_id": 123,
      "line": 42,
      "snippet": [
        {"line": 40, "text": "fn previous_line() {}"},
        {"line": 41, "text": ""},
        {"line": 42, "text": "fn matching_line() {}"},
        {"line": 43, "text": "{"},
        {"line": 44, "text": "    body();"}
      ]
    }
  ]
}
```

### Files-only (--files-only)
```
path/to/file.rs
path/to/other.rs
```

### Count (-c)
```
5
```

## How it works

- Trigram index: every 3-byte sequence in every file is indexed
- Query must be >= 3 characters
- Search finds files containing ALL trigrams from the query, then verifies with actual text match
- Background daemon watches for file changes and updates the index
- Multiple processes can read the index simultaneously (LMDB)

## Important notes

- First search on a new repo spawns a daemon and starts indexing. Results may be partial.
- Use `-w` (wait) if you need complete results on first search.
- Use `sf index build` to pre-build the index before searching.
- The index is stored in `.source_fast/index.mdb` under the repo root.
- Daemon auto-starts on first search and stays running for file watching.
