//! A small error type carrying a human readable message.
//!
//! Every failure in Remounty ends up in front of the user (menu, alert or log),
//! so errors are kept as descriptive text with optional context layers instead
//! of a deep type hierarchy.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::new(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Adds a description of what was being attempted to an error.
pub trait Context<T> {
    fn context(self, what: impl fmt::Display) -> Result<T>;
}

impl<T, E: fmt::Display> Context<T> for std::result::Result<T, E> {
    fn context(self, what: impl fmt::Display) -> Result<T> {
        self.map_err(|err| Error::new(format!("{what}: {err}")))
    }
}

impl<T> Context<T> for Option<T> {
    fn context(self, what: impl fmt::Display) -> Result<T> {
        self.ok_or_else(|| Error::new(what.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_wraps_message() {
        let res: std::result::Result<(), &str> = Err("boom");
        let err = res.context("doing things").err();
        assert_eq!(err, Some(Error::new("doing things: boom")));
    }

    #[test]
    fn option_context() {
        let value: Option<u8> = None;
        assert_eq!(value.context("missing").err(), Some(Error::new("missing")));
    }
}
