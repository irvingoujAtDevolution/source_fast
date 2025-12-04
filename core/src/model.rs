use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub file_id: u32,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct Snippet {
    pub path: PathBuf,
    pub line_number: usize,
    pub lines: Vec<(usize, String)>,
}
