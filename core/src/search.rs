use std::path::Path;
use std::path::PathBuf;

use rayon::prelude::*;
use regex::Regex;

use crate::IndexResult;
use crate::model::{SearchHit, SearchResult};
use crate::storage::search_database_file_filtered;
use crate::text::extract_snippet;

pub fn attach_snippets(hits: Vec<SearchHit>, query: &str) -> Vec<SearchResult> {
    hits.into_par_iter()
        .map(|hit| {
            let path = PathBuf::from(&hit.path);
            match extract_snippet(&path, query) {
                Ok(snippet) => SearchResult {
                    file_id: hit.file_id,
                    path: hit.path,
                    snippet,
                    snippet_error: None,
                },
                Err(err) => SearchResult {
                    file_id: hit.file_id,
                    path: hit.path,
                    snippet: None,
                    snippet_error: Some(err.to_string()),
                },
            }
        })
        .collect()
}

pub fn search_database_file_with_snippets(path: &Path, query: &str) -> IndexResult<Vec<SearchResult>> {
    search_database_file_with_snippets_filtered(path, query, None)
}

pub fn search_database_file_with_snippets_filtered(
    path: &Path,
    query: &str,
    file_regex: Option<&Regex>,
) -> IndexResult<Vec<SearchResult>> {
    let hits = search_database_file_filtered(path, query, file_regex)?;
    Ok(attach_snippets(hits, query))
}

