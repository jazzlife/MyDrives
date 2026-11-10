//! FTP / FTPS backend - the "web host, router, camera, old NAS" case.
//!
//! FTP is a stream protocol pretending to be a filesystem, and the distance
//! between the two is where all the work is:
//!
//! * **One transfer per connection.** A control connection carries exactly one
//!   data transfer at a time, so a mount that reads one file while listing
//!   another needs more than one connection. This keeps a small pool
//!   ([`FtpTarget::max_connections`]) and leases a connection per operation.
//! * **There is no `pread`.** A read at an offset is `REST n` + `RETR`, which
//!   opens a new data connection and streams from there to the end of the file.
//!   Re-issuing that for every 1 MiB chunk the VFS asks for would make a
//!   sequential read cost one connection per chunk, so an open file keeps its
//!   transfer and its position: reads that continue where the last one stopped
//!   ride the open stream, and only a seek restarts it.
//! * **Ending a transfer early is the dangerous part.** `ABOR` is widely
//!   half-implemented, and a connection left out of sync answers the *next*
//!   command with the previous transfer's reply. A stream that is abandoned
//!   mid-file therefore takes its connection with it instead of returning it to
//!   the pool.
//! * **No exclusive create, no atomic replace.** `STOR` overwrites silently and
//!   `RNFR`/`RNTO` fails if the target exists on most servers, so `create` and
//!   `rename` check first. That is a race no FTP server offers a way to close.
//! * **No `statfs`.** There is no command for free space, and reporting zero
//!   makes Explorer and Finder refuse to copy anything. The mount reports a
//!   nominal volume instead; see [`NOMINAL_VOLUME`].

mod listing;
mod transport;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use suppaftp::tokio::{AsyncNoTlsStream, AsyncRustlsStream, ImplAsyncFtpStream, TransferStream};
use suppaftp::types::FileType;
use suppaftp::{FtpError, Status};

use mydrives_core::backend::{BFile, Backend};
use mydrives_core::error::{Error, Result};
use mydrives_core::path::RPath;
use mydrives_core::types::{Attr, Caps, DirEntry, FileKind, FsStat, OpenOpts, SetAttr};

use listing::Parsed;
use transport::Transport;

/// What `statfs` reports. FTP cannot answer "how much space is left", and a
/// volume that claims zero free bytes is one Explorer and Finder refuse to copy
/// into, so the mount claims a plausible, constant size. Quota errors surface
/// where they actually happen - on the write.
pub const NOMINAL_VOLUME: u64 = 1 << 40; // 1 TiB

/// Largest file this backend will rewrite to satisfy a truncate to a non-zero
/// length. FTP has no truncate, so it means download-then-upload.
const MAX_TRUNCATE_REWRITE: u64 = 64 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FtpSecurity {
    /// No TLS. Fine on a LAN, not on anything reachable from the internet.
    Plain,
    /// `AUTH TLS` on the control connection after connecting (RFC 4217). What
    /// "FTPS" means on virtually every server that offers it.
    #[default]
    Explicit,
    /// TLS from the first byte, historically on port 990.
    Implicit,
}

impl FtpSecurity {
    pub fn as_str(&self) -> &'static str {
        match self {
            FtpSecurity::Plain => "plain",
            FtpSecurity::Explicit => "explicit",
            FtpSecurity::Implicit => "implicit",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "plain" | "none" | "ftp" => Some(FtpSecurity::Plain),
            "explicit" | "ftps" | "tls" | "auth-tls" => Some(FtpSecurity::Explicit),
            "implicit" | "ftps-implicit" => Some(FtpSecurity::Implicit),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FtpTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// Expose only this subdirectory of the account as the mount root.
    pub subpath: Option<String>,
    pub security: FtpSecurity,
    /// Accept a certificate that does not verify. Needed for the self-signed
    /// certificates NAS boxes ship with.
    pub accept_invalid_certs: bool,
    /// Upper bound on simultaneous control connections. Servers commonly cap
    /// this per account, and exceeding it gets the account throttled.
    pub max_connections: usize,
}

impl Default for FtpTarget {
    fn default() -> Self {
        FtpTarget {
            host: String::new(),
            port: 21,
            user: String::new(),
            password: String::new(),
            subpath: None,
            security: FtpSecurity::Explicit,
            accept_invalid_certs: false,
            max_connections: 4,
        }
    }
}

impl FtpTarget {
    pub fn url(&self) -> String {
        let scheme = match self.security {
            FtpSecurity::Plain => "ftp",
            _ => "ftps",
        };
        let port = if (self.security == FtpSecurity::Plain && self.port == 21)
            || (self.security == FtpSecurity::Implicit && self.port == 990)
            || (self.security == FtpSecurity::Explicit && self.port == 21)
        {
            String::new()
        } else {
            format!(":{}", self.port)
        };
        format!(
            "{scheme}://{}{port}{}",
            self.host,
            self.subpath.as_deref().unwrap_or("")
        )
    }
}

/// Builds the backend for a target, picking the TLS layer it asked for.
///
/// The two are different Rust types - the whole implementation is generic over
/// the transport - so the choice is made once, here.
pub fn backend(id: String, target: FtpTarget) -> Arc<dyn Backend> {
    match target.security {
        FtpSecurity::Plain => Arc::new(FtpBackend::<AsyncNoTlsStream>::new(id, target)),
        _ => Arc::new(FtpBackend::<AsyncRustlsStream>::new(id, target)),
    }
}

type Stream<T> = ImplAsyncFtpStream<T>;

/// A leased control connection. Returning it to the pool on drop is what keeps
/// a `readdir` from paying for a TCP handshake and a login every time.
struct Lease<T: Transport> {
    stream: Option<Stream<T>>,
    pool: Arc<Pool<T>>,
    _permit: OwnedSemaphorePermit,
}

impl<T: Transport> Lease<T> {
    fn get(&mut self) -> &mut Stream<T> {
        self.stream
            .as_mut()
            .expect("the stream is taken only when the lease is consumed")
    }

    /// Give up on this connection: it is out of sync with the server, so it
    /// must not go back into the pool.
    fn discard(mut self) {
        self.stream = None;
    }
}

impl<T: Transport> Drop for Lease<T> {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            self.pool.idle.lock().push(stream);
        }
    }
}

struct Pool<T: Transport> {
    target: FtpTarget,
    idle: Mutex<Vec<Stream<T>>>,
    permits: Arc<Semaphore>,
}

impl<T: Transport> Pool<T> {
    /// A logged-in connection, from the pool or freshly opened.
    async fn lease(self: &Arc<Self>) -> Result<Lease<T>> {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| Error::Backend(e.to_string()))?;

        // The guard is released here, not at the end of the `if let`: holding a
        // parking_lot lock across an await would make every future in this file
        // non-Send.
        let pooled = self.idle.lock().pop();
        if let Some(mut stream) = pooled {
            // A pooled connection may have been closed by an idle timeout while
            // it sat there; one NOOP costs a round trip and saves a confusing
            // error on the real command.
            if stream.noop().await.is_ok() {
                return Ok(Lease {
                    stream: Some(stream),
                    pool: self.clone(),
                    _permit: permit,
                });
            }
        }

        let stream = self.open().await?;
        Ok(Lease {
            stream: Some(stream),
            pool: self.clone(),
            _permit: permit,
        })
    }

    async fn open(&self) -> Result<Stream<T>> {
        let mut stream = T::open(&self.target).await.map_err(map_ftp_error)?;
        let user = if self.target.user.is_empty() {
            "anonymous"
        } else {
            &self.target.user
        };
        let password = if self.target.user.is_empty() && self.target.password.is_empty() {
            "mydrives@example.com"
        } else {
            &self.target.password
        };
        stream.login(user, password).await.map_err(map_ftp_error)?;
        // Everything is bytes: ASCII mode rewrites line endings mid-transfer,
        // which corrupts every file that is not text and breaks every size.
        stream
            .transfer_type(FileType::Binary)
            .await
            .map_err(map_ftp_error)?;
        Ok(stream)
    }
}

/// A transfer bound to an open file, with the position it has reached.
enum Transfer<T: Transport> {
    Read {
        lease: Lease<T>,
        stream: TransferStream<T>,
        pos: u64,
        /// The server has sent everything; the next read returns nothing.
        eof: bool,
    },
    Write {
        lease: Lease<T>,
        stream: TransferStream<T>,
        pos: u64,
    },
}

struct OpenFile<T: Transport> {
    path: RPath,
    opts: OpenOpts,
    /// At most one transfer per open file, and only one caller at a time may
    /// touch it - `read_at` mutates the stream position.
    transfer: tokio::sync::Mutex<Option<Transfer<T>>>,
}

pub struct FtpBackend<T: Transport> {
    id: String,
    pool: Arc<Pool<T>>,
    open: Mutex<HashMap<u64, Arc<OpenFile<T>>>>,
    next_handle: AtomicU64,
    connected: AtomicBool,
}

impl<T: Transport> FtpBackend<T> {
    pub fn new(id: String, target: FtpTarget) -> Self {
        let permits = Arc::new(Semaphore::new(target.max_connections.max(1)));
        FtpBackend {
            id,
            pool: Arc::new(Pool {
                target,
                idle: Mutex::new(Vec::new()),
                permits,
            }),
            open: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
            connected: AtomicBool::new(false),
        }
    }

    fn target(&self) -> &FtpTarget {
        &self.pool.target
    }

    /// Mount-relative path to the absolute path on the server.
    ///
    /// Every command is given an absolute path so that connections never carry
    /// a working directory: a pooled connection is then interchangeable with
    /// any other, whatever the last operation did with it.
    fn remote(&self, path: &RPath) -> String {
        let base = self
            .target()
            .subpath
            .as_deref()
            .unwrap_or("")
            .trim_end_matches('/');
        let rel = path.as_str();
        if rel == "/" {
            if base.is_empty() {
                "/".to_string()
            } else {
                base.to_string()
            }
        } else {
            format!("{base}{rel}")
        }
    }

    fn handle(&self, f: BFile) -> Result<Arc<OpenFile<T>>> {
        self.open
            .lock()
            .get(&f.0)
            .cloned()
            .ok_or_else(|| Error::Backend(format!("unknown file handle {}", f.0)))
    }

    /// `MLSD`, falling back to `LIST` on a server that does not have it.
    async fn list_dir(&self, path: &RPath) -> Result<Vec<DirEntry>> {
        let remote = self.remote(path);
        let mut lease = self.pool.lease().await?;
        let mlsd = lease.get().mlsd(Some(remote.as_str())).await;
        let lines = match mlsd {
            Ok(lines) => {
                let mut out = Vec::with_capacity(lines.len());
                for line in lines {
                    if let Parsed::Entry(e) = listing::parse_mlsd_line(&line) {
                        out.push(e);
                    }
                }
                return Ok(out);
            }
            Err(e) if is_unimplemented(&e) => {
                lease.get().list(Some(remote.as_str())).await.map_err(|e| {
                    map_ftp_path_error(e, path)
                })?
            }
            Err(e) => return Err(map_ftp_path_error(e, path)),
        };
        Ok(lines.iter().filter_map(|l| listing::parse_list_line(l)).collect())
    }

    /// Attributes for one path.
    ///
    /// `MLST` answers it in one round trip, but plenty of servers that have
    /// `MLSD` still lack `MLST`, so the fallback asks `SIZE`/`MDTM` (which only
    /// work on files) and treats "that is not a file" as "it is a directory"
    /// after proving it can be entered.
    async fn stat_path(&self, path: &RPath) -> Result<Attr> {
        if path.is_root() {
            return Ok(Attr::dir());
        }
        let remote = self.remote(path);
        let mut lease = self.pool.lease().await?;

        match lease.get().mlst(Some(remote.as_str())).await {
            Ok(line) => {
                if let Parsed::Entry(e) = listing::parse_mlsd_line(line.trim_start()) {
                    return Ok(e.attr);
                }
                // A server that answers MLST with something unparsable is not
                // going to do better on a second try; fall through.
            }
            Err(e) if !is_unimplemented(&e) => return Err(map_ftp_path_error(e, path)),
            Err(_) => {}
        }

        match lease.get().size(remote.as_str()).await {
            Ok(size) => {
                let mtime = lease
                    .get()
                    .mdtm(remote.as_str())
                    .await
                    .ok()
                    .map(naive_to_system_time)
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                Ok(Attr::new(FileKind::File, size as u64, mtime))
            }
            Err(_) => match lease.get().cwd(remote.as_str()).await {
                Ok(()) => Ok(Attr::dir()),
                Err(e) => Err(map_ftp_path_error(e, path)),
            },
        }
    }

    async fn exists(&self, path: &RPath) -> bool {
        self.stat_path(path).await.is_ok()
    }

    /// Replaces a file's contents with `data`. The only way FTP offers to make
    /// a file shorter.
    async fn rewrite(&self, path: &RPath, data: &[u8]) -> Result<()> {
        let remote = self.remote(path);
        let mut lease = self.pool.lease().await?;
        let mut stream = lease
            .get()
            .put_with_stream(remote.as_str())
            .await
            .map_err(|e| map_ftp_path_error(e, path))?;
        let write = async {
            stream.write_all(data).await?;
            stream.flush().await
        }
        .await;
        if let Err(e) = write {
            let _ = stream.finish().await;
            lease.discard();
            return Err(Error::Io(e));
        }
        stream.finish().await.map_err(|e| map_ftp_path_error(e, path))
    }

    /// Ends whatever transfer an open file is holding.
    ///
    /// A read that stopped before the end of the file leaves the server mid-send
    /// with no reliable way to resynchronise, so that connection is dropped
    /// rather than pooled.
    async fn end_transfer(&self, transfer: Option<Transfer<T>>) -> Result<()> {
        match transfer {
            None => Ok(()),
            Some(Transfer::Read { lease, stream, eof, .. }) => {
                if eof {
                    let finished = stream.finish().await;
                    if finished.is_err() {
                        lease.discard();
                    }
                    Ok(())
                } else {
                    drop(stream);
                    lease.discard();
                    Ok(())
                }
            }
            Some(Transfer::Write { mut lease, stream, .. }) => {
                let result = stream.finish().await;
                match result {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let _ = lease.get();
                        lease.discard();
                        Err(map_ftp_error(e))
                    }
                }
            }
        }
    }
}

#[async_trait]
impl<T: Transport> Backend for FtpBackend<T> {
    fn id(&self) -> &str {
        &self.id
    }

    fn describe(&self) -> String {
        self.target().url()
    }

    fn caps(&self) -> Caps {
        Caps {
            write: true,
            // MFMT is not universal, but a server without it only fails to keep
            // timestamps on copies - nothing the VFS has to refuse up front.
            set_times: true,
            truncate: true,
            // RNFR/RNTO refuses an existing target nearly everywhere, so the
            // VFS must remove it first rather than assume an atomic replace.
            rename_replace: false,
            case_sensitive: true,
            max_io: 1 << 20,
        }
    }

    async fn connect(&self) -> Result<()> {
        // One real session proves host, TLS, credentials and the subpath, and
        // then goes into the pool for the first listing to use.
        let mut lease = self.pool.lease().await?;
        let root = self.remote(&RPath::root());
        if root != "/" {
            lease
                .get()
                .cwd(root.as_str())
                .await
                .map_err(|e| map_ftp_error(e))?;
        }
        self.connected.store(true, Ordering::SeqCst);
        tracing::info!(target = %self.target().url(), "FTP 연결됨");
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        self.connected.store(false, Ordering::SeqCst);
        let open: Vec<Arc<OpenFile<T>>> = self.open.lock().drain().map(|(_, v)| v).collect();
        for file in open {
            let transfer = file.transfer.lock().await.take();
            let _ = self.end_transfer(transfer).await;
        }
        let idle: Vec<Stream<T>> = self.pool.idle.lock().drain(..).collect();
        for mut stream in idle {
            let _ = stream.quit().await;
        }
        Ok(())
    }

    async fn stat(&self, path: &RPath) -> Result<Attr> {
        self.stat_path(path).await
    }

    async fn readdir(&self, path: &RPath) -> Result<Vec<DirEntry>> {
        self.list_dir(path).await
    }

    async fn open(&self, path: &RPath, opts: OpenOpts) -> Result<BFile> {
        // Opening does not touch the network: the transfer starts at the first
        // read or write, and that is also when its direction is known.
        let attr = self.stat_path(path).await?;
        if attr.is_dir() {
            return Err(Error::IsADirectory(path.as_str().to_string()));
        }
        if opts.truncate && opts.write {
            self.rewrite(path, &[]).await?;
        }
        let id = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.open.lock().insert(
            id,
            Arc::new(OpenFile {
                path: path.clone(),
                opts,
                transfer: tokio::sync::Mutex::new(None),
            }),
        );
        Ok(BFile(id))
    }

    async fn create(&self, path: &RPath) -> Result<BFile> {
        // FTP has no exclusive create: STOR overwrites without a word. The
        // check keeps O_EXCL and FILE_CREATE honest for everything except a
        // simultaneous create from another client.
        if self.exists(path).await {
            return Err(Error::AlreadyExists(path.as_str().to_string()));
        }
        self.rewrite(path, &[]).await?;
        let id = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.open.lock().insert(
            id,
            Arc::new(OpenFile {
                path: path.clone(),
                opts: OpenOpts::read_write(),
                transfer: tokio::sync::Mutex::new(None),
            }),
        );
        Ok(BFile(id))
    }

    async fn read_at(&self, f: BFile, offset: u64, len: u32) -> Result<Vec<u8>> {
        let file = self.handle(f)?;
        let mut guard = file.transfer.lock().await;

        // A read that continues where the last one stopped rides the open
        // transfer; anything else has to restart it.
        let continues = matches!(&*guard, Some(Transfer::Read { pos, .. }) if *pos == offset);
        if !continues {
            let previous = guard.take();
            self.end_transfer(previous).await?;

            let remote = self.remote(&file.path);
            let mut lease = self.pool.lease().await?;
            if offset > 0 {
                lease
                    .get()
                    .resume_transfer(offset as usize)
                    .await
                    .map_err(map_ftp_error)?;
            }
            match lease.get().retr_as_stream(remote.as_str()).await {
                Ok(stream) => {
                    *guard = Some(Transfer::Read {
                        lease,
                        stream,
                        pos: offset,
                        eof: false,
                    })
                }
                Err(e) => {
                    lease.discard();
                    // Reading at or past the end is normal; servers answer it
                    // with 550 because the range is empty.
                    if offset > 0 && is_status(&e, Status::FileUnavailable) {
                        return Ok(Vec::new());
                    }
                    return Err(map_ftp_path_error(e, &file.path));
                }
            }
        }

        let Some(Transfer::Read { stream, pos, eof, .. }) = guard.as_mut() else {
            unreachable!("a read transfer was just installed");
        };
        if *eof {
            return Ok(Vec::new());
        }

        let mut buf = vec![0u8; len as usize];
        let mut filled = 0usize;
        while filled < buf.len() {
            match stream.read(&mut buf[filled..]).await {
                Ok(0) => {
                    *eof = true;
                    break;
                }
                Ok(n) => filled += n,
                Err(e) => {
                    let broken = guard.take();
                    // The data connection died mid-file: the control connection
                    // is no longer trustworthy either.
                    if let Some(Transfer::Read { lease, stream, .. }) = broken {
                        drop(stream);
                        lease.discard();
                    }
                    return Err(Error::Io(e));
                }
            }
        }
        buf.truncate(filled);
        *pos += filled as u64;
        Ok(buf)
    }

    async fn write_at(&self, f: BFile, offset: u64, data: &[u8]) -> Result<u32> {
        let file = self.handle(f)?;
        if !file.opts.write {
            return Err(Error::PermissionDenied(file.path.as_str().to_string()));
        }
        let mut guard = file.transfer.lock().await;

        let continues = matches!(&*guard, Some(Transfer::Write { pos, .. }) if *pos == offset);
        if !continues {
            let previous = guard.take();
            self.end_transfer(previous).await?;

            let remote = self.remote(&file.path);
            let mut lease = self.pool.lease().await?;
            if offset > 0 {
                // REST before STOR is how a resumed upload works; a server
                // without it cannot do a write that is not at the start.
                lease
                    .get()
                    .resume_transfer(offset as usize)
                    .await
                    .map_err(map_ftp_error)?;
            }
            match lease.get().put_with_stream(remote.as_str()).await {
                Ok(stream) => {
                    *guard = Some(Transfer::Write {
                        lease,
                        stream,
                        pos: offset,
                    })
                }
                Err(e) => {
                    lease.discard();
                    return Err(map_ftp_path_error(e, &file.path));
                }
            }
        }

        let Some(Transfer::Write { stream, pos, .. }) = guard.as_mut() else {
            unreachable!("a write transfer was just installed");
        };
        match stream.write_all(data).await {
            Ok(()) => {
                *pos += data.len() as u64;
                Ok(data.len() as u32)
            }
            Err(e) => {
                let broken = guard.take();
                if let Some(Transfer::Write { lease, stream, .. }) = broken {
                    drop(stream);
                    lease.discard();
                }
                Err(Error::Io(e))
            }
        }
    }

    async fn flush(&self, f: BFile) -> Result<()> {
        // An upload is only on disk once the data connection closes and the
        // server has answered, so flush ends the transfer. A later write simply
        // starts a new one at the position it left off.
        let file = self.handle(f)?;
        let transfer = file.transfer.lock().await.take();
        self.end_transfer(transfer).await
    }

    async fn close(&self, f: BFile) -> Result<()> {
        let file = self.open.lock().remove(&f.0);
        let Some(file) = file else { return Ok(()) };
        let transfer = file.transfer.lock().await.take();
        self.end_transfer(transfer).await
    }

    async fn set_len(&self, path: &RPath, f: Option<BFile>, len: u64) -> Result<()> {
        // Any transfer on this file has to end first: it would otherwise write
        // over whatever the truncate just did.
        if let Some(f) = f {
            if let Ok(file) = self.handle(f) {
                let transfer = file.transfer.lock().await.take();
                self.end_transfer(transfer).await?;
            }
        }
        let current = self.stat_path(path).await?.size;
        if current == len {
            return Ok(());
        }
        if len == 0 {
            return self.rewrite(path, &[]).await;
        }
        if len > current {
            // Growing means writing zeroes, which is a plain append.
            let pad = vec![0u8; (len - current) as usize];
            let remote = self.remote(path);
            let mut lease = self.pool.lease().await?;
            let mut stream = lease
                .get()
                .append_with_stream(remote.as_str())
                .await
                .map_err(|e| map_ftp_path_error(e, path))?;
            let written = stream.write_all(&pad).await;
            let finished = stream.finish().await;
            written.map_err(Error::Io)?;
            return finished.map_err(|e| map_ftp_path_error(e, path));
        }
        if current > MAX_TRUNCATE_REWRITE {
            return Err(Error::unsupported(format!(
                "FTP는 자르기를 지원하지 않아 파일을 다시 올려야 하는데 {current} 바이트는 너무 큽니다"
            )));
        }
        // Shrinking: read the part that survives, write it back.
        let remote = self.remote(path);
        let mut lease = self.pool.lease().await?;
        let mut stream = lease
            .get()
            .retr_as_stream(remote.as_str())
            .await
            .map_err(|e| map_ftp_path_error(e, path))?;
        let mut head = vec![0u8; len as usize];
        let read = stream.read_exact(&mut head).await;
        // The transfer is deliberately abandoned part-way, so the connection
        // goes with it.
        drop(stream);
        lease.discard();
        read.map_err(Error::Io)?;
        self.rewrite(path, &head).await
    }

    async fn set_attr(&self, path: &RPath, _f: Option<BFile>, set: &SetAttr) -> Result<()> {
        if let Some(len) = set.size {
            self.set_len(path, None, len).await?;
        }
        let Some(mtime) = set.mtime else {
            return Ok(());
        };
        let Some(stamp) = listing::format_timeval(mtime) else {
            return Ok(());
        };
        let remote = self.remote(path);
        let mut lease = self.pool.lease().await?;
        let result = lease
            .get()
            .custom_command(format!("MFMT {stamp} {remote}"), &[Status::File])
            .await;
        if let Err(e) = result {
            if is_unimplemented(&e) {
                // A server without MFMT cannot keep timestamps. Failing here
                // would turn every `cp -p` and every Finder copy into an error
                // after the data is already safely written.
                tracing::debug!(path = %path.as_str(), "서버가 MFMT를 지원하지 않아 수정 시각을 유지하지 못했습니다");
                return Ok(());
            }
            return Err(map_ftp_path_error(e, path));
        }
        Ok(())
    }

    async fn mkdir(&self, path: &RPath) -> Result<()> {
        let remote = self.remote(path);
        let mut lease = self.pool.lease().await?;
        lease
            .get()
            .mkdir(remote.as_str())
            .await
            .map_err(|e| map_ftp_path_error(e, path))
    }

    async fn unlink(&self, path: &RPath) -> Result<()> {
        let remote = self.remote(path);
        let mut lease = self.pool.lease().await?;
        lease
            .get()
            .rm(remote.as_str())
            .await
            .map_err(|e| map_ftp_path_error(e, path))
    }

    async fn rmdir(&self, path: &RPath) -> Result<()> {
        let remote = self.remote(path);
        let mut lease = self.pool.lease().await?;
        lease
            .get()
            .rmdir(remote.as_str())
            .await
            .map_err(|e| map_ftp_path_error(e, path))
    }

    async fn rename(&self, from: &RPath, to: &RPath, replace: bool) -> Result<()> {
        if self.exists(to).await {
            if !replace {
                return Err(Error::AlreadyExists(to.as_str().to_string()));
            }
            // RNTO onto an existing name fails on most servers, so the target
            // is removed first. There is no way to make that atomic in FTP.
            let attr = self.stat_path(to).await?;
            if attr.is_dir() {
                self.rmdir(to).await?;
            } else {
                self.unlink(to).await?;
            }
        }
        let (src, dst) = (self.remote(from), self.remote(to));
        let mut lease = self.pool.lease().await?;
        lease
            .get()
            .rename(src.as_str(), dst.as_str())
            .await
            .map_err(|e| map_ftp_path_error(e, from))
    }

    async fn statfs(&self) -> Result<FsStat> {
        Ok(FsStat {
            total_bytes: NOMINAL_VOLUME,
            free_bytes: NOMINAL_VOLUME,
            block_size: 4096,
            total_files: 0,
            free_files: 0,
        })
    }
}

fn naive_to_system_time(t: chrono::NaiveDateTime) -> SystemTime {
    let secs = t.and_utc().timestamp();
    if secs < 0 {
        SystemTime::UNIX_EPOCH
    } else {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64)
    }
}

fn status_of(e: &FtpError) -> Option<Status> {
    match e {
        FtpError::UnexpectedResponse(r) => Some(r.status),
        _ => None,
    }
}

fn is_status(e: &FtpError, status: Status) -> bool {
    status_of(e) == Some(status)
}

/// "This server does not have that command", in its several spellings.
fn is_unimplemented(e: &FtpError) -> bool {
    matches!(
        status_of(e),
        Some(Status::NotImplemented)
            | Some(Status::NotImplementedParameter)
            | Some(Status::BadCommand)
            | Some(Status::BadArguments)
    )
}

fn map_ftp_error(e: FtpError) -> Error {
    match &e {
        FtpError::ConnectionError(io) => Error::Offline(io.to_string()),
        FtpError::SecureError(m) => Error::Backend(format!("TLS: {m}")),
        FtpError::UnexpectedResponse(r) => {
            let body = r.as_string().unwrap_or_default();
            match r.status {
                Status::NotLoggedIn => Error::AuthFailed(body),
                Status::ExceededStorage | Status::BadFilename => Error::PermissionDenied(body),
                Status::FileUnavailable => Error::NotFound(body),
                _ => Error::Backend(format!("{}: {body}", r.status as u32)),
            }
        }
        _ => Error::Backend(e.to_string()),
    }
}

/// The same mapping, but for a command that names a path: FTP answers "no such
/// file", "you may not", "it is not empty" and "it already exists" all with
/// 550, so the text is the only thing left to tell them apart.
fn map_ftp_path_error(e: FtpError, path: &RPath) -> Error {
    let FtpError::UnexpectedResponse(r) = &e else {
        return map_ftp_error(e);
    };
    if r.status != Status::FileUnavailable && r.status != Status::RequestedActionNotTaken {
        return map_ftp_error(e);
    }
    let body = r.as_string().unwrap_or_default().to_ascii_lowercase();
    let p = path.as_str().to_string();
    if body.contains("exist") && !body.contains("not exist") && !body.contains("no such") {
        Error::AlreadyExists(p)
    } else if body.contains("not empty") || body.contains("directory not empty") {
        Error::NotEmpty(p)
    } else if body.contains("permission") || body.contains("denied") || body.contains("access") {
        Error::PermissionDenied(p)
    } else {
        Error::NotFound(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> FtpTarget {
        FtpTarget {
            host: "files.example.com".into(),
            user: "u".into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_url_names_the_protocol_that_is_actually_used() {
        let mut t = target();
        t.security = FtpSecurity::Plain;
        assert_eq!(t.url(), "ftp://files.example.com");
        t.security = FtpSecurity::Explicit;
        assert_eq!(t.url(), "ftps://files.example.com");
        t.port = 2121;
        assert_eq!(t.url(), "ftps://files.example.com:2121");
    }

    #[test]
    fn a_subpath_becomes_the_root_of_the_mount() {
        let mut t = target();
        t.subpath = Some("/home/u/public".into());
        let b = FtpBackend::<AsyncNoTlsStream>::new("ftp".into(), t);
        assert_eq!(b.remote(&RPath::root()), "/home/u/public");
        assert_eq!(b.remote(&RPath::new("/a/b.txt")), "/home/u/public/a/b.txt");
    }

    #[test]
    fn without_a_subpath_paths_are_passed_through() {
        let b = FtpBackend::<AsyncNoTlsStream>::new("ftp".into(), target());
        assert_eq!(b.remote(&RPath::root()), "/");
        assert_eq!(b.remote(&RPath::new("/a/b.txt")), "/a/b.txt");
    }

    #[test]
    fn security_round_trips_through_its_config_spelling() {
        for s in [
            FtpSecurity::Plain,
            FtpSecurity::Explicit,
            FtpSecurity::Implicit,
        ] {
            assert_eq!(FtpSecurity::parse(s.as_str()), Some(s));
        }
        assert_eq!(FtpSecurity::parse("FTPS"), Some(FtpSecurity::Explicit));
        assert_eq!(FtpSecurity::parse("nonsense"), None);
    }
}
