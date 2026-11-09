//! Types shared by backends, the VFS and the mount layers.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type Ino = u64;
pub type Fh = u64;

/// FUSE and NFS both insist the root inode is 1.
pub const ROOT_INO: Ino = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
}

#[derive(Clone, Copy, Debug)]
pub struct Attr {
    pub kind: FileKind,
    pub size: u64,
    pub crtime: SystemTime,
    pub atime: SystemTime,
    pub mtime: SystemTime,
    pub ctime: SystemTime,
    pub readonly: bool,
    pub hidden: bool,
}

impl Attr {
    pub fn new(kind: FileKind, size: u64, mtime: SystemTime) -> Self {
        Attr {
            kind,
            size,
            crtime: mtime,
            atime: mtime,
            mtime,
            ctime: mtime,
            readonly: false,
            hidden: false,
        }
    }

    pub fn dir() -> Self {
        Attr::new(FileKind::Dir, 0, SystemTime::now())
    }

    pub fn file(size: u64) -> Self {
        Attr::new(FileKind::File, size, SystemTime::now())
    }

    pub fn is_dir(&self) -> bool {
        self.kind == FileKind::Dir
    }

    /// Unix mode bits a mount layer should report. Network filesystems have no
    /// POSIX ownership of their own, so permissions are derived from the
    /// read-only flag and handed the mounting user's uid/gid by the adapter.
    pub fn unix_mode(&self) -> u16 {
        match (self.kind, self.readonly) {
            (FileKind::Dir, false) => 0o755,
            (FileKind::Dir, true) => 0o555,
            (FileKind::File, false) => 0o644,
            (FileKind::File, true) => 0o444,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: String,
    pub attr: Attr,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenOpts {
    pub read: bool,
    pub write: bool,
    pub truncate: bool,
    pub append: bool,
}

impl OpenOpts {
    pub fn read_only() -> Self {
        OpenOpts {
            read: true,
            ..Default::default()
        }
    }

    pub fn read_write() -> Self {
        OpenOpts {
            read: true,
            write: true,
            ..Default::default()
        }
    }

    /// Builds options from POSIX open(2) flags handed over by FUSE.
    pub fn from_unix_flags(flags: i32) -> Self {
        const O_ACCMODE: i32 = 0o3;
        const O_WRONLY: i32 = 0o1;
        const O_RDWR: i32 = 0o2;
        const O_TRUNC: i32 = 0o1000;
        const O_APPEND: i32 = 0o2000;
        let acc = flags & O_ACCMODE;
        OpenOpts {
            read: acc != O_WRONLY,
            write: acc == O_WRONLY || acc == O_RDWR,
            truncate: flags & O_TRUNC != 0,
            append: flags & O_APPEND != 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SetAttr {
    pub size: Option<u64>,
    pub mtime: Option<SystemTime>,
    pub atime: Option<SystemTime>,
    pub readonly: Option<bool>,
}

impl SetAttr {
    pub fn is_empty(&self) -> bool {
        self.size.is_none()
            && self.mtime.is_none()
            && self.atime.is_none()
            && self.readonly.is_none()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FsStat {
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub block_size: u32,
    pub total_files: u64,
    pub free_files: u64,
}

impl Default for FsStat {
    fn default() -> Self {
        FsStat {
            total_bytes: 0,
            free_bytes: 0,
            block_size: 4096,
            total_files: 0,
            free_files: 0,
        }
    }
}

/// What a backend can actually do. The VFS refuses operations up front instead of
/// letting the user discover halfway through a copy that the remote cannot set
/// timestamps.
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    pub write: bool,
    pub set_times: bool,
    pub truncate: bool,
    pub rename_replace: bool,
    pub case_sensitive: bool,
    /// Largest read/write the backend handles well in one call.
    pub max_io: u32,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            write: true,
            set_times: true,
            truncate: true,
            rename_replace: true,
            case_sensitive: false,
            max_io: 1 << 20,
        }
    }
}

/// Windows FILETIME (100 ns ticks since 1601-01-01) <-> SystemTime, needed by the
/// SMB backend and the Windows mount layer.
pub fn system_time_to_filetime(t: SystemTime) -> u64 {
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() + EPOCH_DIFF_SECS) * 10_000_000 + (d.subsec_nanos() as u64) / 100,
        Err(_) => 0,
    }
}

pub fn filetime_to_system_time(ft: u64) -> SystemTime {
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
    if ft == 0 {
        return UNIX_EPOCH;
    }
    let secs = ft / 10_000_000;
    let nanos = ((ft % 10_000_000) * 100) as u32;
    if secs < EPOCH_DIFF_SECS {
        return UNIX_EPOCH;
    }
    UNIX_EPOCH + Duration::new(secs - EPOCH_DIFF_SECS, nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_roundtrip() {
        let t = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_700);
        let ft = system_time_to_filetime(t);
        let back = filetime_to_system_time(ft);
        assert_eq!(back, t);
        assert_eq!(filetime_to_system_time(0), UNIX_EPOCH);
    }

    #[test]
    fn unix_open_flags() {
        let o = OpenOpts::from_unix_flags(0o2 | 0o1000);
        assert!(o.read && o.write && o.truncate);
        let o = OpenOpts::from_unix_flags(0);
        assert!(o.read && !o.write);
        let o = OpenOpts::from_unix_flags(0o1);
        assert!(!o.read && o.write);
    }
}
