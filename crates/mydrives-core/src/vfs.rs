//! The VFS: one place where remote-storage reality is turned into something an
//! operating system is willing to mount.
//!
//! Responsibilities:
//!   * inode <-> path mapping that survives renames (see [`crate::inode`]),
//!   * open-handle table with per-handle read chunking (a 128 KiB FUSE read or a
//!     64 KiB Explorer read must not become one network round trip each),
//!   * metadata caching with explicit invalidation on every mutation,
//!   * read-cache coherence via a per-path write epoch, so a reader does not serve
//!     stale bytes after another handle wrote,
//!   * capability and read-only enforcement before the backend is bothered.
//!
//! Both shapes of mount layer are served: FUSE/NFS use the inode entry points,
//! WinFsp hands us full paths and uses the `*_path` entry points.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;

use crate::backend::{BFile, Backend};
use crate::cache::MetaCache;
use crate::error::{Error, Result};
use crate::inode::InodeTable;
use crate::path::RPath;
use crate::types::{
    Attr, Caps, DirEntry, Fh, FileKind, FsStat, Ino, OpenOpts, SetAttr, ROOT_INO,
};

#[derive(Clone, Copy, Debug)]
pub struct VfsOptions {
    pub attr_ttl: Duration,
    pub dir_ttl: Duration,
    /// Read granularity against the backend. Reads smaller than this are served
    /// from a per-handle chunk, which is what makes sequential reads over SMB
    /// reach line speed instead of one round trip per 128 KiB.
    pub read_chunk: u32,
    pub readonly: bool,
}

impl Default for VfsOptions {
    fn default() -> Self {
        VfsOptions {
            attr_ttl: Duration::from_secs(2),
            dir_ttl: Duration::from_secs(2),
            read_chunk: 1 << 20,
            readonly: false,
        }
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub struct VfsStats {
    pub reads: u64,
    pub writes: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub backend_calls: u64,
    pub cache_hits: u64,
    pub errors: u64,
    pub open_handles: u64,
    pub inodes: u64,
}

#[derive(Default)]
struct Counters {
    reads: AtomicU64,
    writes: AtomicU64,
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
    backend_calls: AtomicU64,
    cache_hits: AtomicU64,
    errors: AtomicU64,
}

struct CachedChunk {
    start: u64,
    epoch: u64,
    /// How many bytes we asked the backend for. `data.len() < req_len` therefore
    /// means "the file really ends here", which is what lets us answer short reads
    /// and EOF from the cache instead of guessing.
    req_len: u32,
    data: Vec<u8>,
}

impl CachedChunk {
    fn eof_known(&self) -> bool {
        self.data.len() < self.req_len as usize
    }

    fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }

    fn slice(&self, offset: u64, size: u32) -> Option<Vec<u8>> {
        if offset < self.start {
            return None;
        }
        if offset >= self.end() {
            // Past the cached bytes: only answerable when we know it is EOF.
            return if self.eof_known() { Some(Vec::new()) } else { None };
        }
        let rel = (offset - self.start) as usize;
        let end = (rel + size as usize).min(self.data.len());
        if end - rel < size as usize && !self.eof_known() {
            // The request runs past the chunk and more data may exist.
            return None;
        }
        Some(self.data[rel..end].to_vec())
    }
}

struct OpenFile {
    ino: Ino,
    path: RPath,
    file: BFile,
    opts: OpenOpts,
    chunk: Mutex<Option<CachedChunk>>,
    dirty: AtomicBool,
}

pub struct Vfs {
    backend: Arc<dyn Backend>,
    opts: VfsOptions,
    meta: MetaCache,
    inodes: Mutex<InodeTable>,
    handles: Mutex<HashMap<Fh, Arc<OpenFile>>>,
    epochs: Mutex<HashMap<String, u64>>,
    /// Highest offset we have written to a path while it is open.
    ///
    /// The size reported back to the OS must never go below this. A backend can
    /// legitimately answer a stat with a size that trails the writes it just
    /// accepted (SMB servers report what is committed, not what is buffered),
    /// and an NFS client that is told "the file is smaller than what you wrote"
    /// trims its own page cache - the next read then returns the trimmed
    /// version. Observed as the first read after `dd` differing from every later
    /// read, with the server holding the correct bytes all along.
    size_floors: Mutex<HashMap<String, u64>>,
    next_fh: AtomicU64,
    counters: Counters,
}

impl Vfs {
    pub fn new(backend: Arc<dyn Backend>, opts: VfsOptions) -> Arc<Self> {
        Arc::new(Vfs {
            backend,
            meta: MetaCache::new(opts.attr_ttl, opts.dir_ttl),
            opts,
            inodes: Mutex::new(InodeTable::new()),
            handles: Mutex::new(HashMap::new()),
            epochs: Mutex::new(HashMap::new()),
            size_floors: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            counters: Counters::default(),
        })
    }

    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.backend
    }

    pub fn options(&self) -> &VfsOptions {
        &self.opts
    }

    pub fn caps(&self) -> Caps {
        self.backend.caps()
    }

    pub fn stats(&self) -> VfsStats {
        VfsStats {
            reads: self.counters.reads.load(Ordering::Relaxed),
            writes: self.counters.writes.load(Ordering::Relaxed),
            bytes_read: self.counters.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.counters.bytes_written.load(Ordering::Relaxed),
            backend_calls: self.counters.backend_calls.load(Ordering::Relaxed),
            cache_hits: self.counters.cache_hits.load(Ordering::Relaxed),
            errors: self.counters.errors.load(Ordering::Relaxed),
            open_handles: self.handles.lock().len() as u64,
            inodes: self.inodes.lock().len() as u64,
        }
    }

    /// Drops all cached metadata. Called after a reconnect: anything we learned
    /// before the connection dropped may be from a different session.
    pub fn invalidate_all(&self) {
        self.meta.clear();
    }

    // ---------------------------------------------------------------- helpers

    fn count_call<T>(&self, r: Result<T>) -> Result<T> {
        self.counters.backend_calls.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = &r {
            // "Not found" and friends are *answers*, not failures: every Finder
            // or Explorer window probes for .DS_Store, desktop.ini, .Trashes and
            // so on. Counting those would show a brand-new mount as having
            // dozens of errors, which trains the user to ignore the number.
            let expected = matches!(
                e,
                Error::NotFound(_)
                    | Error::AlreadyExists(_)
                    | Error::NotEmpty(_)
                    | Error::IsADirectory(_)
                    | Error::NotADirectory(_)
            );
            if !expected {
                self.counters.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        r
    }

    fn ensure_writable(&self, path: &RPath) -> Result<()> {
        if self.opts.readonly {
            return Err(Error::ReadOnly(path.to_string()));
        }
        if !self.backend.caps().write {
            return Err(Error::ReadOnly(format!(
                "{} (backend is read-only)",
                path
            )));
        }
        Ok(())
    }

    fn epoch(&self, path: &RPath) -> u64 {
        *self.epochs.lock().get(path.as_str()).unwrap_or(&0)
    }

    fn bump_epoch(&self, path: &RPath) -> u64 {
        let mut map = self.epochs.lock();
        let e = map.entry(path.as_str().to_string()).or_insert(0);
        *e += 1;
        *e
    }

    fn handle(&self, fh: Fh) -> Result<Arc<OpenFile>> {
        self.handles
            .lock()
            .get(&fh)
            .cloned()
            .ok_or_else(|| Error::InvalidArgument(format!("stale file handle {fh}")))
    }

    fn insert_handle(&self, h: OpenFile) -> Fh {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().insert(fh, Arc::new(h));
        fh
    }

    pub fn intern(&self, path: &RPath, kind: FileKind) -> Ino {
        self.inodes.lock().intern(path, kind)
    }

    pub fn path_of(&self, ino: Ino) -> Result<RPath> {
        self.inodes
            .lock()
            .get(ino)
            .map(|n| n.path.clone())
            .ok_or_else(|| Error::NotFound(format!("unknown inode {ino}")))
    }

    pub fn kind_of(&self, ino: Ino) -> Option<FileKind> {
        self.inodes.lock().get(ino).map(|n| n.kind)
    }

    /// The inode a path currently maps to, if it has ever been looked up.
    /// Stateless mount layers need this to drop handles they cached by inode.
    pub fn ino_of(&self, path: &RPath) -> Option<Ino> {
        self.inodes.lock().ino_of(path)
    }

    // ------------------------------------------------------------- path API

    /// Applies the write high-water mark (see `size_floors`).
    fn with_floor(&self, path: &RPath, mut attr: Attr) -> Attr {
        if let Some(floor) = self.size_floors.lock().get(path.as_str()) {
            if attr.size < *floor {
                attr.size = *floor;
            }
        }
        attr
    }

    pub async fn stat_path(&self, path: &RPath) -> Result<Attr> {
        if let Some(attr) = self.meta.attr(path) {
            self.counters.cache_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(self.with_floor(path, attr));
        }
        if path.is_root() {
            // Some servers refuse QUERY_INFO on the share root itself; synthesize a
            // directory for those. Connection-level failures must still propagate,
            // or an offline NAS would look like an empty mounted drive.
            let attr = match self.count_call(self.backend.stat(path).await) {
                Ok(a) => a,
                Err(e) if e.is_retryable() => return Err(e),
                Err(_) => Attr::dir(),
            };
            self.meta.put_attr(path, attr);
            return Ok(attr);
        }
        let attr = self.count_call(self.backend.stat(path).await)?;
        self.meta.put_attr(path, attr);
        Ok(self.with_floor(path, attr))
    }

    pub async fn readdir_path(&self, path: &RPath) -> Result<Arc<Vec<DirEntry>>> {
        if let Some(entries) = self.meta.dir(path) {
            self.counters.cache_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(entries);
        }
        let entries = Arc::new(self.count_call(self.backend.readdir(path).await)?);
        self.meta.put_dir(path, entries.clone());
        Ok(entries)
    }

    pub async fn open_path(&self, path: &RPath, opts: OpenOpts) -> Result<Fh> {
        if opts.write || opts.truncate {
            self.ensure_writable(path)?;
        }
        let attr = self.stat_path(path).await?;
        if attr.is_dir() {
            return Err(Error::IsADirectory(path.to_string()));
        }
        // Deliberately *not* refusing a write open on a file whose read-only
        // attribute is set. POSIX keeps an already-open descriptor writable after
        // a chmod, and git relies on exactly that: it chmods a loose object to
        // 0444 and then finishes writing it. A stateless mount layer (NFS) re-opens
        // per request, so enforcing the attribute here turns that into EACCES.
        // Windows does expect the attribute to be enforced - the WinFsp provider
        // does it there, where the semantics belong.
        let file = self.count_call(self.backend.open(path, opts).await)?;
        tracing::debug!(%path, ?opts, size = attr.size, "open");
        if opts.truncate && attr.size != 0 {
            self.count_call(self.backend.set_len(path, Some(file), 0).await)?;
            self.clear_floor(path, Some(0));
            // A truncation shrinks the file, so the cached size must be dropped
            // rather than merged (see `after_write`, which only grows).
            self.meta.invalidate(path);
            self.bump_epoch(path);
        }
        let ino = self.intern(path, FileKind::File);
        Ok(self.insert_handle(OpenFile {
            ino,
            path: path.clone(),
            file,
            opts,
            chunk: Mutex::new(None),
            dirty: AtomicBool::new(false),
        }))
    }

    pub async fn create_path(&self, path: &RPath, opts: OpenOpts) -> Result<(Fh, Ino, Attr)> {
        self.ensure_writable(path)?;
        let file = self.count_call(self.backend.create(path).await)?;
        tracing::debug!(%path, "create");
        self.meta.invalidate(path);
        let attr = Attr::file(0);
        self.meta.put_attr(path, attr);
        self.bump_epoch(path);
        let ino = self.intern(path, FileKind::File);
        let fh = self.insert_handle(OpenFile {
            ino,
            path: path.clone(),
            file,
            opts: OpenOpts {
                read: true,
                write: true,
                ..opts
            },
            chunk: Mutex::new(None),
            dirty: AtomicBool::new(false),
        });
        Ok((fh, ino, attr))
    }

    pub async fn mkdir_path(&self, path: &RPath) -> Result<(Ino, Attr)> {
        self.ensure_writable(path)?;
        self.count_call(self.backend.mkdir(path).await)?;
        self.meta.invalidate(path);
        let attr = Attr::dir();
        self.meta.put_attr(path, attr);
        Ok((self.intern(path, FileKind::Dir), attr))
    }

    pub async fn unlink_path(&self, path: &RPath) -> Result<()> {
        self.ensure_writable(path)?;
        let attr = self.stat_path(path).await?;
        if attr.is_dir() {
            return Err(Error::IsADirectory(path.to_string()));
        }
        self.count_call(self.backend.unlink(path).await)?;
        self.forget_path(path);
        Ok(())
    }

    pub async fn rmdir_path(&self, path: &RPath) -> Result<()> {
        self.ensure_writable(path)?;
        if path.is_root() {
            return Err(Error::PermissionDenied("cannot remove the mount root".into()));
        }
        self.count_call(self.backend.rmdir(path).await)?;
        self.forget_path(path);
        Ok(())
    }

    pub async fn rename_path(&self, from: &RPath, to: &RPath, replace: bool) -> Result<()> {
        self.ensure_writable(from)?;
        if from == to {
            return Ok(());
        }
        if from.is_ancestor_of(to) {
            return Err(Error::InvalidArgument(format!(
                "cannot move {from} into its own subtree {to}"
            )));
        }
        if replace && !self.backend.caps().rename_replace {
            // Emulate rename-over for backends that cannot do it atomically.
            if let Ok(existing) = self.stat_path(to).await {
                if existing.is_dir() {
                    return Err(Error::IsADirectory(to.to_string()));
                }
                self.count_call(self.backend.unlink(to).await)?;
                self.forget_path(to);
            }
        }
        self.count_call(self.backend.rename(from, to, replace).await)?;
        self.meta.invalidate_tree(from);
        self.meta.invalidate_tree(to);
        self.clear_floor(from, None);
        self.clear_floor(to, None);
        self.inodes.lock().rename(from, to);
        // Open handles keep pointing at the backend handle they opened, which is
        // correct: on both Windows and POSIX a rename does not disturb open files.
        Ok(())
    }

    pub async fn setattr_path(&self, path: &RPath, set: SetAttr) -> Result<Attr> {
        if set.is_empty() {
            return self.stat_path(path).await;
        }
        self.ensure_writable(path)?;
        let caps = self.backend.caps();
        if set.size.is_some() && !caps.truncate {
            return Err(Error::unsupported("truncate"));
        }
        if (set.mtime.is_some() || set.atime.is_some()) && !caps.set_times {
            // Silently accepted: refusing utimes() breaks `cp -p`, rsync and
            // Finder copies on backends that simply do not store timestamps.
            tracing::debug!(%path, "backend cannot set times; ignoring");
        }
        tracing::debug!(%path, size = ?set.size, has_mtime = set.mtime.is_some(), "setattr");
        if let Some(len) = set.size {
            self.count_call(self.backend.set_len(path, None, len).await)?;
            self.clear_floor(path, Some(len));
            self.bump_epoch(path);
        }
        if set.mtime.is_some() || set.atime.is_some() || set.readonly.is_some() {
            if caps.set_times || set.readonly.is_some() {
                self.count_call(self.backend.set_attr(path, None, &set).await)?;
            }
        }
        self.meta.invalidate(path);
        self.stat_path(path).await
    }

    pub async fn statfs(&self) -> Result<FsStat> {
        self.count_call(self.backend.statfs().await)
    }

    fn forget_path(&self, path: &RPath) {
        self.meta.invalidate_tree(path);
        self.inodes.lock().remove_tree(path);
        self.epochs.lock().remove(path.as_str());
        self.size_floors.lock().remove(path.as_str());
    }

    /// The file shrank (truncate) or we are done with it: the write high-water
    /// mark must not keep inflating its reported size.
    fn clear_floor(&self, path: &RPath, to: Option<u64>) {
        let mut floors = self.size_floors.lock();
        match to {
            Some(size) => {
                floors.insert(path.as_str().to_string(), size);
            }
            None => {
                floors.remove(path.as_str());
            }
        }
    }

    fn after_write(&self, path: &RPath, new_size_hint: u64) {
        self.bump_epoch(path);
        {
            let mut floors = self.size_floors.lock();
            let floor = floors.entry(path.as_str().to_string()).or_insert(0);
            if *floor < new_size_hint {
                *floor = new_size_hint;
            }
        }
        // Keep the cached attributes plausible while a copy is in flight; the
        // authoritative size is re-read on close.
        if let Some(mut attr) = self.meta.attr(path) {
            attr.size = attr.size.max(new_size_hint);
            attr.mtime = SystemTime::now();
            self.meta.put_attr(path, attr);
        }
    }

    // --------------------------------------------------------------- file IO

    pub async fn read(&self, fh: Fh, offset: u64, size: u32) -> Result<Vec<u8>> {
        let h = self.handle(fh)?;
        if !h.opts.read {
            return Err(Error::PermissionDenied(format!(
                "{} not opened for reading",
                h.path
            )));
        }
        self.counters.reads.fetch_add(1, Ordering::Relaxed);
        let chunk_size = self.opts.read_chunk.max(size);
        let epoch = self.epoch(&h.path);

        // Requests larger than the chunk size bypass the cache entirely.
        if size >= chunk_size {
            let data = self
                .count_call(self.backend.read_at(h.file, offset, size).await)?;
            self.counters
                .bytes_read
                .fetch_add(data.len() as u64, Ordering::Relaxed);
            return Ok(data);
        }

        {
            let guard = h.chunk.lock();
            if let Some(c) = guard.as_ref().filter(|c| c.epoch == epoch) {
                if let Some(data) = c.slice(offset, size) {
                    self.counters.cache_hits.fetch_add(1, Ordering::Relaxed);
                    self.counters
                        .bytes_read
                        .fetch_add(data.len() as u64, Ordering::Relaxed);
                    return Ok(data);
                }
            }
        }

        let base = (offset / chunk_size as u64) * chunk_size as u64;
        // The request may straddle the chunk boundary, so ask for enough to cover
        // it; otherwise a read of the last bytes of a chunk could never be served.
        let want = (((offset - base) as u32).saturating_add(size)).max(chunk_size);
        let data = self.count_call(self.backend.read_at(h.file, base, want).await)?;
        tracing::debug!(
            path = %h.path,
            offset,
            size,
            base,
            got = data.len(),
            "read from backend"
        );
        let chunk = CachedChunk {
            start: base,
            epoch,
            req_len: want,
            data,
        };
        let out = chunk.slice(offset, size).unwrap_or_default();
        // An empty result is never cached. It would claim "the file ends at
        // `base`" for every later offset in this chunk, so one transient short
        // read (a stale handle, a reconnect) would be remembered as EOF and hand
        // out zeros for the rest of the file.
        if !chunk.data.is_empty() {
            *h.chunk.lock() = Some(chunk);
        } else {
            *h.chunk.lock() = None;
        }
        self.counters
            .bytes_read
            .fetch_add(out.len() as u64, Ordering::Relaxed);
        Ok(out)
    }

    pub async fn write(&self, fh: Fh, offset: u64, data: &[u8]) -> Result<u32> {
        let h = self.handle(fh)?;
        if !h.opts.write {
            return Err(Error::PermissionDenied(format!(
                "{} not opened for writing",
                h.path
            )));
        }
        self.ensure_writable(&h.path)?;
        self.counters.writes.fetch_add(1, Ordering::Relaxed);
        let n = self
            .count_call(self.backend.write_at(h.file, offset, data).await)?;
        tracing::debug!(path = %h.path, offset, len = data.len(), written = n, "write");
        h.dirty.store(true, Ordering::Relaxed);
        *h.chunk.lock() = None;
        self.counters
            .bytes_written
            .fetch_add(n as u64, Ordering::Relaxed);
        self.after_write(&h.path, offset + n as u64);
        Ok(n)
    }

    pub async fn truncate(&self, fh: Fh, len: u64) -> Result<()> {
        let h = self.handle(fh)?;
        self.ensure_writable(&h.path)?;
        self.count_call(self.backend.set_len(&h.path, Some(h.file), len).await)?;
        self.clear_floor(&h.path, Some(len));
        *h.chunk.lock() = None;
        self.meta.invalidate(&h.path);
        self.bump_epoch(&h.path);
        Ok(())
    }

    pub async fn flush(&self, fh: Fh) -> Result<()> {
        let h = self.handle(fh)?;
        if !h.dirty.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.count_call(self.backend.flush(h.file).await)?;
        self.meta.invalidate(&h.path);
        Ok(())
    }

    pub async fn release(&self, fh: Fh) -> Result<()> {
        let h = match self.handles.lock().remove(&fh) {
            Some(h) => h,
            None => return Ok(()),
        };
        let dirty = h.dirty.load(Ordering::Relaxed);
        if dirty {
            // Best effort: a failing flush must still close the handle, otherwise we
            // leak a file on the server for the rest of the session.
            if let Err(e) = self.backend.flush(h.file).await {
                tracing::warn!(path = %h.path, error = %e, "flush on close failed");
            }
        }
        let r = self.count_call(self.backend.close(h.file).await);
        if dirty {
            self.meta.invalidate(&h.path);
            self.bump_epoch(&h.path);
        }
        // Once nobody has the file open, the server's own size is authoritative
        // again - keeping the high-water mark would hide a shrink done elsewhere.
        let still_open = self
            .handles
            .lock()
            .values()
            .any(|other| other.path == h.path);
        if !still_open {
            self.clear_floor(&h.path, None);
        }
        r
    }

    pub fn path_of_handle(&self, fh: Fh) -> Result<RPath> {
        Ok(self.handle(fh)?.path.clone())
    }

    /// Closes every open handle. Used on unmount so the server does not keep the
    /// files locked.
    pub async fn release_all(&self) {
        let handles: Vec<Fh> = self.handles.lock().keys().copied().collect();
        for fh in handles {
            let _ = self.release(fh).await;
        }
    }

    // ------------------------------------------------------------- inode API

    pub async fn lookup(&self, parent: Ino, name: &str) -> Result<(Ino, Attr)> {
        let base = self.path_of(parent)?;
        let path = base.join(name);
        let attr = self.stat_path(&path).await?;
        let kind = attr.kind;
        Ok((self.intern(&path, kind), attr))
    }

    pub async fn getattr(&self, ino: Ino) -> Result<Attr> {
        let path = self.path_of(ino)?;
        self.stat_path(&path).await
    }

    pub async fn setattr(&self, ino: Ino, set: SetAttr) -> Result<Attr> {
        let path = self.path_of(ino)?;
        self.setattr_path(&path, set).await
    }

    pub async fn readdir(&self, ino: Ino) -> Result<Vec<(Ino, DirEntry)>> {
        let path = self.path_of(ino)?;
        let entries = self.readdir_path(&path).await?;
        let mut out = Vec::with_capacity(entries.len());
        {
            let mut table = self.inodes.lock();
            for e in entries.iter() {
                let child = path.join(&e.name);
                let ino = table.intern(&child, e.attr.kind);
                out.push((ino, e.clone()));
            }
        }
        Ok(out)
    }

    pub async fn open(&self, ino: Ino, opts: OpenOpts) -> Result<Fh> {
        let path = self.path_of(ino)?;
        self.open_path(&path, opts).await
    }

    pub async fn create(&self, parent: Ino, name: &str, opts: OpenOpts) -> Result<(Fh, Ino, Attr)> {
        let path = self.path_of(parent)?.join(name);
        self.create_path(&path, opts).await
    }

    pub async fn mkdir(&self, parent: Ino, name: &str) -> Result<(Ino, Attr)> {
        let path = self.path_of(parent)?.join(name);
        self.mkdir_path(&path).await
    }

    pub async fn unlink(&self, parent: Ino, name: &str) -> Result<()> {
        let path = self.path_of(parent)?.join(name);
        self.unlink_path(&path).await
    }

    pub async fn rmdir(&self, parent: Ino, name: &str) -> Result<()> {
        let path = self.path_of(parent)?.join(name);
        self.rmdir_path(&path).await
    }

    pub async fn rename(
        &self,
        parent: Ino,
        name: &str,
        new_parent: Ino,
        new_name: &str,
        replace: bool,
    ) -> Result<()> {
        let from = self.path_of(parent)?.join(name);
        let to = self.path_of(new_parent)?.join(new_name);
        self.rename_path(&from, &to, replace).await
    }

    /// The root inode is never forgotten; everything else may be dropped when the
    /// kernel says it no longer references it.
    pub fn forget(&self, ino: Ino) {
        if ino == ROOT_INO {
            return;
        }
        let mut table = self.inodes.lock();
        if let Some(node) = table.get(ino) {
            let path = node.path.clone();
            // Keep inodes that still have an open handle.
            if self.handles.lock().values().any(|h| h.ino == ino) {
                return;
            }
            table.remove_tree(&path);
        }
    }
}
