use std::fmt;
use std::io;

pub const PASS: i32 = 0;
pub const FAIL: i32 = 1;
pub const SETUP: i32 = 2;
pub const VOID: i32 = 3;
pub const BLOCKED: i32 = 4;
pub const USAGE: i32 = 64;

pub const UNAVAILABLE: i32 = 1;
pub const PROCESS_GONE: i32 = 2;

#[derive(Debug)]
pub enum Error {
    Usage(String),
    Unavailable(String),
    Interrupted(i32),
}

impl Error {
    pub fn os(operation: impl fmt::Display) -> Self {
        Error::io(operation, io::Error::last_os_error())
    }
    pub fn io(operation: impl fmt::Display, error: io::Error) -> Self {
        Error::Unavailable(format!("{operation}: {error}"))
    }
    pub fn errno(operation: impl fmt::Display, code: i32) -> Self {
        Error::io(operation, io::Error::from_raw_os_error(code))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(message) | Error::Unavailable(message) => f.write_str(message),
            Error::Interrupted(signal) => write!(f, "interrupted by signal {signal}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Missing paths are ordinary output races; any other I/O error must propagate.
pub fn missing(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR))
}
