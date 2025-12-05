# LLM TODOs

- Replace `std::process::Command` calls to `git ls-files` and `git status --porcelain` in `fs/src/scanner.rs::initial_git_scan` with equivalent `gix` APIs, keeping the same “candidate paths → apply_changes_by_files” behavior.

