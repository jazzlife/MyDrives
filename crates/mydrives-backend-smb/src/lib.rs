//! SMB2/3 backend - the NAS case.
//!
//! Implemented on the pure-Rust `smb` crate so that one binary works on Windows,
//! macOS and Linux without Samba development packages.
//!
//! Things that bite when you map SMB onto a POSIX-ish VFS, and how they are
//! handled here:
//!
//! * **Reads past EOF** return `STATUS_END_OF_FILE` instead of zero bytes. A
//!   literal translation turns a normal `read()` at the end of a file into an I/O
//!   error, which makes `cp` and Finder copies fail on the last block.
//! * **Deletion** is not a verb. You open with DELETE access and
//!   `FILE_DELETE_ON_CLOSE`, then close. A failed close means the file is still
//!   there.
//! * **Rename** is a SET_INFO on an open handle with DELETE access, and the new
//!   name is share-relative, not absolute.
//! * **Timestamps** use FILETIME (100 ns ticks since 1601) and `0` means "leave
//!   unchanged" - passing a real zero wipes the timestamp.
//! * **Directory enumeration** holds a lock on the handle: SMB2 allows only one
//!   outstanding query per handle, so each `readdir` opens its own handle.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use parking_lot::Mutex;

use smb::resource::{Directory, File, FileCreateArgs, GetLen, ReadAt, Resource, SetLen, WriteAt};
use smb::{Client, ClientConfig, UncPath};

use mydrives_core::backend::{BFile, Backend};
use mydrives_core::error::{Error, Result};
use mydrives_core::path::RPath;
use mydrives_core::types::{
    filetime_to_system_time, system_time_to_filetime, Attr, Caps, DirEntry, FileKind, FsStat,
    OpenOpts, SetAttr,
};

const STATUS_END_OF_FILE: u32 = 0xC000_0011;
/// Returned when the target carries the read-only attribute.
const STATUS_CANNOT_DELETE: u32 = 0xC000_0121;
/// The server no longer knows this file id - the handle died under us (idle
/// close on the server, a reconnect, or our own expiry racing an in-flight read).
const STATUS_FILE_CLOSED: u32 = 0xC000_0128;
const STATUS_INVALID_HANDLE: u32 = 0xC000_0008;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Minimum gap between re-opens of the same handle, so reading past the end of a
/// file that really is that long does not re-open on every request.
const REOPEN_COOLDOWN: Duration = Duration::from_millis(200);

#[derive(Clone, Debug)]
pub struct SmbTarget {
    pub host: String,
    pub port: u16,
    pub share: String,
    pub user: String,
    pub password: String,
    /// Expose only this subdirectory of the share as the mount root.
    pub subpath: Option<String>,
}

impl SmbTarget {
    /// The `smb` crate takes `host` or `host:port` and resolves it itself.
    fn server(&self) -> String {
        if self.port == 445 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn url(&self) -> String {
        let sub = self.subpath.as_deref().unwrap_or("");
        format!("smb://{}/{}{}", self.server(), self.share, sub)
    }
}

/// An open file, plus what we need to work around a stale EOF in the client.
///
/// `smb::resource::File` captures the file size when the handle is opened and
/// short-circuits `read_block` with `Ok(0)` for any offset at or past it - the
/// value is never refreshed after a write through the same handle. So reading
/// back data you just wrote returns nothing at all: `tar -x` then `cmp`, an
/// editor's save-then-reload, `cp` into a file it also reads. Observed as a
/// 27-byte file reading as 27 zero bytes while the server held the right data.
///
/// The fix is to notice when our own write grew the file past the size the
/// handle was opened with, and re-open before the next read.
struct OpenFile {
    path: RPath,
    opts: OpenOpts,
    file: Mutex<Arc<File>>,
    /// File size as of the current handle's open - the value `read_block`
    /// compares against before deciding a read is past the end.
    opened_eof: AtomicU64,
    /// Serializes re-opening, so a second reader cannot slip through with the
    /// stale handle while the first is still mid-reopen.
    reopen: tokio::sync::Mutex<()>,
    /// When the handle was last re-opened, to avoid reopening on every read of a
    /// file that genuinely ends where the handle thinks it does.
    last_reopen: Mutex<std::time::Instant>,
}

pub struct SmbBackend {
    id: String,
    target: SmbTarget,
    client: tokio::sync::RwLock<Option<Arc<Client>>>,
    open: Mutex<HashMap<u64, Arc<OpenFile>>>,
    next_handle: AtomicU64,
}

impl SmbBackend {
    pub fn new(id: impl Into<String>, target: SmbTarget) -> Self {
        SmbBackend {
            id: id.into(),
            target,
            client: tokio::sync::RwLock::new(None),
            open: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
        }
    }

    /// Lists the shares the server offers, so the user does not have to know the
    /// exact name. Uses its own IPC$ session and leaves the data connection
    /// untouched; administrative shares (`IPC$`, `ADMIN$`, `C$`) are filtered out.
    pub async fn list_shares(&self) -> Result<Vec<String>> {
        let server = self.target.server();
        let client = Client::new(ClientConfig::default());
        let connect = client.ipc_connect(
            &server,
            &self.target.user,
            self.target.password.clone(),
        );
        match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(map_smb_err(e, &server)),
            Err(_) => return Err(Error::Timeout(format!("connecting to {server}"))),
        }

        // Enumeration goes over DCE/RPC (srvsvc). Samba-based servers - which is
        // what QNAP, Synology and friends run - reject the NDR64 transfer syntax
        // this client proposes, so this fails against most NAS boxes even though
        // the session itself is fine. Report it as "unsupported" rather than as a
        // connection problem, so the caller just asks for the name instead.
        let shares = match client.list_shares(&server).await {
            Ok(shares) => shares,
            Err(e) => {
                let _ = client.close().await;
                let detail = e.to_string();
                return Err(if detail.contains("BindAck") || detail.contains("syntax") {
                    Error::Unsupported(format!(
                        "{server}: 이 서버는 공유 목록 조회를 지원하지 않습니다(Samba 계열). 공유 폴더 이름을 직접 입력하세요"
                    ))
                } else {
                    map_smb_err(e, &server)
                });
            }
        };
        let _ = client.close().await;

        Ok(shares
            .iter()
            .filter_map(|s| s.netname.as_ref().map(|n| n.value.to_string()))
            .filter(|name| !name.ends_with('$'))
            .collect())
    }

    async fn client(&self) -> Result<Arc<Client>> {
        self.client
            .read()
            .await
            .clone()
            .ok_or_else(|| Error::Offline(format!("{} is not connected", self.target.url())))
    }

    /// Path inside the share, SMB style, no leading separator.
    fn share_relative(&self, path: &RPath) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(sub) = &self.target.subpath {
            let sub = RPath::new(sub);
            if !sub.is_root() {
                parts.push(sub.to_backslash());
            }
        }
        if !path.is_root() {
            parts.push(path.to_backslash());
        }
        parts.join("\\")
    }

    fn unc(&self, path: &RPath) -> Result<UncPath> {
        let unc = UncPath::new(&self.target.server())
            .and_then(|u| u.with_share(&self.target.share))
            .map_err(|e| Error::InvalidArgument(format!("bad SMB target: {e}")))?;
        let rel = self.share_relative(path);
        Ok(if rel.is_empty() {
            unc.with_no_path()
        } else {
            unc.with_path(&rel)
        })
    }

    async fn open_resource(&self, path: &RPath, args: &FileCreateArgs) -> Result<Resource> {
        let client = self.client().await?;
        let unc = self.unc(path)?;
        client
            .create_file(&unc, args)
            .await
            .map_err(|e| map_smb_err(e, path.as_str()))
    }

    fn handle(&self, f: BFile) -> Result<Arc<OpenFile>> {
        self.open
            .lock()
            .get(&f.0)
            .cloned()
            .ok_or_else(|| Error::InvalidArgument(format!("bad SMB handle {}", f.0)))
    }

    async fn register(&self, path: &RPath, file: File, opts: OpenOpts) -> BFile {
        let id = self.next_handle.fetch_add(1, Ordering::Relaxed);
        // `get_len` returns the size the handle was opened with - exactly the
        // value `read_block` compares against - and costs no round trip.
        let eof = file.get_len().await.unwrap_or(0);
        self.open.lock().insert(
            id,
            Arc::new(OpenFile {
                path: path.clone(),
                opts,
                file: Mutex::new(Arc::new(file)),
                opened_eof: AtomicU64::new(eof),
                reopen: tokio::sync::Mutex::new(()),
                last_reopen: Mutex::new(
                    std::time::Instant::now() - REOPEN_COOLDOWN,
                ),
            }),
        );
        BFile(id)
    }

    /// Returns a handle able to serve a read that ends at `needed_end`.
    ///
    /// The decision is made by the *read*, not by write bookkeeping: a flag set
    /// on write is always one step behind when reads and writes overlap, and the
    /// failure mode is silent (zeros). If the read would run past what this
    /// handle believes is the end of the file, re-open and ask again.
    async fn readable_file(&self, h: &OpenFile, needed_end: u64) -> Result<Arc<File>> {
        if needed_end <= h.opened_eof.load(Ordering::Acquire) {
            return Ok(h.file.lock().clone());
        }
        let _reopening = h.reopen.lock().await;
        // Someone may have refreshed it while we waited.
        if needed_end <= h.opened_eof.load(Ordering::Acquire) {
            return Ok(h.file.lock().clone());
        }
        // The file may genuinely end here; do not re-open on every read past EOF.
        if h.last_reopen.lock().elapsed() < REOPEN_COOLDOWN {
            return Ok(h.file.lock().clone());
        }
        tracing::debug!(path = %h.path, needed_end, "re-opening: the read runs past the handle's cached EOF");
        self.reopen_handle(h).await
    }

    /// Re-opens unconditionally (used when the server says the handle is gone).
    async fn force_reopen(&self, h: &OpenFile) -> Result<Arc<File>> {
        let _reopening = h.reopen.lock().await;
        tracing::debug!(path = %h.path, "re-opening: the server closed the handle");
        self.reopen_handle(h).await
    }

    /// Opens a fresh handle for `h` and swaps it in. Callers hold `h.reopen`.
    async fn reopen_handle(&self, h: &OpenFile) -> Result<Arc<File>> {

        let mut access = smb::FileAccessMask::new().with_file_read_attributes(true);
        if h.opts.read {
            access = access.with_generic_read(true);
        }
        if h.opts.write {
            access = access.with_generic_write(true);
        }
        let args = FileCreateArgs {
            disposition: smb::CreateDisposition::Open,
            attributes: smb::FileAttributes::new(),
            options: smb::CreateOptions::new().with_non_directory_file(true),
            desired_access: access,
        };
        let resource = self.open_resource(&h.path, &args).await?;
        if resource.is_dir() {
            let _ = close_resource(resource, &h.path).await;
            return Err(Error::IsADirectory(h.path.to_string()));
        }
        let fresh = Arc::new(resource.unwrap_file());
        h.opened_eof
            .store(fresh.get_len().await.unwrap_or(0), Ordering::Release);
        *h.last_reopen.lock() = std::time::Instant::now();

        let previous = {
            let mut slot = h.file.lock();
            std::mem::replace(&mut *slot, fresh.clone())
        };
        // Closing the old handle releases it on the server; a failure here is not
        // worth failing the read for.
        if let Err(e) = previous.close().await {
            tracing::debug!(path = %h.path, error = %e, "closing the superseded handle failed");
        }
        Ok(fresh)
    }

    async fn attr_of(&self, path: &RPath) -> Result<Attr> {
        let args = FileCreateArgs::make_open_existing(
            smb::FileAccessMask::new()
                .with_file_read_attributes(true)
                .with_read_control(true),
        );
        let resource = self.open_resource(path, &args).await?;
        let is_dir = resource.is_dir();
        // One query, not two: FileNetworkOpenInformation carries the times,
        // the attributes *and* the size, so a stat is create + query + close
        // rather than create + basic + standard + close. Over a WAN every
        // round trip is a noticeable share of a Finder refresh.
        let info: smb::FileNetworkOpenInformation = match &resource {
            Resource::File(f) => f.query_info().await,
            Resource::Directory(d) => d.query_info().await,
            Resource::Pipe(_) => {
                return Err(Error::Unsupported(format!("{path} is a named pipe")))
            }
        }
        .map_err(|e| map_smb_err(e, path.as_str()))?;
        close_resource(resource, path).await?;

        Ok(Attr {
            kind: if is_dir { FileKind::Dir } else { FileKind::File },
            size: if is_dir { 0 } else { info.end_of_file },
            crtime: filetime_to_system_time(*info.creation_time),
            atime: filetime_to_system_time(*info.last_access_time),
            mtime: filetime_to_system_time(*info.last_write_time),
            ctime: filetime_to_system_time(*info.change_time),
            readonly: info.file_attributes.readonly(),
            hidden: info.file_attributes.hidden(),
        })
    }

    /// Deletion, SMB style.
    ///
    /// Done with SET_INFO(FileDispositionInformation) rather than
    /// FILE_DELETE_ON_CLOSE on purpose: the server validates immediately and
    /// returns STATUS_DIRECTORY_NOT_EMPTY / STATUS_ACCESS_DENIED from the
    /// SET_INFO, whereas with delete-on-close the failure only shows up at close
    /// time, where it is easy to lose. Reporting a failed delete as success is
    /// the worst possible outcome: Explorer removes the item from its view and
    /// the user believes the file is gone.
    async fn delete(&self, path: &RPath, directory: bool) -> Result<()> {
        let options = if directory {
            smb::CreateOptions::new().with_directory_file(true)
        } else {
            smb::CreateOptions::new().with_non_directory_file(true)
        };
        let args = FileCreateArgs {
            disposition: smb::CreateDisposition::Open,
            attributes: smb::FileAttributes::new(),
            options,
            desired_access: smb::FileAccessMask::new()
                .with_delete(true)
                .with_file_read_attributes(true)
                // Needed to clear a read-only attribute that would otherwise
                // block the delete (see below).
                .with_file_write_attributes(true),
        };
        let resource = self.open_resource(path, &args).await?;

        // Servers are supposed to reject the open on a kind mismatch, but not all
        // of them do (Samba 4.12 happily opens a file with FILE_DIRECTORY_FILE).
        if resource.is_dir() != directory {
            let kind_err = if directory {
                Error::NotADirectory(path.to_string())
            } else {
                Error::IsADirectory(path.to_string())
            };
            let _ = close_resource(resource, path).await;
            return Err(kind_err);
        }

        if let Resource::Pipe(_) = resource {
            let _ = close_resource(resource, path).await;
            return Err(Error::Unsupported(format!("deleting pipe {path}")));
        }

        let mut attempt = mark_deleted(&resource).await;

        // STATUS_CANNOT_DELETE means the file carries the read-only attribute.
        // POSIX says otherwise: permission to unlink comes from the *directory*,
        // so `rm` on a 0444 file succeeds - and tools rely on it (git chmods its
        // loose objects to 0444 and then has to be able to replace them). Clear
        // the attribute and try once more.
        if matches!(
            &attempt,
            Err(smb::Error::ReceivedErrorMessage(STATUS_CANNOT_DELETE, _))
        ) {
            tracing::debug!(%path, "file is marked read-only; clearing it to delete");
            let normal = smb::FileBasicInformation {
                creation_time: 0u64.into(),
                last_access_time: 0u64.into(),
                last_write_time: 0u64.into(),
                change_time: 0u64.into(),
                file_attributes: smb::FileAttributes::new().with_normal(true),
            };
            let cleared = match &resource {
                Resource::File(f) => f.set_info(normal).await,
                Resource::Directory(d) => d.set_info(normal).await,
                Resource::Pipe(_) => unreachable!("handled above"),
            };
            if cleared.is_ok() {
                attempt = mark_deleted(&resource).await;
            }
        }

        let marked = attempt.map_err(|e| map_smb_err(e, path.as_str()));

        let closed = close_resource(resource, path).await;
        marked.and(closed)
    }
}

#[async_trait]
impl Backend for SmbBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn describe(&self) -> String {
        self.target.url()
    }

    fn caps(&self) -> Caps {
        Caps {
            write: true,
            set_times: true,
            truncate: true,
            rename_replace: true,
            // SMB servers are case-insensitive in practice (and Windows clients
            // assume it), even when the underlying filesystem is not.
            case_sensitive: false,
            max_io: 1 << 20,
            // A share on the local network is cheap to re-read, and someone
            // else may change it at any moment.
            metadata_ttl: std::time::Duration::from_secs(10),
        }
    }

    async fn connect(&self) -> Result<()> {
        // Drop any previous session first: after a sleep/wake the old TCP
        // connection is dead and its handles are worthless.
        if let Some(old) = self.client.write().await.take() {
            let _ = old.close().await;
            self.open.lock().clear();
        }

        let client = Client::new(ClientConfig::default());
        let unc = self.unc(&RPath::root())?;
        let user = self.target.user.clone();
        let password = self.target.password.clone();

        let connect = client.share_connect(&unc, &user, password);
        match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(map_smb_err(e, &self.target.url())),
            Err(_) => {
                return Err(Error::Timeout(format!(
                    "connecting to {}",
                    self.target.url()
                )))
            }
        }
        *self.client.write().await = Some(Arc::new(client));
        tracing::info!(target_url = %self.target.url(), "SMB session established");
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        let handles: Vec<Arc<OpenFile>> = self.open.lock().drain().map(|(_, v)| v).collect();
        for h in handles {
            let file = h.file.lock().clone();
            let _ = file.close().await;
        }
        if let Some(client) = self.client.write().await.take() {
            let _ = client.close().await;
        }
        Ok(())
    }

    async fn stat(&self, path: &RPath) -> Result<Attr> {
        self.attr_of(path).await
    }

    async fn readdir(&self, path: &RPath) -> Result<Vec<DirEntry>> {
        let args = FileCreateArgs {
            disposition: smb::CreateDisposition::Open,
            attributes: smb::FileAttributes::new(),
            options: smb::CreateOptions::new().with_directory_file(true),
            desired_access: smb::FileAccessMask::new()
                .with_generic_read(true)
                .with_file_read_attributes(true),
        };
        let resource = self.open_resource(path, &args).await?;
        if !resource.is_dir() {
            let _ = close_resource(resource, path).await;
            return Err(Error::NotADirectory(path.to_string()));
        }
        let dir = Arc::new(resource.unwrap_dir());

        let mut out = Vec::new();
        {
            let mut stream =
                Directory::query::<smb::FileDirectoryInformation>(&dir, "*")
                    .await
                    .map_err(|e| map_smb_err(e, path.as_str()))?;
            while let Some(item) = stream.next().await {
                let info = item.map_err(|e| map_smb_err(e, path.as_str()))?;
                let name = String::try_from(info.file_name)
                    .map_err(|e| Error::Backend(format!("bad file name in {path}: {e}")))?;
                if name == "." || name == ".." {
                    continue;
                }
                let is_dir = info.file_attributes.directory();
                out.push(DirEntry {
                    name,
                    attr: Attr {
                        kind: if is_dir { FileKind::Dir } else { FileKind::File },
                        size: if is_dir { 0 } else { info.end_of_file },
                        crtime: filetime_to_system_time(*info.creation_time),
                        atime: filetime_to_system_time(*info.last_access_time),
                        mtime: filetime_to_system_time(*info.last_write_time),
                        ctime: filetime_to_system_time(*info.change_time),
                        readonly: info.file_attributes.readonly(),
                        hidden: info.file_attributes.hidden(),
                    },
                });
            }
        }
        dir.close().await.map_err(|e| map_smb_err(e, path.as_str()))?;
        Ok(out)
    }

    async fn open(&self, path: &RPath, opts: OpenOpts) -> Result<BFile> {
        let mut access = smb::FileAccessMask::new().with_file_read_attributes(true);
        if opts.read {
            access = access.with_generic_read(true);
        }
        if opts.write {
            access = access.with_generic_write(true);
        }
        let args = FileCreateArgs {
            disposition: smb::CreateDisposition::Open,
            attributes: smb::FileAttributes::new(),
            options: smb::CreateOptions::new().with_non_directory_file(true),
            desired_access: access,
        };
        let resource = self.open_resource(path, &args).await?;
        if resource.is_dir() {
            let _ = close_resource(resource, path).await;
            return Err(Error::IsADirectory(path.to_string()));
        }
        Ok(self.register(path, resource.unwrap_file(), opts).await)
    }

    async fn create(&self, path: &RPath) -> Result<BFile> {
        let args = FileCreateArgs {
            disposition: smb::CreateDisposition::Create,
            attributes: smb::FileAttributes::new(),
            options: smb::CreateOptions::new().with_non_directory_file(true),
            desired_access: smb::FileAccessMask::new()
                .with_generic_read(true)
                .with_generic_write(true)
                .with_file_read_attributes(true)
                .with_delete(true),
        };
        let resource = self.open_resource(path, &args).await?;
        Ok(self.register(path, resource.unwrap_file(), OpenOpts::read_write()).await)
    }

    async fn read_at(&self, f: BFile, offset: u64, len: u32) -> Result<Vec<u8>> {
        let h = self.handle(f)?;
        let file = self.readable_file(&h, offset + len as u64).await?;
        let mut buf = vec![0u8; len as usize];
        match file.read_at(&mut buf, offset).await {
            Ok(n) => {
                buf.truncate(n);
                Ok(buf)
            }
            // Reading at or past EOF is not an error for the layers above us.
            Err(smb::Error::ReceivedErrorMessage(STATUS_END_OF_FILE, _)) => Ok(Vec::new()),
            // The handle died under us. Re-open and try once more rather than
            // handing the application an I/O error for a recoverable condition.
            Err(smb::Error::ReceivedErrorMessage(
                STATUS_FILE_CLOSED | STATUS_INVALID_HANDLE,
                _,
            )) => {
                let fresh = self.force_reopen(&h).await?;
                let mut buf = vec![0u8; len as usize];
                match fresh.read_at(&mut buf, offset).await {
                    Ok(n) => {
                        buf.truncate(n);
                        Ok(buf)
                    }
                    Err(smb::Error::ReceivedErrorMessage(STATUS_END_OF_FILE, _)) => {
                        Ok(Vec::new())
                    }
                    Err(e) => Err(map_smb_err(e, h.path.as_str())),
                }
            }
            Err(e) => Err(map_smb_err(e, h.path.as_str())),
        }
    }

    async fn write_at(&self, f: BFile, offset: u64, data: &[u8]) -> Result<u32> {
        let h = self.handle(f)?;
        let file = h.file.lock().clone();
        let n = file
            .write_at(data, offset)
            .await
            .map_err(|e| map_smb_err(e, h.path.as_str()))?;
        Ok(n as u32)
    }

    async fn flush(&self, f: BFile) -> Result<()> {
        let h = self.handle(f)?;
        let file = h.file.lock().clone();
        file.flush().await.map_err(Error::Io)
    }

    async fn close(&self, f: BFile) -> Result<()> {
        let h = match self.open.lock().remove(&f.0) {
            Some(h) => h,
            None => return Ok(()),
        };
        let file = h.file.lock().clone();
        file.close()
            .await
            .map_err(|e| map_smb_err(e, h.path.as_str()))
    }

    async fn set_len(&self, path: &RPath, f: Option<BFile>, len: u64) -> Result<()> {
        let info = smb::FileEndOfFileInformation { end_of_file: len };
        if let Some(f) = f {
            let h = self.handle(f)?;
            // The handle may not have been opened with write access.
            let file = h.file.lock().clone();
            match file.set_info(info).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::debug!(%path, error = %e, "set_len on existing handle failed; reopening");
                }
            }
        }
        let args = FileCreateArgs::make_open_existing(
            smb::FileAccessMask::new()
                .with_generic_write(true)
                .with_file_read_attributes(true),
        );
        let resource = self.open_resource(path, &args).await?;
        let file = resource.unwrap_file();
        let r = file
            .set_len(len)
            .await
            .map_err(|e| map_smb_err(e, path.as_str()));
        let _ = file.close().await;
        r
    }

    async fn set_attr(&self, path: &RPath, _f: Option<BFile>, set: &SetAttr) -> Result<()> {
        // FILETIME 0 means "do not change", so only the requested fields are set.
        let attributes = match set.readonly {
            Some(true) => smb::FileAttributes::new().with_readonly(true),
            // Clearing read-only needs an explicit NORMAL, not an empty mask
            // (an empty mask is the "leave as is" encoding).
            Some(false) => smb::FileAttributes::new().with_normal(true),
            None => smb::FileAttributes::new(),
        };
        let info = smb::FileBasicInformation {
            creation_time: 0u64.into(),
            last_access_time: set.atime.map(system_time_to_filetime).unwrap_or(0).into(),
            last_write_time: set.mtime.map(system_time_to_filetime).unwrap_or(0).into(),
            change_time: 0u64.into(),
            file_attributes: attributes,
        };
        let args = FileCreateArgs::make_open_existing(
            smb::FileAccessMask::new()
                .with_file_write_attributes(true)
                .with_file_read_attributes(true),
        );
        let resource = self.open_resource(path, &args).await?;
        let r = match &resource {
            Resource::File(f) => f.set_info(info).await,
            Resource::Directory(d) => d.set_info(info).await,
            Resource::Pipe(_) => return Err(Error::Unsupported("set_attr on a pipe".into())),
        }
        .map_err(|e| map_smb_err(e, path.as_str()));
        close_resource(resource, path).await?;
        r
    }

    async fn mkdir(&self, path: &RPath) -> Result<()> {
        let args = FileCreateArgs {
            disposition: smb::CreateDisposition::Create,
            attributes: smb::FileAttributes::new().with_directory(true),
            options: smb::CreateOptions::new().with_directory_file(true),
            desired_access: smb::FileAccessMask::new()
                .with_generic_read(true)
                .with_file_read_attributes(true),
        };
        let resource = self.open_resource(path, &args).await?;
        close_resource(resource, path).await
    }

    async fn unlink(&self, path: &RPath) -> Result<()> {
        self.delete(path, false).await
    }

    async fn rmdir(&self, path: &RPath) -> Result<()> {
        self.delete(path, true).await
    }

    async fn rename(&self, from: &RPath, to: &RPath, replace: bool) -> Result<()> {
        let args = FileCreateArgs::make_open_existing(
            smb::FileAccessMask::new()
                .with_delete(true)
                .with_file_read_attributes(true),
        );
        let resource = self.open_resource(from, &args).await?;
        let info = smb::FileRenameInformation {
            replace_if_exists: replace.into(),
            root_directory: 0,
            // Share-relative, with no leading separator.
            file_name: self.share_relative(to).as_str().into(),
        };
        let r = match &resource {
            Resource::File(f) => f.set_info(info).await,
            Resource::Directory(d) => d.set_info(info).await,
            Resource::Pipe(_) => return Err(Error::Unsupported("rename of a pipe".into())),
        }
        .map_err(|e| map_smb_err(e, from.as_str()));
        close_resource(resource, from).await?;
        r
    }

    async fn statfs(&self) -> Result<FsStat> {
        let args = FileCreateArgs {
            disposition: smb::CreateDisposition::Open,
            attributes: smb::FileAttributes::new(),
            options: smb::CreateOptions::new().with_directory_file(true),
            desired_access: smb::FileAccessMask::new().with_file_read_attributes(true),
        };
        let root = RPath::root();
        let resource = self.open_resource(&root, &args).await?;
        let dir = resource.unwrap_dir();
        let info: smb::FileFsFullSizeInformation = dir
            .query_fs_info()
            .await
            .map_err(|e| map_smb_err(e, "statfs"))?;
        let _ = dir.close().await;

        let unit = info.sectors_per_allocation_unit as u64 * info.bytes_per_sector as u64;
        Ok(FsStat {
            total_bytes: info.total_allocation_units * unit,
            free_bytes: info.caller_available_allocation_units * unit,
            block_size: info.bytes_per_sector.max(512),
            total_files: 0,
            free_files: 0,
        })
    }
}

/// Marks an open resource for deletion (SMB has no "delete" verb).
async fn mark_deleted(resource: &Resource) -> std::result::Result<(), smb::Error> {
    let disposition = smb::FileDispositionInformation {
        delete_pending: true.into(),
    };
    match resource {
        Resource::File(f) => f.set_info(disposition).await,
        Resource::Directory(d) => d.set_info(disposition).await,
        Resource::Pipe(_) => Ok(()),
    }
}

async fn close_resource(resource: Resource, path: &RPath) -> Result<()> {
    match resource {
        Resource::File(f) => f.close().await,
        Resource::Directory(d) => d.close().await,
        Resource::Pipe(p) => p.close().await,
    }
    .map_err(|e| map_smb_err(e, path.as_str()))
}

/// Translates SMB failures into the core error space. Connection-level problems
/// must become `Offline`/`Timeout` so the mount manager retries instead of
/// reporting a permanent failure to the user.
pub fn map_smb_err(e: smb::Error, ctx: &str) -> Error {
    use smb::Error as E;
    match e {
        E::ReceivedErrorMessage(status, _) | E::UnexpectedMessageStatus(status) => {
            Error::from_nt_status(status, ctx)
        }
        E::NotFound(what) => Error::NotFound(format!("{ctx}: {what}")),
        E::SspiError(e) => Error::AuthFailed(format!("{ctx}: {e}")),
        E::UnsupportedAuthenticationMechanism(m) => {
            Error::AuthFailed(format!("{ctx}: unsupported auth mechanism {m}"))
        }
        E::IoError(e) => match e.kind() {
            std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof => Error::Offline(format!("{ctx}: {e}")),
            std::io::ErrorKind::TimedOut => Error::Timeout(format!("{ctx}: {e}")),
            _ => Error::Io(e),
        },
        E::TransportError(e) => Error::Offline(format!("{ctx}: {e}")),
        E::OperationTimeout(task, d) => {
            Error::Timeout(format!("{ctx}: {task:?} after {}s", d.as_secs()))
        }
        E::NegotiationError(m) => Error::Backend(format!("{ctx}: SMB negotiation failed: {m}")),
        E::InvalidArgument(m) => Error::InvalidArgument(format!("{ctx}: {m}")),
        E::UnsupportedOperation(m) => Error::Unsupported(format!("{ctx}: {m}")),
        other => Error::Backend(format!("{ctx}: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> SmbTarget {
        SmbTarget {
            host: "nas.local".into(),
            port: 445,
            share: "data".into(),
            user: "u".into(),
            password: "p".into(),
            subpath: None,
        }
    }

    #[test]
    fn share_relative_paths_are_smb_shaped() {
        let b = SmbBackend::new("t", target());
        assert_eq!(b.share_relative(&RPath::root()), "");
        assert_eq!(b.share_relative(&RPath::new("/a/b.txt")), "a\\b.txt");

        let mut t = target();
        t.subpath = Some("/team/docs".into());
        let b = SmbBackend::new("t", t);
        assert_eq!(b.share_relative(&RPath::root()), "team\\docs");
        assert_eq!(
            b.share_relative(&RPath::new("/a/b.txt")),
            "team\\docs\\a\\b.txt"
        );
    }

    #[test]
    fn non_default_port_goes_into_the_server_name() {
        let mut t = target();
        t.port = 4455;
        assert_eq!(t.server(), "nas.local:4455");
        assert_eq!(target().server(), "nas.local");
    }

    #[test]
    fn unc_paths_are_built_for_root_and_children() {
        let b = SmbBackend::new("t", target());
        let unc = b.unc(&RPath::root()).unwrap();
        assert_eq!(unc.server(), "nas.local");
        assert_eq!(unc.share(), Some("data"));
        assert_eq!(unc.path(), None);

        let unc = b.unc(&RPath::new("/dir/f.txt")).unwrap();
        assert_eq!(unc.path(), Some("dir\\f.txt"));
    }

    #[test]
    fn errors_are_classified_for_the_retry_logic() {
        let offline = map_smb_err(
            smb::Error::IoError(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
            "x",
        );
        assert!(offline.is_retryable(), "{offline}");

        let denied = map_smb_err(
            smb::Error::UnexpectedMessageStatus(0xC000_0022),
            "x",
        );
        assert_eq!(denied.errno(), 13);
    }

    #[tokio::test]
    async fn operations_before_connect_report_offline() {
        let b = SmbBackend::new("t", target());
        let err = b.stat(&RPath::root()).await.unwrap_err();
        assert!(err.is_retryable(), "{err}");
    }
}
