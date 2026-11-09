//! # mydrives-core
//!
//! Backend-agnostic, OS-agnostic core of the MyDrives network drive agent.
//!
//! ```text
//!   tray / menu bar ──┐
//!   CLI ───── IPC ─────┤
//!                      ▼
//!              MountManager ──── MountProvider  (WinFsp | FUSE | NFS | Dokan)
//!                      │                 ▲
//!                      ▼                 │ OS kernel callbacks
//!                     Vfs  ── inodes, handles, metadata cache, read chunks
//!                      │
//!                      ▼
//!                   Backend  (SMB | FTP | WebDAV | Google Drive | OneDrive | ...)
//! ```
//!
//! The rules that keep this portable:
//!   * backends are async and know nothing about inodes, handles or the OS;
//!   * the VFS owns all caching and OS-independent semantics;
//!   * mount providers own nothing but translation to one kernel API.

pub mod autostart;
pub mod backend;
pub mod backends;
pub mod cache;
pub mod config;
pub mod error;
pub mod inode;
pub mod manager;
pub mod mount;
pub mod naming;
pub mod path;
pub mod secret;
pub mod types;
pub mod update;
pub mod vfs;

pub use backend::{BFile, Backend};
pub use error::{Error, Result};
pub use manager::{BackendFactory, MountManager, MountState, RemoteStatus};
pub use mount::{MountProvider, MountSession, MountSpec, MountTarget};
pub use path::RPath;
pub use types::{
    Attr, Caps, DirEntry, Fh, FileKind, FsStat, Ino, OpenOpts, SetAttr, ROOT_INO,
};
pub use vfs::{Vfs, VfsOptions, VfsStats};

/// Re-exported so mount providers and backends agree on the tokio version.
pub use tokio;
