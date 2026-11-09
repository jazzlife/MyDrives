//! FUSE mount provider.
//!
//! Used for development and CI on Linux, and on macOS when the user has macFUSE
//! installed. The kext-free macOS path is [`mydrives-mount-nfs`].
//!
//! Threading: fuser 0.18 hands every callback `&self`, so the session can run
//! several reader threads (`Config::n_threads`). Each callback blocks on the
//! async VFS with `Handle::block_on`, which is safe because fuser's session
//! threads are plain OS threads, not tokio workers. This gives real parallelism
//! (N in-flight requests) without requiring the reply objects to be `Send`.

// On macOS, FUSE means macFUSE (a kernel extension, with licence strings
// attached), so it is opt-in at build time; see the `macfuse` feature.
#![cfg(any(target_os = "linux", all(target_os = "macos", feature = "macfuse")))]

use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fuser::{
    BackgroundSession, BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
    SessionACL, TimeOrNow, WriteFlags,
};
use parking_lot::Mutex;
use tokio::runtime::Handle;

#[cfg(target_os = "macos")]
use mydrives_core::fuse_t;
use mydrives_core::error::{Error, Result};
use mydrives_core::mount::{MountProvider, MountSession, MountSpec, MountTarget};
use mydrives_core::types::{Attr, FileKind, Ino, OpenOpts, SetAttr};
use mydrives_core::vfs::Vfs;

/// How long the kernel may cache an entry/attribute reply. Kept short because the
/// remote can change behind our back; the VFS cache absorbs the repeated calls.
const TTL: Duration = Duration::from_secs(1);
const SESSION_THREADS: usize = 4;

pub struct FuseProvider;

impl FuseProvider {
    pub fn new() -> Self {
        FuseProvider
    }
}

impl Default for FuseProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MountProvider for FuseProvider {
    fn name(&self) -> &'static str {
        "fuse"
    }

    fn probe(&self) -> Result<()> {
        // macOS has two FUSE implementations and they are checked differently:
        // macFUSE is a kernel extension and shows up as `/dev/fuse`, while
        // FUSE-T runs entirely in user space - there is no device node, only
        // the library we are linked against.
        #[cfg(target_os = "macos")]
        {
            if fuse_t::is_installed() {
                return Ok(());
            }
            if !PathBuf::from("/dev/fuse").exists() {
                return Err(Error::Unsupported(format!(
                    "FUSE-T is not installed ({}); it is what puts a drive in Finder's sidebar. \
                     Until then the 'nfs' provider is used, which needs nothing installed",
                    fuse_t::LIBRARY
                )));
            }
            return Ok(());
        }

        #[cfg(not(target_os = "macos"))]
        {
            if !PathBuf::from("/dev/fuse").exists() {
                return Err(Error::Unsupported(
                    "/dev/fuse is missing; load the fuse module or run in a container with --device /dev/fuse".into(),
                ));
            }
            // Unprivileged mounts go through the setuid helper; root can mount directly.
            let euid = unsafe { libc::geteuid() };
            if euid != 0 && which("fusermount3").is_none() && which("fusermount").is_none() {
                return Err(Error::Unsupported(
                    "neither fusermount3 nor fusermount is on PATH, and we are not root".into(),
                ));
            }
            Ok(())
        }
    }

    fn supports(&self, target: &MountTarget) -> bool {
        matches!(target, MountTarget::Path(_))
    }

    fn mount(
        &self,
        spec: &MountSpec,
        vfs: Arc<Vfs>,
        rt: Handle,
    ) -> Result<Box<dyn MountSession>> {
        let path = match &spec.target {
            MountTarget::Path(p) => p.clone(),
            MountTarget::Drive(_) => {
                return Err(Error::InvalidArgument(
                    "FUSE cannot mount on a drive letter".into(),
                ))
            }
        };

        let mut options = vec![
            // Shows up as the device in `df` / Finder, so the user can tell which
            // remote a mount belongs to.
            MountOption::FSName(format!("mydrives-{}", spec.id)),
            MountOption::Subtype("mydrives".into()),
            MountOption::NoAtime,
            MountOption::NoSuid,
            MountOption::NoDev,
        ];
        options.push(if spec.readonly {
            MountOption::RO
        } else {
            MountOption::RW
        });

        // The two macOS FUSE implementations want different things, and they
        // are told apart by which one is installed.
        #[cfg(all(target_os = "macos", feature = "macfuse"))]
        if fuse_t::is_installed() {
            // FUSE-T mounts `fuse-t:/<volname>` over NFS by building the
            // `mount` command as one string and splitting it on spaces, so a
            // name like "거북이 NAS" tears the command in half and the mount
            // hangs. Measured: `mount_nfs ... fuse-t:/dev mount /Volumes/x`.
            // Leaving the name out makes Finder use the mount point's name,
            // which is the share name - exactly what the path already says.
            let _ = &spec.volume_name;
        } else {
            // macFUSE: `local` sets MNT_LOCAL, which is what puts a volume in
            // Finder's sidebar next to the external disks, and `volname` is
            // the name printed there.
            options.push(MountOption::CUSTOM("local".into()));
            if !spec.volume_name.trim().is_empty() {
                options.push(MountOption::CUSTOM(format!(
                    "volname={}",
                    spec.volume_name.trim()
                )));
            }
            // Keeps macOS from littering the remote with `._` sidecar files.
            options.push(MountOption::CUSTOM("noappledouble".into()));
        }

        let mut config = Config::default();
        config.mount_options = options;
        config.acl = SessionACL::Owner;
        // Extra reader threads need the FUSE device to be cloneable
        // (`FUSE_DEV_IOC_CLONE`). FUSE-T is not a device at all - the session
        // is a socket to its helper - so the clones read from a file
        // descriptor that is not theirs and requests go unanswered: the mount
        // hangs on the first STATFS. One reader is correct there; the work
        // still happens on the tokio runtime behind it.
        #[cfg(target_os = "macos")]
        let threads = if fuse_t::is_installed() { 1 } else { SESSION_THREADS };
        #[cfg(not(target_os = "macos"))]
        let threads = SESSION_THREADS;
        config.n_threads = Some(threads);

        let fs = FuseFs {
            vfs,
            rt,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            readonly: spec.readonly,
        };

        let session = fuser::spawn_mount(fs, &path, &config).map_err(|e| {
            Error::Backend(format!("FUSE mount of {} failed: {e}", path.display()))
        })?;

        tracing::info!(mountpoint = %path.display(), "FUSE session started");
        Ok(Box::new(FuseMount {
            path,
            session: Mutex::new(Some(session)),
        }))
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(bin))
            .find(|p| p.is_file())
    })
}

struct FuseMount {
    path: PathBuf,
    session: Mutex<Option<BackgroundSession>>,
}

impl MountSession for FuseMount {
    fn mountpoint(&self) -> String {
        self.path.display().to_string()
    }

    fn unmount(self: Box<Self>) -> Result<()> {
        let session = self.session.lock().take();
        match session {
            Some(s) => s.umount_and_join().map_err(|e| {
                Error::Backend(format!("unmounting {} failed: {e}", self.path.display()))
            }),
            None => Ok(()),
        }
    }
}

impl Drop for FuseMount {
    fn drop(&mut self) {
        // Backstop: a dropped session must not leave a dangling mount point.
        if let Some(session) = self.session.lock().take() {
            tracing::warn!(mountpoint = %self.path.display(), "FUSE mount dropped without unmount()");
            drop(session);
        }
    }
}

struct FuseFs {
    vfs: Arc<Vfs>,
    rt: Handle,
    uid: u32,
    gid: u32,
    readonly: bool,
}

impl FuseFs {
    fn attr(&self, ino: Ino, attr: &Attr) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size: attr.size,
            blocks: attr.size.div_ceil(512),
            atime: attr.atime,
            mtime: attr.mtime,
            ctime: attr.ctime,
            crtime: attr.crtime,
            kind: match attr.kind {
                FileKind::Dir => FileType::Directory,
                FileKind::File => FileType::RegularFile,
            },
            perm: if self.readonly {
                attr.unix_mode() & 0o555
            } else {
                attr.unix_mode()
            },
            nlink: if attr.is_dir() { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}

/// POSIX errno for a core error, in the shape fuser wants.
fn errno(e: &Error) -> Errno {
    match e.errno() {
        2 => Errno::ENOENT,
        5 => Errno::EIO,
        13 => Errno::EACCES,
        16 => Errno::EBUSY,
        17 => Errno::EEXIST,
        20 => Errno::ENOTDIR,
        21 => Errno::EISDIR,
        22 => Errno::EINVAL,
        30 => Errno::EROFS,
        38 => Errno::ENOSYS,
        // Network trouble: ETIMEDOUT/EHOSTDOWN are the honest answers, and both
        // make applications report "the server is not responding" rather than
        // "the file is corrupt".
        110 => Errno::ETIMEDOUT,
        112 => Errno::EIO,
        39 => Errno::ENOTEMPTY,
        _ => Errno::EIO,
    }
}

fn name_of(name: &OsStr) -> String {
    // Remote stores are UTF-16 (SMB) or UTF-8 (everything else); a lossy
    // conversion is the only sane option for the rare invalid sequence, and it
    // must not panic the session thread.
    name.to_string_lossy().to_string()
}

fn time_or_now(t: TimeOrNow) -> SystemTime {
    match t {
        TimeOrNow::SpecificTime(t) => t,
        TimeOrNow::Now => SystemTime::now(),
    }
}

impl fuser::Filesystem for FuseFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name = name_of(name);
        match self.rt.block_on(self.vfs.lookup(parent.0, &name)) {
            Ok((ino, attr)) => reply.entry(&TTL, &self.attr(ino, &attr), Generation(0)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, _nlookup: u64) {
        self.vfs.forget(ino.0);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.rt.block_on(self.vfs.getattr(ino.0)) {
            Ok(attr) => reply.attr(&TTL, &self.attr(ino.0, &attr)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        // chmod/chown are accepted and ignored: network shares have no POSIX
        // ownership of their own, and failing them breaks `cp -p`, rsync and
        // Finder copies for no benefit.
        let set = SetAttr {
            size,
            mtime: mtime.map(time_or_now),
            atime: atime.map(time_or_now),
            readonly: None,
        };
        match self.rt.block_on(self.vfs.setattr(ino.0, set)) {
            Ok(attr) => reply.attr(&TTL, &self.attr(ino.0, &attr)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name = name_of(name);
        match self.rt.block_on(self.vfs.mkdir(parent.0, &name)) {
            Ok((ino, attr)) => reply.entry(&TTL, &self.attr(ino, &attr), Generation(0)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name = name_of(name);
        match self.rt.block_on(self.vfs.unlink(parent.0, &name)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name = name_of(name);
        match self.rt.block_on(self.vfs.rmdir(parent.0, &name)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let name = name_of(name);
        let newname = name_of(newname);
        // POSIX rename replaces the destination; RENAME_NOREPLACE would be the
        // exception, but the kernel only sends it when we advertise support.
        match self.rt.block_on(
            self.vfs
                .rename(parent.0, &name, newparent.0, &newname, true),
        ) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let opts = OpenOpts::from_unix_flags(flags.0);
        match self.rt.block_on(self.vfs.open(ino.0, opts)) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let name = name_of(name);
        let opts = OpenOpts::from_unix_flags(flags);
        match self.rt.block_on(self.vfs.create(parent.0, &name, opts)) {
            Ok((fh, ino, attr)) => reply.created(
                &TTL,
                &self.attr(ino, &attr),
                Generation(0),
                FileHandle(fh),
                FopenFlags::empty(),
            ),
            Err(e) => reply.error(errno(&e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.rt.block_on(self.vfs.read(fh.0, offset, size)) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(errno(&e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.rt.block_on(self.vfs.write(fh.0, offset, data)) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        match self.rt.block_on(self.vfs.flush(fh.0)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        match self.rt.block_on(self.vfs.flush(fh.0)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.rt.block_on(self.vfs.release(fh.0)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let entries = match self.rt.block_on(self.vfs.readdir(ino.0)) {
            Ok(e) => e,
            Err(e) => return reply.error(errno(&e)),
        };
        // "." and ".." are ours to synthesize; the VFS never reports them.
        let parent_ino = match self.vfs.path_of(ino.0).ok().and_then(|p| p.parent()) {
            Some(parent) => self.vfs.intern(&parent, FileKind::Dir),
            None => ino.0,
        };
        let mut all: Vec<(Ino, FileType, String)> = vec![
            (ino.0, FileType::Directory, ".".to_string()),
            (parent_ino, FileType::Directory, "..".to_string()),
        ];
        for (child_ino, entry) in entries {
            all.push((
                child_ino,
                match entry.attr.kind {
                    FileKind::Dir => FileType::Directory,
                    FileKind::File => FileType::RegularFile,
                },
                entry.name,
            ));
        }
        for (i, (child, kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
            // The offset the kernel sends back is "where to resume", so it is the
            // index *after* this entry.
            if reply.add(INodeNo(child), i as u64 + 1, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match self.rt.block_on(self.vfs.statfs()) {
            Ok(st) => {
                let bsize = st.block_size.max(512);
                let blocks = st.total_bytes / bsize as u64;
                let free = st.free_bytes / bsize as u64;
                reply.statfs(blocks, free, free, st.total_files, st.free_files, bsize, 255, bsize)
            }
            Err(e) => reply.error(errno(&e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_reports_why_it_cannot_mount() {
        let p = FuseProvider::new();
        assert_eq!(p.name(), "fuse");
        assert!(p.supports(&MountTarget::Path("/tmp/x".into())));
        assert!(!p.supports(&MountTarget::Drive('Z')));
        // probe() may legitimately fail (no /dev/fuse in a sandbox); it must
        // always explain itself rather than panic.
        if let Err(e) = p.probe() {
            assert!(!e.to_string().is_empty());
        }
    }

    #[test]
    fn errno_mapping_covers_the_cases_the_shell_shows() {
        assert_eq!(errno(&Error::NotFound("x".into())), Errno::ENOENT);
        assert_eq!(errno(&Error::NotEmpty("x".into())), Errno::ENOTEMPTY);
        assert_eq!(errno(&Error::ReadOnly("x".into())), Errno::EROFS);
        assert_eq!(errno(&Error::IsADirectory("x".into())), Errno::EISDIR);
        assert_eq!(errno(&Error::Timeout("x".into())), Errno::ETIMEDOUT);
    }
}
