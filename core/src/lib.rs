pub mod error;
pub mod model;
pub mod storage;
pub mod text;

pub use error::{IndexError, IndexResult};
pub use model::{SearchHit, Snippet};
pub use storage::{PersistentIndex, search_database_file};
pub use text::extract_snippet;
