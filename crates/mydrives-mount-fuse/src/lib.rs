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
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyLock, ReplyOpen, ReplyStatfs,
    ReplyWrite, Request, SessionACL, TimeOrNow, WriteFlags,
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
/// Marks a directory handle, keeping it out of the VFS's file handle numbers.
/// See `opendir`.
const DIR_HANDLE_BIT: u64 = 1 << 63;
/// How long FUSE-T's helper gets to put the volume on the mount point.
///
/// A second or two, once macOS has been told the app may look for devices on
/// the local network. Until that is answered - and it is asked the first time
/// a newly installed app mounts - the Bonjour lookup FUSE-T does sits for
/// about half a minute, so the limit has to be generously past that or the
/// first mount after an install reports a failure that then mounts anyway.
#[cfg(all(target_os = "macos", feature = "macfuse"))]
const MOUNT_TIMEOUT: Duration = Duration::from_secs(90);

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
            //
            // What the sidebar prints, though, is the *server* the volume came
            // from, and that defaults to "fuse-t" for every drive. `location`
            // renames it, so the row reads like the drive.
            options.push(MountOption::CUSTOM(format!(
                "location={}",
                location_name(&spec.volume_name)
            )));
            // Keeps macOS from littering the remote with `._` sidecar files.
            options.push(MountOption::CUSTOM("noappledouble".into()));
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
            locks: Locks::default(),
        };

        let session = fuser::spawn_mount(fs, &path, &config).map_err(|e| {
            Error::Backend(format!("FUSE mount of {} failed: {e}", path.display()))
        })?;

        tracing::info!(mountpoint = %path.display(), "FUSE session started");

        // FUSE-T mounts the volume from its helper *after* the session is up,
        // which takes a second or two. Until then the path is still the empty
        // directory underneath, and whoever we just told "mounted" - the tray,
        // a script, the user - is looking at the wrong thing. Worse, files
        // written into that window disappear under the volume when it lands.
        #[cfg(all(target_os = "macos", feature = "macfuse"))]
        if fuse_t::is_installed() && !wait_until_mounted(&path, MOUNT_TIMEOUT) {
            let _ = session.umount_and_join();
            return Err(Error::Backend(format!(
                "FUSE-T did not finish mounting {} within {} s. If macOS asked \
                 whether MyDrives may find devices on the local network, the \
                 mount is waiting for that answer",
                path.display(),
                MOUNT_TIMEOUT.as_secs()
            )));
        }

        Ok(Box::new(FuseMount {
            path,
            session: Mutex::new(Some(session)),
        }))
    }
}

/// Is `path` the root of a mounted volume right now?
///
/// `statfs` answers for the volume the path lives on, so a mount point that is
/// still a plain directory reports the volume underneath it instead of itself.
#[cfg(all(target_os = "macos", feature = "macfuse"))]
fn is_mount_point(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let Ok(real) = std::fs::canonicalize(path) else {
        return false;
    };
    let Ok(c_path) = std::ffi::CString::new(real.as_os_str().as_bytes()) else {
        return false;
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut st) } != 0 {
        return false;
    }
    let on = unsafe { std::ffi::CStr::from_ptr(st.f_mntonname.as_ptr()) };
    std::path::Path::new(OsStr::from_bytes(on.to_bytes())) == real
}

/// Waits for FUSE-T's helper to finish putting the volume on `path`.
#[cfg(all(target_os = "macos", feature = "macfuse"))]
fn wait_until_mounted(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if is_mount_point(path) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Only the Linux probe looks for the setuid mount helper; macOS has FUSE-T.
#[cfg(not(target_os = "macos"))]
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
    locks: Locks,
}

/// One byte range a program has locked, `start..=end` as POSIX counts it.
#[derive(Clone, Copy, Debug)]
struct LockRange {
    owner: u64,
    start: u64,
    end: u64,
    write: bool,
    pid: u32,
}

impl LockRange {
    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.start <= end && start <= self.end
    }

    fn conflicts_with(&self, owner: u64, start: u64, end: u64, write: bool) -> bool {
        self.owner != owner && (write || self.write) && self.overlaps(start, end)
    }
}

/// POSIX advisory locks, kept in this process.
///
/// A remote share speaks no lock protocol we can forward to, but every program
/// on this machine reaches the drive through this one server - so a table here
/// is real mutual exclusion between them, which is what sqlite and anything
/// else that locks a file is asking for. It is the same answer the kernel
/// gives a FUSE filesystem that does not implement locking itself; without it
/// `sqlite3` cannot so much as create a database on the mount.
#[derive(Default)]
struct Locks {
    by_inode: Mutex<std::collections::HashMap<u64, Vec<LockRange>>>,
}

impl Locks {
    /// The lock that would block this request, if any.
    fn conflict(&self, ino: u64, owner: u64, start: u64, end: u64, write: bool) -> Option<LockRange> {
        let table = self.by_inode.lock();
        table
            .get(&ino)?
            .iter()
            .find(|l| l.conflicts_with(owner, start, end, write))
            .copied()
    }

    /// Takes or releases a range. `Err(())` means another owner holds it.
    fn set(
        &self,
        ino: u64,
        owner: u64,
        start: u64,
        end: u64,
        typ: Option<bool>,
        pid: u32,
    ) -> std::result::Result<(), ()> {
        if let Some(write) = typ {
            if self.conflict(ino, owner, start, end, write).is_some() {
                return Err(());
            }
        }
        let mut table = self.by_inode.lock();
        let ranges = table.entry(ino).or_default();
        // The owner's own ranges give way: a new lock over them replaces them,
        // which is how an upgrade, a downgrade and an unlock all work.
        let mut kept: Vec<LockRange> = Vec::with_capacity(ranges.len() + 2);
        for range in ranges.iter() {
            if range.owner != owner || !range.overlaps(start, end) {
                kept.push(*range);
                continue;
            }
            if range.start < start {
                kept.push(LockRange { end: start - 1, ..*range });
            }
            if range.end > end {
                kept.push(LockRange { start: end + 1, ..*range });
            }
        }
        if let Some(write) = typ {
            kept.push(LockRange { owner, start, end, write, pid });
        }
        if kept.is_empty() {
            table.remove(&ino);
        } else {
            *ranges = kept;
        }
        Ok(())
    }

    /// Drops everything an owner holds on a file. POSIX releases a process's
    /// locks as soon as it closes any descriptor for the file.
    fn release_owner(&self, ino: u64, owner: u64) {
        let mut table = self.by_inode.lock();
        if let Some(ranges) = table.get_mut(&ino) {
            ranges.retain(|l| l.owner != owner);
            if ranges.is_empty() {
                table.remove(&ino);
            }
        }
    }
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
/// The name FUSE-T announces the mount under, which is what Finder prints in
/// the sidebar.
///
/// The name ends up inside a Bonjour service name (`<name>._smb._tcp.local`)
/// that macOS has to resolve before it can mount, so it is **ASCII only**:
/// `거북이-NAS` is accepted by FUSE-T and then never resolves, and the mount
/// dies with "Short read of FUSE request header". Whitespace becomes `-`
/// (FUSE-T splits the mount command on spaces), everything outside
/// `A-Za-z0-9._-` is dropped, and a DNS label has 63 bytes to spend.
///
/// What the user named the drive is what the sidebar says. An earlier version
/// put the application's name in front of every one of them, in case a drive
/// called "거북이 NAS" came out as plain "NAS" and collided with a real NAS on
/// the network. That traded a rare collision for a name nobody asked for on
/// *every* drive - and Bonjour resolves collisions by itself anyway, by
/// appending a number. Only a name with nothing usable left in it falls back to
/// the application's.
// Only the FUSE-T branch calls it; the tests cover it everywhere.
#[cfg_attr(
    not(all(target_os = "macos", feature = "macfuse")),
    allow(dead_code)
)]
fn location_name(volume_name: &str) -> String {
    const APP: &str = "MyDrives";
    const LABEL_MAX: usize = 63;

    let mut out = String::new();
    for ch in volume_name.trim().chars() {
        let ch = if ch.is_whitespace() { '-' } else { ch };
        if !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')) {
            continue;
        }
        if ch == '-' && out.ends_with('-') {
            continue;
        }
        if out.len() == LABEL_MAX {
            break;
        }
        out.push(ch);
    }
    match out.trim_matches(['-', '.', '_']) {
        "" => APP.to_string(),
        name => name.to_string(),
    }
}

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
        ino: INodeNo,
        fh: FileHandle,
        lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // A close releases that program's locks on the file, and flush is where
        // the kernel tells us a descriptor is going away.
        self.locks.release_owner(ino.0, lock_owner.0);
        match self.rt.block_on(self.vfs.flush(fh.0)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn getlk(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: ReplyLock,
    ) {
        let write = typ == libc::F_WRLCK as i32;
        match self.locks.conflict(ino.0, lock_owner.0, start, end, write) {
            Some(l) => reply.locked(
                l.start,
                l.end,
                if l.write { libc::F_WRLCK } else { libc::F_RDLCK } as i32,
                l.pid,
            ),
            None => reply.locked(start, end, libc::F_UNLCK as i32, pid),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setlk(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        _sleep: bool,
        reply: ReplyEmpty,
    ) {
        // `sleep` asks us to wait for the range instead of failing. Answering
        // EAGAIN is the honest reply we can give today: the callers that matter
        // (sqlite, git) retry a busy lock themselves.
        let typ = match typ as i32 {
            t if t == libc::F_UNLCK as i32 => None,
            t if t == libc::F_WRLCK as i32 => Some(true),
            _ => Some(false),
        };
        match self.locks.set(ino.0, lock_owner.0, start, end, typ, pid) {
            Ok(()) => reply.ok(),
            Err(()) => reply.error(Errno::EAGAIN),
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
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Some(owner) = lock_owner {
            self.locks.release_owner(ino.0, owner.0);
        }
        match self.rt.block_on(self.vfs.release(fh.0)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    /// FUSE-T's SMB backend rejects a directory handle of 0 with "bad file
    /// descriptor", so one is handed out even though we do not need it:
    /// `readdir` works from the inode. The kernel never looks at the value.
    ///
    /// It has to stay clear of the VFS's file handles, which start at 1:
    /// FUSE-T keeps one handle table for both, so the root directory's handle
    /// and the first file's handle were the same number. The first copy into a
    /// fresh mount then failed with "fcopyfile failed: Bad file descriptor"
    /// and left a truncated file behind. The top bit is free - the VFS counts
    /// up from 1 - so directories live above it.
    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(DIR_HANDLE_BIT | ino.0), FopenFlags::empty());
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
    fn location_name_survives_the_mount_command_and_bonjour() {
        // The name the user gave the drive is the name the sidebar shows.
        assert_eq!(location_name("QNAP"), "QNAP");
        assert_eq!(location_name("FTP"), "FTP");
        // The space is what used to tear FUSE-T's mount command in half.
        assert_eq!(location_name("  Office  NAS  "), "Office-NAS");
        assert_eq!(location_name("backup/2025:주말"), "backup2025");
        // Bonjour has to resolve the name, and a non-ASCII one never does.
        assert_eq!(location_name("거북이 NAS"), "NAS");
        // Only a name with nothing left in it falls back to the app's own.
        assert_eq!(location_name("거북이"), "MyDrives");
        assert_eq!(location_name("   "), "MyDrives");
        assert_eq!(location_name("///"), "MyDrives");
        assert_eq!(location_name(&"n".repeat(100)).len(), 63);
    }

    #[test]
    fn locks_keep_two_programs_off_the_same_bytes() {
        let locks = Locks::default();
        // sqlite locks a handful of bytes high up in the file, not the whole thing.
        assert!(locks.set(7, 0xa, 1024, 1024, Some(true), 1).is_ok());
        assert!(locks.conflict(7, 0xb, 1024, 1024, false).is_some());
        assert!(locks.conflict(7, 0xb, 1025, 2048, true).is_none());
        // The holder may take the same range again, and two readers share it.
        assert!(locks.set(7, 0xa, 1024, 1024, Some(false), 1).is_ok());
        assert!(locks.set(7, 0xb, 1024, 1024, Some(false), 2).is_ok());
        // A writer has to wait for both readers.
        assert!(locks.set(7, 0xc, 1024, 1024, Some(true), 3).is_err());
        // Unlocking takes the middle out of a range and leaves the ends.
        assert!(locks.set(9, 0xa, 0, 99, Some(true), 1).is_ok());
        assert!(locks.set(9, 0xa, 40, 49, None, 1).is_ok());
        assert!(locks.conflict(9, 0xb, 40, 49, true).is_none());
        assert!(locks.conflict(9, 0xb, 39, 39, true).is_some());
        assert!(locks.conflict(9, 0xb, 50, 50, true).is_some());
        // Closing the file drops what that program held, and nothing else.
        locks.release_owner(7, 0xa);
        assert!(locks.conflict(7, 0xc, 1024, 1024, true).is_some());
        locks.release_owner(7, 0xb);
        assert!(locks.conflict(7, 0xc, 1024, 1024, true).is_none());
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
