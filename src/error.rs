//! One error type for the whole CLI. The kind decides the exit code.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Bad arguments or a local problem (files, directories). Exit code 1.
    Usage,
    /// Missing or invalid credentials, keyring trouble. Exit code 2.
    Auth,
    /// The Gmail or OAuth API returned an error or was unreachable. Exit code 3.
    Api,
}

#[derive(Debug)]
pub struct Error {
    pub kind: Kind,
    pub message: String,
    pub hint: Option<String>,
    /// HTTP status, when the error came from an API response.
    pub status: Option<u16>,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    fn new(kind: Kind, message: impl Into<String>) -> Self {
        Error {
            kind,
            message: message.into(),
            hint: None,
            status: None,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(Kind::Usage, message)
    }

    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(Kind::Auth, message)
    }

    pub fn api(message: impl Into<String>) -> Self {
        Self::new(Kind::Api, message)
    }

    /// A local I/O failure, with what we were doing at the time.
    pub fn io(context: impl fmt::Display, err: std::io::Error) -> Self {
        Self::usage(format!("{context}: {err}"))
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn exit_code(&self) -> i32 {
        match self.kind {
            Kind::Usage => 1,
            Kind::Auth => 2,
            Kind::Api => 3,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
