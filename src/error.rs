use std::fmt;
use std::io;

// caim's own statuses stay clear of the 1 and 2 most commands use: sysexits(3) and timeout(1)'s.
pub const OK: i32 = 0;
pub const USAGE: i32 = 64;
pub const CANT_CREATE: i32 = 73;
pub const BUSY: i32 = 75;
pub const CONFIG: i32 = 78;
pub const STALLED: i32 = 124;
pub const FAILED: i32 = 125;
pub const CANNOT_EXECUTE: i32 = 126;
pub const NOT_FOUND: i32 = 127;

#[derive(Debug)]
pub enum Error {
    Usage(String),
    Unavailable(String),
    Interrupted(i32),
    Start(i32, String),
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
            Error::Usage(message) | Error::Unavailable(message) | Error::Start(_, message) => f.write_str(message),
            Error::Interrupted(signal) => write!(f, "interrupted by signal {signal}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Missing paths are ordinary output races; any other I/O error must propagate.
pub fn missing(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR))
}

pub fn not_started(errno: i32) -> i32 {
    match errno {
        libc::ENOENT => NOT_FOUND,
        libc::EACCES | libc::ENOEXEC | libc::EPERM => CANNOT_EXECUTE,
        _ => FAILED,
    }
}
