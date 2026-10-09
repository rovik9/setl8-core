//! One plain error type. Messages are written by hand and never embed raw parser
//! output for secret-bearing inputs (a key file's contents must not reach a terminal).

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Early-return an [`Error`] built from a format string.
#[macro_export]
macro_rules! bail {
    ($($t:tt)*) => {
        return Err($crate::error::Error(format!($($t)*)))
    };
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(format!("i/o error: {}", e.kind()))
    }
}
