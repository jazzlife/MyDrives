//! One error type for every backend, mapped to each OS's native error space.
//!
//! Getting this mapping wrong is the single most common reason a user-mode
//! filesystem "works in the shell but breaks in Explorer/Finder": the shell only
//! looks at success/failure, while Explorer and Office branch on the exact error.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("not a directory: {0}")]
    NotADirectory(String),
    #[error("is a directory: {0}")]
    IsADirectory(String),
    #[error("directory not empty: {0}")]
    NotEmpty(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("read-only: {0}")]
    ReadOnly(String),
    #[error("sharing violation: {0}")]
    SharingViolation(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    /// The remote end is unreachable - sleep/wake, Wi-Fi switch, NAS powered off.
    /// Kept separate from `Backend` because the mount manager retries on this one.
    #[error("offline: {0}")]
    Offline(String),
    #[error("timed out: {0}")]
    Timeout(String),
    #[error("backend: {0}")]
    Backend(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    pub fn backend(e: impl fmt::Display) -> Self {
        Error::Backend(e.to_string())
    }

    pub fn unsupported(what: impl fmt::Display) -> Self {
        Error::Unsupported(what.to_string())
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Offline(_) | Error::Timeout(_))
    }

    /// POSIX errno for FUSE / NFS replies.
    pub fn errno(&self) -> i32 {
        match self {
            Error::NotFound(_) => 2,            // ENOENT
            Error::AlreadyExists(_) => 17,      // EEXIST
            Error::NotADirectory(_) => 20,      // ENOTDIR
            Error::IsADirectory(_) => 21,       // EISDIR
            Error::NotEmpty(_) => 39,           // ENOTEMPTY
            Error::PermissionDenied(_) => 13,   // EACCES
            Error::ReadOnly(_) => 30,           // EROFS
            Error::SharingViolation(_) => 16,   // EBUSY
            Error::InvalidArgument(_) => 22,    // EINVAL
            Error::Unsupported(_) => 38,        // ENOSYS
            Error::AuthFailed(_) => 13,         // EACCES
            Error::Offline(_) => 112,           // EHOSTDOWN
            Error::Timeout(_) => 110,           // ETIMEDOUT
            Error::Backend(_) => 5,             // EIO
            Error::Io(e) => e.raw_os_error().unwrap_or(5),
        }
    }

    /// NTSTATUS for the WinFsp / Dokan reply path.
    pub fn nt_status(&self) -> u32 {
        match self {
            Error::NotFound(_) => 0xC000_0034,          // STATUS_OBJECT_NAME_NOT_FOUND
            Error::AlreadyExists(_) => 0xC000_0035,     // STATUS_OBJECT_NAME_COLLISION
            Error::NotADirectory(_) => 0xC000_0103,     // STATUS_NOT_A_DIRECTORY
            Error::IsADirectory(_) => 0xC000_00BA,      // STATUS_FILE_IS_A_DIRECTORY
            Error::NotEmpty(_) => 0xC000_0101,          // STATUS_DIRECTORY_NOT_EMPTY
            Error::PermissionDenied(_) => 0xC000_0022,  // STATUS_ACCESS_DENIED
            Error::ReadOnly(_) => 0xC000_00A2,          // STATUS_MEDIA_WRITE_PROTECTED
            Error::SharingViolation(_) => 0xC000_0043,  // STATUS_SHARING_VIOLATION
            Error::InvalidArgument(_) => 0xC000_000D,   // STATUS_INVALID_PARAMETER
            Error::Unsupported(_) => 0xC000_0002,       // STATUS_NOT_IMPLEMENTED
            Error::AuthFailed(_) => 0xC000_006D,        // STATUS_LOGON_FAILURE
            Error::Offline(_) => 0xC000_023C,           // STATUS_NETWORK_UNREACHABLE
            Error::Timeout(_) => 0xC000_00B5,           // STATUS_IO_TIMEOUT
            Error::Backend(_) => 0xC000_0185,           // STATUS_IO_DEVICE_ERROR
            Error::Io(_) => 0xC000_0185,
        }
    }

    /// Maps an NTSTATUS from an SMB server (or a Windows mount layer) onto our space.
    pub fn from_nt_status(status: u32, ctx: &str) -> Self {
        match status {
            0xC000_0034 | 0xC000_003A | 0xC000_000F => Error::NotFound(ctx.into()),
            0xC000_0035 | 0xC000_00B0 => Error::AlreadyExists(ctx.into()),
            0xC000_0103 => Error::NotADirectory(ctx.into()),
            0xC000_00BA => Error::IsADirectory(ctx.into()),
            0xC000_0101 => Error::NotEmpty(ctx.into()),
            0xC000_0022 | 0xC000_00CA => Error::PermissionDenied(ctx.into()),
            0xC000_00A2 => Error::ReadOnly(ctx.into()),
            0xC000_0043 => Error::SharingViolation(ctx.into()),
            0xC000_006D | 0xC000_006A | 0xC000_0064 | 0xC000_015B => Error::AuthFailed(ctx.into()),
            0xC000_023C | 0xC000_0236 | 0xC000_0205 | 0xC00000B5 => Error::Offline(ctx.into()),
            other => Error::Backend(format!("{ctx} (NTSTATUS 0x{other:08X})")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_both_directions() {
        assert_eq!(Error::NotFound("x".into()).errno(), 2);
        assert_eq!(Error::NotFound("x".into()).nt_status(), 0xC000_0034);
        assert!(matches!(
            Error::from_nt_status(0xC000_0034, "x"),
            Error::NotFound(_)
        ));
        assert!(Error::from_nt_status(0xC000_023C, "x").is_retryable());
        // Unknown statuses must not be silently swallowed as success.
        assert!(matches!(
            Error::from_nt_status(0xC000_0001, "x"),
            Error::Backend(_)
        ));
    }
}
