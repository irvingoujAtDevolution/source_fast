pub mod error;
pub mod model;
pub mod search;
pub mod storage;
pub mod text;

pub use error::{IndexError, IndexResult};
pub use model::{SearchHit, SearchResult, Snippet};
pub use search::{search_database_file_with_snippets, search_database_file_with_snippets_filtered};
pub use storage::{
    PersistentIndex, is_leader_active_readonly, now_millis, read_leader_readonly,
    read_meta_readonly, rewrite_root_paths, search_database_file, search_database_file_filtered,
    search_files_in_database,
};
pub use text::extract_snippet;
