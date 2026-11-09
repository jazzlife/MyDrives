//! The storage backend contract.
//!
//! Everything a remote store must provide to be mountable. Deliberately small and
//! stateless-per-path: SMB, FTP, WebDAV and the cloud drives all map onto it, and
//! the caching, inode bookkeeping and OS semantics live in the VFS above.

use async_trait::async_trait;

use crate::error::Result;
use crate::path::RPath;
use crate::types::{Attr, Caps, DirEntry, FsStat, OpenOpts};

/// Opaque per-backend open-file identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BFile(pub u64);

#[async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Stable id, used in logs and in the mount manager.
    fn id(&self) -> &str;

    /// Human readable description of where this points ("smb://nas/data").
    fn describe(&self) -> String;

    fn caps(&self) -> Caps;

    /// Establish (or re-establish) the session. Called on mount and on reconnect,
    /// so it must be idempotent.
    async fn connect(&self) -> Result<()>;

    async fn disconnect(&self) -> Result<()>;

    async fn stat(&self, path: &RPath) -> Result<Attr>;

    async fn readdir(&self, path: &RPath) -> Result<Vec<DirEntry>>;

    async fn open(&self, path: &RPath, opts: OpenOpts) -> Result<BFile>;

    /// Create and open a new regular file. Must fail with `AlreadyExists` when the
    /// name is taken - the mount layers rely on that to implement O_EXCL and
    /// FILE_CREATE.
    async fn create(&self, path: &RPath) -> Result<BFile>;

    async fn read_at(&self, f: BFile, offset: u64, len: u32) -> Result<Vec<u8>>;

    async fn write_at(&self, f: BFile, offset: u64, data: &[u8]) -> Result<u32>;

    async fn flush(&self, f: BFile) -> Result<()>;

    async fn close(&self, f: BFile) -> Result<()>;

    /// Truncate or extend. `f` is passed when the caller holds the file open,
    /// which lets backends avoid a second round trip.
    async fn set_len(&self, path: &RPath, f: Option<BFile>, len: u64) -> Result<()>;

    async fn set_attr(&self, path: &RPath, f: Option<BFile>, attr: &crate::types::SetAttr)
        -> Result<()>;

    async fn mkdir(&self, path: &RPath) -> Result<()>;

    async fn unlink(&self, path: &RPath) -> Result<()>;

    async fn rmdir(&self, path: &RPath) -> Result<()>;

    async fn rename(&self, from: &RPath, to: &RPath, replace: bool) -> Result<()>;

    async fn statfs(&self) -> Result<FsStat>;
}
