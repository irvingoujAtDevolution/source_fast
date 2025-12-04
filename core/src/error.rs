use std::io;

use bincode::error::{DecodeError, EncodeError};
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

impl From<EncodeError> for IndexError {
    fn from(err: EncodeError) -> Self {
        IndexError::Encode(err.to_string())
    }
}

impl From<DecodeError> for IndexError {
    fn from(err: DecodeError) -> Self {
        IndexError::Encode(err.to_string())
    }
}

pub type IndexResult<T> = Result<T, IndexError>;
