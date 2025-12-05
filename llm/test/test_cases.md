Here is the comprehensive **Test Plan** for `source_fast`.

You should pass this list to your Coding Agent. It is organized by **complexity** (from "Happy Path" to "Nasty Edge Cases").

---

### Phase 1: Basic Functionality (The "Happy Path")
*These tests verify the core engine works under normal conditions.*

| ID | Test Case | Action | Expected Outcome |
| :--- | :--- | :--- | :--- |
| **B1** | **Fresh Init** | Run `sf index` on a fresh repo. | DB created. `meta` table has `git_head`. All tracked files indexed. |
| **B2** | **Basic Search** | Run `sf search "struct Main"` (content that exists). | Returns file path and snippet. |
| **B3** | **No Match** | Run `sf search "xyz_nonexistent_123"`. | Returns "No results found". |
| **B4** | **Re-Index (No Changes)** | Run `sf index` immediately a second time. | Execution should be instant (< 1s). Logs show "0 files changed". |

---

### Phase 2: The "Git Magic" (Incremental Logic)
*These tests verify that we are correctly using the Git Commit Hash shortcut.*

| ID | Test Case | Action | Expected Outcome |
| :--- | :--- | :--- | :--- |
| **G1** | **New Commit** | Modify 1 file, `git commit`. Run `sf index`. | Logs show "1 file changed". Search finds new content. Old content gone. |
| **G2** | **Dirty State (Modified)** | Modify `main.rs`, **do not commit**. Run `sf index`. | Search finds new dirty content. DB `git_head` remains at old commit hash (or reflects that dirty state was handled). |
| **G3** | **Dirty State (Untracked)** | Create `new.rs`, **do not `git add`**. Run `sf index`. | Search finds `new.rs`. |
| **G4** | **Branch Switch** | `git checkout -b feature`. Change 10 files. Commit. `sf index`. Switch back `git checkout main`. `sf index`. | First index updates 10 files. Second index reverts those 10 files. Search reflects `main` branch state. |
| **G5** | **Git Reset** | `git reset --hard HEAD~1` (Delete recent work). Run `sf index`. | Deleted files must **disappear** from search results. (Verify "Ghost Matches" are gone). |
| **G6** | **Git Ignore** | Create `secret.key`. Add to `.gitignore`. Run `sf index`. | `secret.key` should **NOT** be indexed. |

---

### Phase 3: File System Edge Cases
*These tests verify the "Forward Index" cleanup logic and binary protection.*

| ID | Test Case | Action | Expected Outcome |
| :--- | :--- | :--- | :--- |
| **F1** | **Deletion** | Delete `file.rs` (rm file.rs). Run `sf index`. | Search for unique string in `file.rs` returns 0 results. |
| **F2** | **Rename** | `mv old.rs new.rs`. Run `sf index`. | Search returns `new.rs`. Does **not** return `old.rs`. |
| **F3** | **Binary Bomb** | Create a valid PNG image named `icon.png` (or `icon.rs` to trick it). Run `sf index`. | Log should say "Skipping binary file". DB size should not explode. |
| **F4** | **Null Byte Injection** | Create a text file, insert a `0x00` byte in the middle. Run `sf index`. | Should be skipped/ignored (treated as binary). |
| **F5** | **Empty File** | Create empty `empty.rs`. Run `sf index`. | Should not crash. |

---

### Phase 4: Resilience & Recovery (The "Chaos Monkey")
*These tests verify the transaction safety.*

| ID | Test Case | Action | Expected Outcome |
| :--- | :--- | :--- | :--- |
| **R1** | **Interruption** | Start indexing a HUGE repo. Hit `Ctrl+C` halfway. Run `sf index` again. | Second run should detect mismatched Git Hash and resume/finish indexing correctly. |
| **R2** | **History Rewrite** | `git rebase -i` (Change old commit hashes). Run `sf index`. | Tool detects stored hash is missing from Git history. Triggers **Full Re-scan**. State corrects itself. |
| **R3** | **Locked DB** | Start `sf server` (Server Mode). In another terminal, try `sf index` (CLI Mode). | CLI should error gracefully ("Database Locked") or wait 5s, not panic. |
| **R4** | **Corrupt DB** | Delete `.source_fast/index.db`. Run `sf index`. | Should transparently recreate DB and full index. |

---

### Phase 5: Search Quality & MCP
*These tests verify the tool is useful to an AI.*

| ID | Test Case | Action | Expected Outcome |
| :--- | :--- | :--- | :--- |
| **S1** | **Substring** | File contains "function_name". Search "nction". | Should match. |
| **S2** | **Snippet Context** | Search for a unique line. | Output should contain the line **plus** 2 lines before and after. |
| **S3** | **JSON-RPC** | Run `sf --server`. Pipe `{"jsonrpc": "2.0", "method": "tools/call", ...}` to stdin. | Should output valid JSON-RPC response to stdout. Log to stderr. |

---

### How to use this list
Tell your Agent:
> "After implementing the features, create a test script (or manual verification steps) that runs through **G1, G5, F1, F3, and R1**. These are the most critical tests for success."