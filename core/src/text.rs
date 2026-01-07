use std::collections::{HashSet, VecDeque};
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::Snippet;

fn is_binary_file(path: &Path) -> std::io::Result<bool> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = [0u8; 1024];
    let read = f.read(&mut buf)?;
    Ok(buf[..read].contains(&0))
}

pub fn read_text_file(path: &Path) -> std::io::Result<Option<String>> {
    if is_binary_file(path)? {
        return Ok(None);
    }

    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Ok(None),
        Err(e) => Err(e),
    }
}

fn collect_trigrams_bytes(bytes: &[u8]) -> Vec<[u8; 3]> {
    if bytes.len() < 3 {
        return Vec::new();
    }

    let mut set: HashSet<[u8; 3]> = HashSet::new();
    for window in bytes.windows(3) {
        set.insert([window[0], window[1], window[2]]);
    }

    let mut result: Vec<[u8; 3]> = set.into_iter().collect();
    result.sort_unstable();
    result
}

pub fn collect_trigrams(text: &str) -> Vec<[u8; 3]> {
    collect_trigrams_bytes(text.as_bytes())
}

pub fn file_modified_timestamp(path: &Path) -> u64 {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn normalize_path(path: &Path) -> String {
    // Try direct canonicalization first (file exists)
    if let Ok(p) = path.canonicalize() {
        return p.to_string_lossy().into_owned();
    }

    // File doesn't exist - canonicalize parent and append filename
    if let Some(parent) = path.parent() {
        if let Ok(canonical_parent) = parent.canonicalize() {
            if let Some(file_name) = path.file_name() {
                return canonical_parent
                    .join(file_name)
                    .to_string_lossy()
                    .into_owned();
            }
        }
    }

    // Ultimate fallback
    path.to_string_lossy().into_owned()
}

pub fn extract_snippet(path: &Path, query: &str) -> std::io::Result<Option<Snippet>> {
    use std::io::BufRead;

    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut lines_iter = reader.lines().enumerate();
    let mut buffer: VecDeque<(usize, String)> = VecDeque::new();

    while let Some((idx, line_res)) = lines_iter.next() {
        let line_no = idx + 1;
        let line = line_res?;

        if line.contains(query) {
            let mut collected = Vec::new();
            for (n, text) in &buffer {
                collected.push((*n, text.clone()));
            }
            collected.push((line_no, line.clone()));

            for _ in 0..2 {
                if let Some((i, next_res)) = lines_iter.next() {
                    let next_line = next_res?;
                    collected.push((i + 1, next_line));
                } else {
                    break;
                }
            }

            return Ok(Some(Snippet {
                path: path.to_path_buf(),
                line_number: line_no,
                lines: collected,
            }));
        } else {
            if buffer.len() == 2 {
                buffer.pop_front();
            }
            buffer.push_back((line_no, line));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // ============ Trigram Tests ============

    #[test]
    fn test_trigrams_basic() {
        let trigrams = collect_trigrams("hello");
        // "hello" -> "hel", "ell", "llo"
        assert_eq!(trigrams.len(), 3);
        assert!(trigrams.contains(&[b'h', b'e', b'l']));
        assert!(trigrams.contains(&[b'e', b'l', b'l']));
        assert!(trigrams.contains(&[b'l', b'l', b'o']));
    }

    #[test]
    fn test_trigrams_exactly_three_chars() {
        let trigrams = collect_trigrams("abc");
        assert_eq!(trigrams.len(), 1);
        assert_eq!(trigrams[0], [b'a', b'b', b'c']);
    }

    #[test]
    fn test_trigrams_less_than_three_chars() {
        assert!(collect_trigrams("").is_empty());
        assert!(collect_trigrams("a").is_empty());
        assert!(collect_trigrams("ab").is_empty());
    }

    #[test]
    fn test_trigrams_deduplication() {
        // "aaa" has only one unique trigram: "aaa"
        let trigrams = collect_trigrams("aaa");
        assert_eq!(trigrams.len(), 1);

        // "aaaa" also has only one unique trigram
        let trigrams = collect_trigrams("aaaa");
        assert_eq!(trigrams.len(), 1);
    }

    #[test]
    fn test_trigrams_sorted() {
        let trigrams = collect_trigrams("zyxabc");
        // Should be sorted by byte value
        for i in 1..trigrams.len() {
            assert!(trigrams[i - 1] <= trigrams[i]);
        }
    }

    #[test]
    fn test_trigrams_unicode() {
        // UTF-8 multi-byte characters
        let trigrams = collect_trigrams("日本語");
        // Each Japanese char is 3 bytes in UTF-8, so we get multiple trigrams
        assert!(!trigrams.is_empty());
    }

    #[test]
    fn test_trigrams_with_spaces() {
        let trigrams = collect_trigrams("a b c");
        // "a b", " b ", "b c", " c " - wait, that's wrong
        // Actually: "a b" (indices 0,1,2), " b " (1,2,3), "b c" (2,3,4)
        assert!(trigrams.contains(&[b'a', b' ', b'b']));
        assert!(trigrams.contains(&[b' ', b'b', b' ']));
        assert!(trigrams.contains(&[b'b', b' ', b'c']));
    }

    #[test]
    fn test_trigrams_special_chars() {
        let trigrams = collect_trigrams("fn(){}");
        assert!(trigrams.contains(&[b'f', b'n', b'(']));
        assert!(trigrams.contains(&[b'(', b')', b'{']));
    }

    #[test]
    fn test_trigrams_newlines() {
        let trigrams = collect_trigrams("a\nb\nc");
        assert!(trigrams.contains(&[b'a', b'\n', b'b']));
    }

    // ============ Binary Detection Tests ============

    #[test]
    fn test_is_binary_with_null_byte() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"hello\x00world").unwrap();
        file.flush().unwrap();

        let result = read_text_file(file.path()).unwrap();
        assert!(result.is_none(), "File with null byte should be detected as binary");
    }

    #[test]
    fn test_is_not_binary_text_file() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"hello world\nthis is text").unwrap();
        file.flush().unwrap();

        let result = read_text_file(file.path()).unwrap();
        assert!(result.is_some(), "Plain text file should not be binary");
        assert_eq!(result.unwrap(), "hello world\nthis is text");
    }

    #[test]
    fn test_empty_file_not_binary() {
        let file = NamedTempFile::new().unwrap();
        let result = read_text_file(file.path()).unwrap();
        assert!(result.is_some(), "Empty file should not be considered binary");
        assert_eq!(result.unwrap(), "");
    }

    #[test]
    fn test_binary_at_start() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"\x00hello").unwrap();
        file.flush().unwrap();

        let result = read_text_file(file.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_binary_detection_within_first_1024_bytes() {
        let mut file = NamedTempFile::new().unwrap();
        let mut content = vec![b'a'; 1000];
        content.push(0); // null byte at position 1000
        file.write_all(&content).unwrap();
        file.flush().unwrap();

        let result = read_text_file(file.path()).unwrap();
        assert!(result.is_none(), "Null byte within first 1024 bytes should be detected");
    }

    #[test]
    fn test_binary_detection_beyond_1024_bytes() {
        let mut file = NamedTempFile::new().unwrap();
        let mut content = vec![b'a'; 2000];
        content[1500] = 0; // null byte at position 1500 (beyond detection window)
        file.write_all(&content).unwrap();
        file.flush().unwrap();

        // The file passes the binary check (only first 1024 bytes are checked)
        // Null byte is valid UTF-8, so read_to_string succeeds
        let result = read_text_file(file.path()).unwrap();
        // This actually succeeds because:
        // 1. Binary check only looks at first 1024 bytes (no null there)
        // 2. Null byte is valid UTF-8
        assert!(result.is_some(), "File passes binary check and is valid UTF-8");
        assert!(result.unwrap().contains('\0'), "Content should contain null byte");
    }

    // ============ Normalize Path Tests ============

    #[test]
    fn test_normalize_existing_file() {
        let file = NamedTempFile::new().unwrap();
        let normalized = normalize_path(file.path());

        // Should be an absolute path
        assert!(Path::new(&normalized).is_absolute());
    }

    #[test]
    fn test_normalize_nonexistent_file_existing_parent() {
        let temp_dir = tempfile::tempdir().unwrap();
        let nonexistent = temp_dir.path().join("does_not_exist.txt");

        let normalized = normalize_path(&nonexistent);

        // Should still produce an absolute path with the filename
        assert!(normalized.ends_with("does_not_exist.txt"));
        assert!(Path::new(&normalized).is_absolute());
    }

    #[test]
    fn test_normalize_relative_path() {
        // Test with a relative path that exists
        let relative = Path::new(".");

        let normalized = normalize_path(relative);
        assert!(Path::new(&normalized).is_absolute());
    }

    // ============ Snippet Extraction Tests ============

    #[test]
    fn test_extract_snippet_basic() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "line 1").unwrap();
        writeln!(file, "line 2").unwrap();
        writeln!(file, "target line").unwrap();
        writeln!(file, "line 4").unwrap();
        writeln!(file, "line 5").unwrap();
        file.flush().unwrap();

        let snippet = extract_snippet(file.path(), "target").unwrap().unwrap();

        assert_eq!(snippet.line_number, 3);
        // Should have 2 lines before + target + 2 lines after = 5 lines
        assert_eq!(snippet.lines.len(), 5);
        assert_eq!(snippet.lines[0], (1, "line 1".to_string()));
        assert_eq!(snippet.lines[1], (2, "line 2".to_string()));
        assert_eq!(snippet.lines[2], (3, "target line".to_string()));
        assert_eq!(snippet.lines[3], (4, "line 4".to_string()));
        assert_eq!(snippet.lines[4], (5, "line 5".to_string()));
    }

    #[test]
    fn test_extract_snippet_at_file_start() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "target line").unwrap();
        writeln!(file, "line 2").unwrap();
        writeln!(file, "line 3").unwrap();
        file.flush().unwrap();

        let snippet = extract_snippet(file.path(), "target").unwrap().unwrap();

        assert_eq!(snippet.line_number, 1);
        // No lines before, target + 2 lines after
        assert_eq!(snippet.lines.len(), 3);
        assert_eq!(snippet.lines[0], (1, "target line".to_string()));
    }

    #[test]
    fn test_extract_snippet_at_file_end() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "line 1").unwrap();
        writeln!(file, "line 2").unwrap();
        write!(file, "target line").unwrap(); // no newline at end
        file.flush().unwrap();

        let snippet = extract_snippet(file.path(), "target").unwrap().unwrap();

        assert_eq!(snippet.line_number, 3);
        // 2 lines before + target, no lines after
        assert_eq!(snippet.lines.len(), 3);
    }

    #[test]
    fn test_extract_snippet_single_line_file() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "target").unwrap();
        file.flush().unwrap();

        let snippet = extract_snippet(file.path(), "target").unwrap().unwrap();

        assert_eq!(snippet.line_number, 1);
        assert_eq!(snippet.lines.len(), 1);
    }

    #[test]
    fn test_extract_snippet_no_match() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "line 1").unwrap();
        writeln!(file, "line 2").unwrap();
        file.flush().unwrap();

        let snippet = extract_snippet(file.path(), "nonexistent").unwrap();
        assert!(snippet.is_none());
    }

    #[test]
    fn test_extract_snippet_empty_file() {
        let file = NamedTempFile::new().unwrap();

        let snippet = extract_snippet(file.path(), "target").unwrap();
        assert!(snippet.is_none());
    }

    #[test]
    fn test_extract_snippet_returns_first_match() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "first target").unwrap();
        writeln!(file, "second target").unwrap();
        file.flush().unwrap();

        let snippet = extract_snippet(file.path(), "target").unwrap().unwrap();

        // Should return the first match
        assert_eq!(snippet.line_number, 1);
        assert!(snippet.lines[0].1.contains("first"));
    }

    // ============ File Modified Timestamp Tests ============

    #[test]
    fn test_file_modified_timestamp_exists() {
        let file = NamedTempFile::new().unwrap();
        let ts = file_modified_timestamp(file.path());

        // Should be a reasonable timestamp (after year 2020)
        assert!(ts > 1577836800); // Jan 1, 2020
    }

    #[test]
    fn test_file_modified_timestamp_nonexistent() {
        let ts = file_modified_timestamp(Path::new("/nonexistent/file/path"));
        assert_eq!(ts, 0);
    }
}
