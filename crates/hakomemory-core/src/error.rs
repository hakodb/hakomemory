//! Typed errors for the public facade. The rest of the engine still uses
//! `anyhow`; conversion happens at this boundary so library callers get a
//! stable error code instead of an opaque string.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    InvalidInput(String),
    NotFound(String),
    Storage(String),
    SchemaMismatch(String),
    EmbedderDrift(String),
    Embed(String),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::InvalidInput(_) => "E_INPUT",
            Error::NotFound(_) => "E_NOT_FOUND",
            Error::Storage(_) => "E_STORAGE",
            Error::SchemaMismatch(_) => "E_SCHEMA",
            Error::EmbedderDrift(_) => "E_EMBED",
            Error::Embed(_) => "E_EMBED",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Error::InvalidInput(m)
            | Error::NotFound(m)
            | Error::Storage(m)
            | Error::SchemaMismatch(m)
            | Error::EmbedderDrift(m)
            | Error::Embed(m) => m,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for Error {}

impl From<anyhow::Error> for Error {
    fn from(error: anyhow::Error) -> Self {
        Error::Storage(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
