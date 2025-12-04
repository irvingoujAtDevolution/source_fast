use std::io;

use rusqlite::Error as SqliteError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("database error: {0}")]
    Db(#[from] SqliteError),

    #[error("encode error: {0}")]
    Encode(String),
}

impl From<Box<bincode::ErrorKind>> for IndexError {
    fn from(err: Box<bincode::ErrorKind>) -> Self {
        IndexError::Encode(err.to_string())
    }
}

pub type IndexResult<T> = Result<T, IndexError>;
