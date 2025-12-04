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
    match path.canonicalize() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
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
