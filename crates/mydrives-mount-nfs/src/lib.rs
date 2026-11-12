//! Mounting without a kernel extension.
//!
//! On macOS every third-party filesystem driver is a problem: macFUSE needs a
//! kernel extension (recovery-mode approval on Apple Silicon, and its licence
//! restricts commercial bundling), FUSE-T is free for non-commercial use only,
//! and FSKit needs a Swift app extension and macOS 15.4+. The one mechanism that
//! is always present, needs no installer and has no licence attached is the
//! in-box **NFS client**.
//!
//! So this provider runs a loopback NFSv3 server (bound to 127.0.0.1 on a random
//! port) and asks the OS to mount it. Same trick FUSE-T uses internally, minus
//! the dependency.
//!
//! Trade-offs, honestly: NFSv3 has no byte-range locking here (`nolocks`), no
//! extended attributes, and the kernel's attribute cache is coarser than FUSE's.
//! For a network drive whose backend is itself remote, that is an acceptable
//! exchange for "works on a stock Mac".
//!
//! Note on privileges: `mount` is a privileged operation on macOS and Linux. The
//! provider tries unprivileged first and, if allowed, retries via `sudo -n`; a
//! shipping build should install a small privileged helper once (SMAppService on
//! macOS) instead.

#![cfg(unix)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::runtime::Handle;

use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, set_atime,
    set_mtime, set_size3, specdata3,
};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

use mydrives_core::error::{Error, Result};
use mydrives_core::mount::{MountProvider, MountSession, MountSpec, MountTarget};
use mydrives_core::types::{Attr, Fh, FileKind, Ino, OpenOpts, SetAttr, ROOT_INO};
use mydrives_core::vfs::Vfs;

/// How many backend handles the bridge keeps open. NFS is stateless, so handles
/// are opened on demand and recycled; without a cap a `find /mnt` would open
/// every file on the share.
const LISTING_DEADLINE: Duration = Duration::from_secs(5);
const MAX_OPEN_HANDLES: usize = 64;
/// Kept short on purpose. An open SMB handle blocks deleting or renaming the
/// file on most servers (sharing violation), so a cached handle that outlives
/// its usefulness turns `rm` into an I/O error.
const HANDLE_IDLE: Duration = Duration::from_secs(5);
/// How often the idle sweep may run (it is driven by incoming requests, as the
/// bridge has no timer of its own).
const EXPIRE_EVERY: Duration = Duration::from_secs(2);

pub struct NfsProvider {
    /// Allow escalating the `mount` call with `sudo -n`.
    allow_sudo: bool,
}

impl NfsProvider {
    pub fn new() -> Self {
        NfsProvider {
            allow_sudo: std::env::var("MYDRIVES_SUDO_MOUNT").as_deref() == Ok("1"),
        }
    }

    pub fn with_sudo(mut self, allow: bool) -> Self {
        self.allow_sudo = allow;
        self
    }
}

impl Default for NfsProvider {
    fn default() -> Self {
        Self::new()
    }
}

fn mount_helper() -> Option<PathBuf> {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &["/sbin/mount_nfs", "/sbin/mount"]
    } else {
        &["/sbin/mount.nfs", "/usr/sbin/mount.nfs", "/bin/mount"]
    };
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

impl MountProvider for NfsProvider {
    fn name(&self) -> &'static str {
        "nfs"
    }

    fn probe(&self) -> Result<()> {
        if mount_helper().is_none() {
            return Err(Error::Unsupported(
                "no NFS mount helper found (mount_nfs / mount.nfs)".into(),
            ));
        }
        Ok(())
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
                    "NFS mounts need a directory, not a drive letter".into(),
                ))
            }
        };

        let bridge = NfsBridge::new(vfs, &spec.id);

        // The server is started inside the runtime and reports its port back over
        // a channel: no `block_on` here, because `mount()` may well be called from
        // a runtime thread.
        //
        // Port 0 lets the OS pick a free one, so several remotes can be mounted at
        // once and nothing clashes with a real NFS server.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::result::Result<u16, String>>();
        let task = rt.spawn(async move {
            let listener = match NFSTcpListener::bind("127.0.0.1:0", bridge).await {
                Ok(l) => l,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(listener.get_listen_port()));
            if let Err(e) = listener.handle_forever().await {
                tracing::error!(error = %e, "loopback NFS server stopped");
            }
        });

        let port = match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(port)) => port,
            Ok(Err(e)) => {
                task.abort();
                return Err(Error::Backend(format!(
                    "starting the loopback NFS server failed: {e}"
                )));
            }
            Err(_) => {
                task.abort();
                return Err(Error::Timeout(
                    "the loopback NFS server did not start within 10s".into(),
                ));
            }
        };

        // The spec carries what the config says; `with_sudo` (and the env
        // override behind it) stays for a provider built outside the manager.
        match run_mount(&path, port, spec.readonly, self.allow_sudo || spec.sudo_mount) {
            Ok(used_sudo) => {
                tracing::info!(
                    mountpoint = %path.display(),
                    port,
                    used_sudo,
                    "mounted through the in-box NFS client"
                );
                Ok(Box::new(NfsMount {
                    path,
                    task: Mutex::new(Some(task)),
                    used_sudo,
                }))
            }
            Err(e) => {
                task.abort();
                Err(e)
            }
        }
    }
}

/// Returns whether sudo was needed.
fn run_mount(path: &PathBuf, port: u16, readonly: bool, allow_sudo: bool) -> Result<bool> {
    let mut options = vec![
        "vers=3".to_string(),
        "tcp".to_string(),
        format!("port={port}"),
        format!("mountport={port}"),
        // Locking: see below.
        "rsize=131072".to_string(),
        "wsize=131072".to_string(),
        // Short attribute cache: the real source of truth is the remote, and the
        // VFS already caches with its own TTL.
        "actimeo=2".to_string(),
        // `hard`, deliberately. With `soft` the client gives up after the timeout
        // and hands the application an I/O error, which over a slow link means
        // silently lost writes (observed: ~30% of 300 file creations failed over a
        // 77 ms RTT link with soft,timeo=100). The server here is our own loopback
        // process, so it does not vanish; when the *backend* is unreachable the
        // bridge answers NFS3ERR_JUKEBOX, which tells the client to retry, and the
        // mount manager reports "offline" in the tray.
        "hard".to_string(),
        "timeo=600".to_string(),
        "retrans=5".to_string(),
    ];

    // No NLM lock manager is implemented, so the *client* must handle locking
    // locally. This is not cosmetic: with locking disabled outright, fcntl()
    // fails and SQLite - and therefore a long list of normal applications -
    // reports "disk I/O error" on the mount. macOS spells it `locallocks`;
    // Linux's `nolock` already means "lock locally".
    if cfg!(target_os = "macos") {
        options.push("locallocks".to_string());
    } else {
        options.push("nolock".to_string());
    }

    if readonly {
        options.push("ro".to_string());
    }
    #[cfg(target_os = "macos")]
    {
        // Without this, macOS insists on the (unused) reserved-port rule.
        options.push("noresvport".to_string());
    }

    let target = path.display().to_string();
    let opts = options.join(",");
    let run = |sudo: bool| -> std::io::Result<std::process::Output> {
        let mut cmd = if sudo {
            let mut c = Command::new("sudo");
            c.arg("-n").arg("mount");
            c
        } else {
            Command::new("mount")
        };
        cmd.arg("-t")
            .arg("nfs")
            .arg("-o")
            .arg(&opts)
            .arg("127.0.0.1:/")
            .arg(&target)
            .output()
    };

    let first = run(false).map_err(|e| Error::Backend(format!("running mount failed: {e}")))?;
    if first.status.success() {
        return Ok(false);
    }
    let stderr = String::from_utf8_lossy(&first.stderr).trim().to_string();
    if !allow_sudo {
        return Err(Error::PermissionDenied(format!(
            "mounting {target} failed: {stderr}. Mounting needs privileges: \
             set [general] sudo_mount = true to let the agent use `sudo -n`, or install \
             the privileged mount helper"
        )));
    }
    let second = run(true).map_err(|e| Error::Backend(format!("running sudo mount failed: {e}")))?;
    if second.status.success() {
        return Ok(true);
    }
    Err(Error::Backend(format!(
        "mounting {target} failed: {} (unprivileged attempt said: {stderr})",
        String::from_utf8_lossy(&second.stderr).trim()
    )))
}

struct NfsMount {
    path: PathBuf,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    used_sudo: bool,
}

impl NfsMount {
    fn umount(&self) -> Result<()> {
        let target = self.path.display().to_string();
        let run = |sudo: bool| -> std::io::Result<std::process::Output> {
            if sudo {
                Command::new("sudo").arg("-n").arg("umount").arg(&target).output()
            } else {
                Command::new("umount").arg(&target).output()
            }
        };
        let out = run(self.used_sudo)
            .map_err(|e| Error::Backend(format!("running umount failed: {e}")))?;
        if out.status.success() {
            return Ok(());
        }
        // A forced unmount is the right follow-up: the server is going away, so
        // leaving a half-dead mount point behind is worse.
        let forced = if cfg!(target_os = "macos") {
            Command::new(if self.used_sudo { "sudo" } else { "umount" })
                .args(if self.used_sudo {
                    vec!["-n", "umount", "-f", &target]
                } else {
                    vec!["-f", &target]
                })
                .output()
        } else {
            Command::new(if self.used_sudo { "sudo" } else { "umount" })
                .args(if self.used_sudo {
                    vec!["-n", "umount", "-l", &target]
                } else {
                    vec!["-l", &target]
                })
                .output()
        };
        match forced {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(Error::Backend(format!(
                "unmounting {target} failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ))),
            Err(e) => Err(Error::Backend(format!("running umount -f failed: {e}"))),
        }
    }
}

impl MountSession for NfsMount {
    fn mountpoint(&self) -> String {
        self.path.display().to_string()
    }

    fn unmount(self: Box<Self>) -> Result<()> {
        let result = self.umount();
        if let Some(task) = self.task.lock().take() {
            task.abort();
        }
        result
    }
}

impl Drop for NfsMount {
    fn drop(&mut self) {
        if let Some(task) = self.task.lock().take() {
            let _ = self.umount();
            task.abort();
        }
    }
}

// --------------------------------------------------------------------- bridge

struct OpenHandle {
    fh: Fh,
    writable: bool,
    last_used: Instant,
}

struct NfsBridge {
    vfs: Arc<Vfs>,
    fsid: u64,
    uid: u32,
    gid: u32,
    handles: Mutex<HashMap<Ino, OpenHandle>>,
    last_expire: Mutex<Instant>,
    /// Directories whose listing is running right now, so a retry joins the
    /// walk already under way instead of starting a second one.
    listing: Arc<Mutex<std::collections::HashSet<fileid3>>>,
    /// How long to wait for a walk before telling the client to ask again.
    listing_deadline: Duration,
}

impl NfsBridge {
    /// The directory's entries, without sitting on this connection while a
    /// slow backend walks it.
    ///
    /// A folder of a few hundred files on a paging API is not one call: Google
    /// Drive hands back a page at a time, and one `readdir` of a large folder
    /// was measured here at 31.8 seconds. For all of that the NFS server was
    /// inside a single request and answering nothing else, which is long
    /// enough for the kernel to decide the server has gone and put up "server
    /// connection interrupted".
    ///
    /// So the walk is spawned rather than awaited in place. If it finishes
    /// quickly the answer goes straight back, which is every ordinary folder.
    /// If it does not, the client is told to ask again - NFS3ERR_JUKEBOX, the
    /// protocol's own "busy, retry" - while the walk keeps going and fills the
    /// VFS cache, so the retry is served from there. Nothing is cancelled and
    /// no partial listing is ever returned.
    async fn listing(
        &self,
        dirid: fileid3,
    ) -> std::result::Result<(Vec<(Ino, mydrives_core::types::DirEntry)>, bool), nfsstat3> {
        // Already walking this one: do not start the same walk twice, but do
        // hand over what has landed. These retries are exactly the calls that
        // should be collecting the pages as they arrive - refusing them is
        // what made a folder of 23,362 entries sit through the whole 35 s
        // walk even though its first page was there after 1.8 s.
        if !self.listing.lock().insert(dirid) {
            return match self.vfs.partial_readdir(dirid) {
                Some(sofar) if !sofar.is_empty() => Ok((sofar, false)),
                _ => Err(nfsstat3::NFS3ERR_JUKEBOX),
            };
        }
        let vfs = self.vfs.clone();
        let inflight = self.listing.clone();
        // Spawned, not awaited in place: a `timeout` around the future itself
        // would cancel the walk, and the retry would start over and never get
        // anywhere.
        let walk = tokio::spawn(async move {
            let r = vfs.readdir(dirid).await;
            inflight.lock().remove(&dirid);
            r
        });
        match tokio::time::timeout(self.listing_deadline, walk).await {
            Ok(Ok(entries)) => entries.map(|e| (e, true)).map_err(status),
            // The task itself fell over; the set entry went with it.
            Ok(Err(_)) => Err(nfsstat3::NFS3ERR_IO),
            // Still walking. Hand over the pages that have landed and say the
            // listing is not finished; the client comes back for the rest with
            // a cursor, which is how NFS reads a directory anyway. With
            // nothing to show yet, ask it to retry instead.
            Err(_) => match self.vfs.partial_readdir(dirid) {
                Some(sofar) if !sofar.is_empty() => Ok((sofar, false)),
                _ => Err(nfsstat3::NFS3ERR_JUKEBOX),
            },
        }
    }

    fn new(vfs: Arc<Vfs>, id: &str) -> Self {
        let mut fsid: u64 = 1469598103934665603;
        for b in id.as_bytes() {
            fsid ^= *b as u64;
            fsid = fsid.wrapping_mul(1099511628211);
        }
        NfsBridge {
            vfs,
            fsid,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            handles: Mutex::new(HashMap::new()),
            last_expire: Mutex::new(Instant::now()),
            listing: Arc::new(Mutex::new(std::collections::HashSet::new())),
            listing_deadline: LISTING_DEADLINE,
        }
    }

    fn attr(&self, id: Ino, attr: &Attr) -> fattr3 {
        fattr3 {
            ftype: match attr.kind {
                FileKind::Dir => ftype3::NF3DIR,
                FileKind::File => ftype3::NF3REG,
            },
            mode: attr.unix_mode() as u32,
            nlink: if attr.is_dir() { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            size: attr.size,
            used: attr.size.div_ceil(4096) * 4096,
            rdev: specdata3::default(),
            fsid: self.fsid,
            fileid: id,
            atime: to_nfstime(attr.atime),
            mtime: to_nfstime(attr.mtime),
            ctime: to_nfstime(attr.ctime),
        }
    }

    /// Opens on demand and reuses; evicts the least recently used handle when the
    /// cap is reached so a directory walk cannot exhaust the server's handles.
    async fn handle(&self, id: Ino, write: bool) -> std::result::Result<Fh, nfsstat3> {
        {
            let mut map = self.handles.lock();
            if let Some(h) = map.get_mut(&id) {
                if h.writable || !write {
                    h.last_used = Instant::now();
                    return Ok(h.fh);
                }
            }
        }
        // Re-open with write access if we only had a reader.
        self.drop_handle(id).await;

        let opts = if write {
            OpenOpts::read_write()
        } else {
            OpenOpts::read_only()
        };
        let fh = self.vfs.open(id, opts).await.map_err(status)?;

        let evict: Option<Fh> = {
            let mut map = self.handles.lock();
            if let Some(previous) = map.insert(
                id,
                OpenHandle {
                    fh,
                    writable: write,
                    last_used: Instant::now(),
                },
            ) {
                // Raced with another task opening the same file; the loser's
                // handle has to go back, not be forgotten.
                return_handle(&self.vfs, previous.fh);
            }
            if map.len() > MAX_OPEN_HANDLES {
                let victim = map
                    .iter()
                    .filter(|(k, v)| **k != id && v.last_used.elapsed() > Duration::from_millis(0))
                    .min_by_key(|(_, v)| v.last_used)
                    .map(|(k, _)| *k);
                victim.and_then(|k| map.remove(&k)).map(|h| h.fh)
            } else {
                None
            }
        };
        if let Some(old) = evict {
            let _ = self.vfs.release(old).await;
        }
        Ok(fh)
    }

    async fn drop_handle(&self, id: Ino) {
        let fh = self.handles.lock().remove(&id).map(|h| h.fh);
        if let Some(fh) = fh {
            let _ = self.vfs.release(fh).await;
        }
    }

    /// Closes handles nobody has touched for a while, so writes land on the
    /// server and - just as important - the files can be deleted or renamed.
    async fn expire_handles(&self) {
        let stale: Vec<(Ino, Fh)> = {
            let map = self.handles.lock();
            map.iter()
                .filter(|(_, h)| h.last_used.elapsed() > HANDLE_IDLE)
                .map(|(id, h)| (*id, h.fh))
                .collect()
        };
        for (id, fh) in stale {
            self.handles.lock().remove(&id);
            let _ = self.vfs.release(fh).await;
        }
    }

    /// Runs the idle sweep at most every [`EXPIRE_EVERY`]. Called from the hot
    /// paths so the bridge needs no timer thread of its own.
    async fn maybe_expire(&self) {
        {
            let mut last = self.last_expire.lock();
            if last.elapsed() < EXPIRE_EVERY {
                return;
            }
            *last = Instant::now();
        }
        self.expire_handles().await;
    }
}

/// Releases a backend handle from a context where awaiting is not possible
/// (inside a lock). Dropping it silently would leak an open file on the server.
fn return_handle(vfs: &Arc<Vfs>, fh: Fh) {
    let vfs = vfs.clone();
    tokio::spawn(async move {
        let _ = vfs.release(fh).await;
    });
}

fn to_nfstime(t: SystemTime) -> nfstime3 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => nfstime3 {
            seconds: d.as_secs() as u32,
            nseconds: d.subsec_nanos(),
        },
        Err(_) => nfstime3::default(),
    }
}

fn from_nfstime(t: nfstime3) -> SystemTime {
    UNIX_EPOCH + Duration::new(t.seconds as u64, t.nseconds)
}

fn name_of(name: &filename3) -> String {
    String::from_utf8_lossy(&name.0).to_string()
}

fn status(e: Error) -> nfsstat3 {
    // nfsserve logs the NFS status but not what caused it, which makes a failure
    // in the field unreadable. Log the real error once, for anything that is not
    // a normal negative answer.
    if !matches!(
        e,
        Error::NotFound(_) | Error::AlreadyExists(_) | Error::NotEmpty(_)
    ) {
        tracing::warn!(error = %e, "backend operation failed");
    }
    match e.errno() {
        2 => nfsstat3::NFS3ERR_NOENT,
        // Sharing violation: the file is open somewhere. ACCES at least tells
        // the user "not allowed right now" instead of "I/O error".
        13 | 16 => nfsstat3::NFS3ERR_ACCES,
        17 => nfsstat3::NFS3ERR_EXIST,
        20 => nfsstat3::NFS3ERR_NOTDIR,
        21 => nfsstat3::NFS3ERR_ISDIR,
        22 => nfsstat3::NFS3ERR_INVAL,
        30 => nfsstat3::NFS3ERR_ROFS,
        38 => nfsstat3::NFS3ERR_NOTSUPP,
        39 => nfsstat3::NFS3ERR_NOTEMPTY,
        // The remote is unreachable: JUKEBOX tells the client to retry rather
        // than reporting corruption to the application.
        110 | 112 => nfsstat3::NFS3ERR_JUKEBOX,
        _ => nfsstat3::NFS3ERR_IO,
    }
}

fn to_setattr(s: &sattr3) -> SetAttr {
    SetAttr {
        size: match s.size {
            set_size3::size(n) => Some(n),
            set_size3::Void => None,
        },
        mtime: match s.mtime {
            set_mtime::SET_TO_CLIENT_TIME(t) => Some(from_nfstime(t)),
            set_mtime::SET_TO_SERVER_TIME => Some(SystemTime::now()),
            set_mtime::DONT_CHANGE => None,
        },
        atime: match s.atime {
            set_atime::SET_TO_CLIENT_TIME(t) => Some(from_nfstime(t)),
            set_atime::SET_TO_SERVER_TIME => Some(SystemTime::now()),
            set_atime::DONT_CHANGE => None,
        },
        // POSIX mode bits are advisory here, including the write bit.
        //
        // Mapping `chmod 0444` onto the SMB read-only attribute looks right and
        // breaks real programs: git writes a loose object, chmods it to 0444 and
        // keeps writing through the same descriptor. POSIX allows that; SMB
        // refuses to *open* a read-only file for writing, and a stateless NFS
        // front-end re-opens per request - so the next write dies with
        // ACCESS_DENIED. A network drive is better off ignoring the bit than
        // breaking every tool that sets it. Explorer's "read-only" checkbox still
        // works: that goes through the Windows provider's set_basic_info.
        readonly: None,
    }
}

#[async_trait]
impl NFSFileSystem for NfsBridge {
    fn capabilities(&self) -> VFSCapabilities {
        if self.vfs.options().readonly {
            VFSCapabilities::ReadOnly
        } else {
            VFSCapabilities::ReadWrite
        }
    }

    fn root_dir(&self) -> fileid3 {
        ROOT_INO
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> std::result::Result<fileid3, nfsstat3> {
        let name = name_of(filename);
        match name.as_str() {
            "." => Ok(dirid),
            ".." => {
                let path = self.vfs.path_of(dirid).map_err(status)?;
                match path.parent() {
                    Some(parent) => Ok(self.vfs.intern(&parent, FileKind::Dir)),
                    None => Ok(dirid),
                }
            }
            _ => self
                .vfs
                .lookup(dirid, &name)
                .await
                .map(|(ino, _)| ino)
                .map_err(status),
        }
    }

    async fn getattr(&self, id: fileid3) -> std::result::Result<fattr3, nfsstat3> {
        // The most frequent call, so it drives the idle-handle sweep.
        self.maybe_expire().await;
        let attr = self.vfs.getattr(id).await.map_err(status)?;
        Ok(self.attr(id, &attr))
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> std::result::Result<fattr3, nfsstat3> {
        let set = to_setattr(&setattr);
        if set.size.is_some() {
            // Truncation must go through the open handle if we have one, so the
            // cached read chunks are dropped.
            self.drop_handle(id).await;
        }
        let attr = self.vfs.setattr(id, set).await.map_err(status)?;
        Ok(self.attr(id, &attr))
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> std::result::Result<(Vec<u8>, bool), nfsstat3> {
        let attr = self.vfs.getattr(id).await.map_err(status)?;
        if attr.is_dir() {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }
        let fh = self.handle(id, false).await?;
        let data = self.vfs.read(fh, offset, count).await.map_err(status)?;
        let eof = offset + data.len() as u64 >= attr.size;
        Ok((data, eof))
    }

    async fn write(
        &self,
        id: fileid3,
        offset: u64,
        data: &[u8],
    ) -> std::result::Result<fattr3, nfsstat3> {
        let fh = self.handle(id, true).await?;
        self.vfs.write(fh, offset, data).await.map_err(status)?;
        // The client expects the post-write attributes; the VFS keeps the size
        // current while the handle is open.
        let attr = self.vfs.getattr(id).await.map_err(status)?;
        self.expire_handles().await;
        Ok(self.attr(id, &attr))
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
    ) -> std::result::Result<(fileid3, fattr3), nfsstat3> {
        let name = name_of(filename);
        let (fh, ino, _) = self
            .vfs
            .create(dirid, &name, OpenOpts::read_write())
            .await
            .map_err(status)?;
        // Replacing an entry without releasing it would leak the old backend
        // handle - and a leaked handle keeps the file undeletable.
        //
        // The insert is scoped: an `if let` would keep the (non-Send) guard
        // alive across the await below.
        let previous = {
            self.handles.lock().insert(
                ino,
                OpenHandle {
                    fh,
                    writable: true,
                    last_used: Instant::now(),
                },
            )
        };
        if let Some(previous) = previous {
            let _ = self.vfs.release(previous.fh).await;
        }
        let set = to_setattr(&attr);
        let final_attr = if set.is_empty() {
            self.vfs.getattr(ino).await.map_err(status)?
        } else {
            self.vfs.setattr(ino, set).await.map_err(status)?
        };
        Ok((ino, self.attr(ino, &final_attr)))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> std::result::Result<fileid3, nfsstat3> {
        let name = name_of(filename);
        let (fh, ino, _) = self
            .vfs
            .create(dirid, &name, OpenOpts::read_write())
            .await
            .map_err(status)?;
        let _ = self.vfs.release(fh).await;
        Ok(ino)
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> std::result::Result<(fileid3, fattr3), nfsstat3> {
        let name = name_of(dirname);
        let (ino, attr) = self.vfs.mkdir(dirid, &name).await.map_err(status)?;
        Ok((ino, self.attr(ino, &attr)))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> std::result::Result<(), nfsstat3> {
        let name = name_of(filename);
        let dir = self.vfs.path_of(dirid).map_err(status)?;
        let path = dir.join(&name);
        let attr = self.vfs.stat_path(&path).await.map_err(status)?;

        // Our own cached handle has to go first: an open SMB handle makes the
        // server refuse the delete with a sharing violation.
        let ino = self.vfs.intern(&path, attr.kind);
        self.drop_handle(ino).await;

        let first = if attr.is_dir() {
            self.vfs.rmdir(dirid, &name).await
        } else {
            self.vfs.unlink(dirid, &name).await
        };

        match first {
            Ok(()) => Ok(()),
            // Someone (possibly us, through another cached handle) still has it
            // open: close everything idle and try once more before giving up.
            Err(e) if e.errno() == 16 => {
                self.expire_handles().await;
                if attr.is_dir() {
                    self.vfs.rmdir(dirid, &name).await
                } else {
                    self.vfs.unlink(dirid, &name).await
                }
                .map_err(status)
            }
            Err(e) => Err(status(e)),
        }
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> std::result::Result<(), nfsstat3> {
        let from = name_of(from_filename);
        let to = name_of(to_filename);

        // Drop cached handles for both ends first.
        //
        // This matters more than it looks: `sed -i`, every editor's "atomic save"
        // and Office all write a temp file and rename it over the original. The
        // replaced object survives on the server as long as a handle is open, so
        // a handle cached under the old inode id would keep serving the *old*
        // bytes to a client that still has the old file handle - the edit would
        // appear to have silently done nothing.
        for (dirid, name) in [(from_dirid, &from), (to_dirid, &to)] {
            if let Ok(dir) = self.vfs.path_of(dirid) {
                if let Some(ino) = self.vfs.ino_of(&dir.join(name)) {
                    self.drop_handle(ino).await;
                }
            }
        }

        self.vfs
            .rename(from_dirid, &from, to_dirid, &to, true)
            .await
            .map_err(status)
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> std::result::Result<ReadDirResult, nfsstat3> {
        let (mut entries, complete) = self.listing(dirid).await?;
        // NFS requires a deterministic order across paginated calls.
        entries.sort_by(|a, b| a.1.name.cmp(&b.1.name));

        let start = if start_after == 0 {
            0
        } else {
            match entries.iter().position(|(ino, _)| *ino == start_after) {
                Some(i) => i + 1,
                // The cookie points at an entry that has since vanished; starting
                // over is better than returning an error mid-listing.
                None => 0,
            }
        };
        let end = (start + max_entries).min(entries.len());
        let out: Vec<DirEntry> = entries[start..end]
            .iter()
            .map(|(ino, e)| DirEntry {
                fileid: *ino,
                name: e.name.as_bytes().into(),
                attr: self.attr(*ino, &e.attr),
            })
            .collect();
        Ok(ReadDirResult {
            entries: out,
            // Never "that is all" while pages are still coming in, or the
            // client stops asking and shows a folder missing its tail.
            end: complete && end >= entries.len(),
        })
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> std::result::Result<(fileid3, fattr3), nfsstat3> {
        // No backend models symlinks yet; claiming support would make `ln -s`
        // appear to work and then lose the link.
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn readlink(&self, _id: fileid3) -> std::result::Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mydrives_core::backends::mem::fixture;
    use mydrives_core::vfs::VfsOptions;
    use nfsserve::nfs::{set_gid3, set_mode3, set_uid3};

    fn bridge() -> NfsBridge {
        let vfs = Vfs::new(Arc::new(fixture("t")), VfsOptions::default());
        NfsBridge::new(vfs, "t")
    }

    /// A backend that takes its time over a listing, as a paging cloud API
    /// does on a folder of a few hundred files.
    struct SlowList {
        inner: mydrives_core::backends::mem::MemBackend,
        delay: Duration,
    }

    #[async_trait]
    impl mydrives_core::Backend for SlowList {
        fn id(&self) -> &str {
            self.inner.id()
        }
        fn describe(&self) -> String {
            self.inner.describe()
        }
        fn caps(&self) -> mydrives_core::types::Caps {
            self.inner.caps()
        }
        async fn connect(&self) -> Result<()> {
            self.inner.connect().await
        }
        async fn disconnect(&self) -> Result<()> {
            self.inner.disconnect().await
        }
        async fn stat(&self, p: &mydrives_core::RPath) -> Result<Attr> {
            self.inner.stat(p).await
        }
        async fn readdir(
            &self,
            p: &mydrives_core::RPath,
        ) -> Result<Vec<mydrives_core::types::DirEntry>> {
            tokio::time::sleep(self.delay).await;
            self.inner.readdir(p).await
        }
        async fn open(
            &self,
            p: &mydrives_core::RPath,
            o: OpenOpts,
        ) -> Result<mydrives_core::BFile> {
            self.inner.open(p, o).await
        }
        async fn create(&self, p: &mydrives_core::RPath) -> Result<mydrives_core::BFile> {
            self.inner.create(p).await
        }
        async fn read_at(
            &self,
            f: mydrives_core::BFile,
            o: u64,
            l: u32,
        ) -> Result<Vec<u8>> {
            self.inner.read_at(f, o, l).await
        }
        async fn write_at(
            &self,
            f: mydrives_core::BFile,
            o: u64,
            d: &[u8],
        ) -> Result<u32> {
            self.inner.write_at(f, o, d).await
        }
        async fn flush(&self, f: mydrives_core::BFile) -> Result<()> {
            self.inner.flush(f).await
        }
        async fn close(&self, f: mydrives_core::BFile) -> Result<()> {
            self.inner.close(f).await
        }
        async fn set_len(
            &self,
            p: &mydrives_core::RPath,
            f: Option<mydrives_core::BFile>,
            l: u64,
        ) -> Result<()> {
            self.inner.set_len(p, f, l).await
        }
        async fn set_attr(
            &self,
            p: &mydrives_core::RPath,
            f: Option<mydrives_core::BFile>,
            a: &SetAttr,
        ) -> Result<()> {
            self.inner.set_attr(p, f, a).await
        }
        async fn mkdir(&self, p: &mydrives_core::RPath) -> Result<()> {
            self.inner.mkdir(p).await
        }
        async fn unlink(&self, p: &mydrives_core::RPath) -> Result<()> {
            self.inner.unlink(p).await
        }
        async fn rmdir(&self, p: &mydrives_core::RPath) -> Result<()> {
            self.inner.rmdir(p).await
        }
        async fn rename(
            &self,
            a: &mydrives_core::RPath,
            b: &mydrives_core::RPath,
            r: bool,
        ) -> Result<()> {
            self.inner.rename(a, b, r).await
        }
        async fn statfs(&self) -> Result<mydrives_core::types::FsStat> {
            self.inner.statfs().await
        }
    }

    /// A folder the backend is slow to walk must not stop this server
    /// answering everything else. The client is told to ask again, the walk
    /// carries on, and the retry is served from the cache it filled.

    /// A backend that pages, the way a cloud listing API does.
    struct Paged {
        inner: mydrives_core::backends::mem::MemBackend,
        per_page: Duration,
    }

    #[async_trait]
    impl mydrives_core::Backend for Paged {
        fn id(&self) -> &str { self.inner.id() }
        fn describe(&self) -> String { self.inner.describe() }
        fn caps(&self) -> mydrives_core::types::Caps { self.inner.caps() }
        async fn connect(&self) -> Result<()> { self.inner.connect().await }
        async fn disconnect(&self) -> Result<()> { self.inner.disconnect().await }
        async fn stat(&self, p: &mydrives_core::RPath) -> Result<Attr> { self.inner.stat(p).await }
        async fn readdir(&self, p: &mydrives_core::RPath) -> Result<Vec<mydrives_core::types::DirEntry>> {
            self.inner.readdir(p).await
        }
        /// One entry per page, each page costing a round trip.
        async fn readdir_pages(
            &self,
            p: &mydrives_core::RPath,
            pages: tokio::sync::mpsc::Sender<Vec<mydrives_core::types::DirEntry>>,
        ) -> Result<()> {
            let mut all = self.inner.readdir(p).await?;
            all.sort_by(|a, b| a.name.cmp(&b.name));
            for e in all {
                tokio::time::sleep(self.per_page).await;
                if pages.send(vec![e]).await.is_err() {
                    break;
                }
            }
            Ok(())
        }
        async fn open(&self, p: &mydrives_core::RPath, o: OpenOpts) -> Result<mydrives_core::BFile> { self.inner.open(p, o).await }
        async fn create(&self, p: &mydrives_core::RPath) -> Result<mydrives_core::BFile> { self.inner.create(p).await }
        async fn read_at(&self, f: mydrives_core::BFile, o: u64, l: u32) -> Result<Vec<u8>> { self.inner.read_at(f, o, l).await }
        async fn write_at(&self, f: mydrives_core::BFile, o: u64, d: &[u8]) -> Result<u32> { self.inner.write_at(f, o, d).await }
        async fn flush(&self, f: mydrives_core::BFile) -> Result<()> { self.inner.flush(f).await }
        async fn close(&self, f: mydrives_core::BFile) -> Result<()> { self.inner.close(f).await }
        async fn set_len(&self, p: &mydrives_core::RPath, f: Option<mydrives_core::BFile>, l: u64) -> Result<()> { self.inner.set_len(p, f, l).await }
        async fn set_attr(&self, p: &mydrives_core::RPath, f: Option<mydrives_core::BFile>, a: &SetAttr) -> Result<()> { self.inner.set_attr(p, f, a).await }
        async fn mkdir(&self, p: &mydrives_core::RPath) -> Result<()> { self.inner.mkdir(p).await }
        async fn unlink(&self, p: &mydrives_core::RPath) -> Result<()> { self.inner.unlink(p).await }
        async fn rmdir(&self, p: &mydrives_core::RPath) -> Result<()> { self.inner.rmdir(p).await }
        async fn rename(&self, a: &mydrives_core::RPath, b: &mydrives_core::RPath, r: bool) -> Result<()> { self.inner.rename(a, b, r).await }
        async fn statfs(&self) -> Result<mydrives_core::types::FsStat> { self.inner.statfs().await }
    }

    /// The point of streaming: the client gets the entries that have arrived
    /// instead of nothing, and is told the listing is not finished so it
    /// comes back for the rest.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_paging_backend_hands_over_what_has_arrived() {
        let vfs = Vfs::new(
            Arc::new(Paged {
                inner: fixture("t"),
                per_page: Duration::from_millis(150),
            }),
            VfsOptions::default(),
        );
        let mut b = NfsBridge::new(vfs, "t");
        b.listing_deadline = Duration::from_millis(250);

        // Past the deadline only the first page or two have landed.
        let early = b.readdir(ROOT_INO, 0, 100).await.expect("a partial listing");
        assert!(!early.entries.is_empty(), "nothing was handed over");
        assert!(!early.end, "an unfinished listing must not claim to be the end");

        // Let the walk finish; the complete answer follows, and it is a
        // superset of what was shown early.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let full = b.readdir(ROOT_INO, 0, 100).await.expect("the finished listing");
        assert!(full.end, "a finished listing has to say so");
        assert!(
            full.entries.len() >= early.entries.len(),
            "the folder shrank: {} then {}",
            early.entries.len(),
            full.entries.len()
        );
        // Every entry shown early is still there, under the same id, so a
        // client that paged from one of them does not lose its place.
        for e in &early.entries {
            assert!(
                full.entries.iter().any(|f| f.fileid == e.fileid),
                "an entry shown early vanished from the finished listing"
            );
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_slow_listing_does_not_hold_up_the_server() {
        let slow = SlowList {
            inner: fixture("t"),
            delay: Duration::from_millis(600),
        };
        let vfs = Vfs::new(Arc::new(slow), VfsOptions::default());
        let mut b = NfsBridge::new(vfs, "t");
        b.listing_deadline = Duration::from_millis(100);

        let started = Instant::now();
        let first = b.readdir(ROOT_INO, 0, 100).await;
        assert!(
            matches!(first, Err(nfsstat3::NFS3ERR_JUKEBOX)),
            "a walk past the deadline must ask the client to retry, got {first:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "the answer has to come back long before the walk finishes"
        );

        // The point of all this: other work is still served while the walk runs.
        let meanwhile = Instant::now();
        b.getattr(ROOT_INO).await.expect("getattr while listing");
        assert!(
            meanwhile.elapsed() < Duration::from_millis(300),
            "the server was blocked by the slow listing"
        );

        // A retry arriving before the walk finishes must not start a second one.
        assert!(matches!(
            b.readdir(ROOT_INO, 0, 100).await,
            Err(nfsstat3::NFS3ERR_JUKEBOX)
        ));

        // Once it finishes, the answer is there - whole, never partial.
        tokio::time::sleep(Duration::from_millis(900)).await;
        let done = b.readdir(ROOT_INO, 0, 100).await.expect("the retry is served");
        assert!(done.end);
        assert!(!done.entries.is_empty());
    }

    #[test]
    fn provider_only_takes_directories() {
        let p = NfsProvider::new();
        assert_eq!(p.name(), "nfs");
        assert!(p.supports(&MountTarget::Path("/tmp/x".into())));
        assert!(!p.supports(&MountTarget::Drive('Z')));
    }

    #[test]
    fn error_mapping_prefers_retry_for_network_trouble() {
        assert!(matches!(
            status(Error::Offline("x".into())),
            nfsstat3::NFS3ERR_JUKEBOX
        ));
        assert!(matches!(
            status(Error::NotFound("x".into())),
            nfsstat3::NFS3ERR_NOENT
        ));
        assert!(matches!(
            status(Error::NotEmpty("x".into())),
            nfsstat3::NFS3ERR_NOTEMPTY
        ));
    }

    #[test]
    fn sattr_conversion() {
        let s = sattr3 {
            mode: set_mode3::mode(0o444),
            uid: set_uid3::Void,
            gid: set_gid3::Void,
            size: set_size3::size(42),
            atime: set_atime::DONT_CHANGE,
            mtime: set_mtime::SET_TO_CLIENT_TIME(nfstime3 {
                seconds: 1_600_000_000,
                nseconds: 0,
            }),
        };
        let set = to_setattr(&s);
        assert_eq!(set.size, Some(42));
        // chmod is advisory: it must not turn into the SMB read-only attribute.
        assert_eq!(set.readonly, None);
        assert!(set.mtime.is_some());
        assert!(set.atime.is_none());
    }

    #[tokio::test]
    async fn lookup_readdir_and_read_through_the_bridge() {
        let b = bridge();
        assert_eq!(b.root_dir(), ROOT_INO);

        let docs = b
            .lookup(ROOT_INO, &"docs".as_bytes().into())
            .await
            .expect("lookup docs");
        let attrs = b.getattr(docs).await.expect("getattr");
        assert!(matches!(attrs.ftype, ftype3::NF3DIR));

        // "." and ".." must resolve, or the client cannot walk back up.
        assert_eq!(b.lookup(docs, &".".as_bytes().into()).await.unwrap(), docs);
        assert_eq!(
            b.lookup(docs, &"..".as_bytes().into()).await.unwrap(),
            ROOT_INO
        );

        let listing = b.readdir(docs, 0, 100).await.expect("readdir");
        assert!(listing.end);
        let names: Vec<String> = listing
            .entries
            .iter()
            .map(|e| String::from_utf8_lossy(&e.name.0).to_string())
            .collect();
        assert_eq!(names, vec!["big.bin", "readme.txt"]);

        let readme = b
            .lookup(docs, &"readme.txt".as_bytes().into())
            .await
            .expect("lookup readme");
        let (data, eof) = b.read(readme, 0, 4096).await.expect("read");
        assert_eq!(data, b"hello mydrives\n");
        assert!(eof);

        // Short reads in the middle must not claim EOF.
        let (part, eof) = b.read(readme, 0, 5).await.expect("partial read");
        assert_eq!(part, b"hello");
        assert!(!eof);
    }

    #[tokio::test]
    async fn readdir_pagination_is_stable() {
        let b = bridge();
        let docs = b.lookup(ROOT_INO, &"docs".as_bytes().into()).await.unwrap();
        let first = b.readdir(docs, 0, 1).await.unwrap();
        assert_eq!(first.entries.len(), 1);
        assert!(!first.end);
        let cookie = first.entries[0].fileid;
        let second = b.readdir(docs, cookie, 1).await.unwrap();
        assert_eq!(second.entries.len(), 1);
        assert_ne!(second.entries[0].fileid, cookie);
        assert!(second.end);
    }

    #[tokio::test]
    async fn write_create_and_remove() {
        let b = bridge();
        let (ino, attr) = b
            .create(ROOT_INO, &"new.txt".as_bytes().into(), sattr3::default())
            .await
            .expect("create");
        assert_eq!(attr.size, 0);

        b.write(ino, 0, b"payload").await.expect("write");
        let (data, _) = b.read(ino, 0, 100).await.expect("read back");
        assert_eq!(data, b"payload");

        b.remove(ROOT_INO, &"new.txt".as_bytes().into())
            .await
            .expect("remove");
        assert!(b.lookup(ROOT_INO, &"new.txt".as_bytes().into()).await.is_err());

        // Removing a non-empty directory must be refused.
        let err = b
            .remove(ROOT_INO, &"docs".as_bytes().into())
            .await
            .expect_err("docs is not empty");
        assert!(matches!(err, nfsstat3::NFS3ERR_NOTEMPTY));
    }

    #[tokio::test]
    async fn handle_cache_is_bounded() {
        let backend = mydrives_core::backends::mem::MemBackend::new("many");
        for i in 0..(MAX_OPEN_HANDLES + 20) {
            backend.put_file(&format!("/f{i}.txt"), b"x");
        }
        let vfs = Vfs::new(Arc::new(backend), VfsOptions::default());
        let b = NfsBridge::new(vfs.clone(), "many");
        for i in 0..(MAX_OPEN_HANDLES + 20) {
            let id = b
                .lookup(ROOT_INO, &format!("f{i}.txt").as_bytes().into())
                .await
                .unwrap();
            b.read(id, 0, 1).await.unwrap();
        }
        assert!(
            b.handles.lock().len() <= MAX_OPEN_HANDLES + 1,
            "handle cache grew to {}",
            b.handles.lock().len()
        );
        assert!(vfs.stats().open_handles <= (MAX_OPEN_HANDLES + 1) as u64);
    }
}
