//! WinFsp bridge.
//!
//! Windows file system semantics are stricter than POSIX, and most "it works in
//! cmd but Explorer/Office misbehaves" bugs live here:
//!
//! * WinFsp asks `get_security_by_name` before every open. It must answer with
//!   the real attributes (and ENOENT for missing files), because that is how
//!   Explorer decides whether a name exists.
//! * `create_options` carries FILE_DIRECTORY_FILE / FILE_NON_DIRECTORY_FILE, and
//!   the kernel expects STATUS_NOT_A_DIRECTORY / STATUS_FILE_IS_A_DIRECTORY when
//!   the request contradicts reality.
//! * Deletion is two-phase: `set_delete` validates (and is where
//!   STATUS_DIRECTORY_NOT_EMPTY belongs), `cleanup` with FspCleanupDelete
//!   performs it.
//! * Sizes and timestamps must be filled in on *every* reply, otherwise Explorer
//!   shows 0-byte files and Office refuses to save.
//! * The mount must happen in the user's logon session. The agent runs as a user
//!   process for exactly this reason: a drive letter created by a service in
//!   session 0 is invisible in the user's Explorer.
//!
//! Threading: WinFsp calls us from its own dispatcher threads, so each callback
//! blocks on the async VFS via `Handle::block_on`. The `FileSystemHost` itself is
//! not `Send`, so it is created, mounted and later stopped on one dedicated
//! thread that the session owns.

use std::ffi::c_void;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;
use tokio::runtime::Handle;

use winfsp::constants::FspCleanupFlags;
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, VolumeParams};
use winfsp::{FspError, U16CStr};
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

use mydrives_core::error::{Error, Result};
use mydrives_core::mount::{MountProvider, MountSession, MountSpec, MountTarget};
use mydrives_core::path::RPath;
use mydrives_core::types::{system_time_to_filetime, Attr, Fh, OpenOpts, SetAttr};
use mydrives_core::vfs::Vfs;

// Win32 bits we need; spelled out rather than pulling in the whole `windows`
// crate just for a handful of constants.
const FILE_READ_DATA: u32 = 0x0001;
const FILE_WRITE_DATA: u32 = 0x0002;
const FILE_APPEND_DATA: u32 = 0x0004;

const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;

const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;

const STATUS_NOT_A_DIRECTORY: i32 = 0xC000_0103u32 as i32;
const STATUS_FILE_IS_A_DIRECTORY: i32 = 0xC000_00BAu32 as i32;

/// Metadata lifetime handed to the Windows cache manager, in milliseconds. One
/// second keeps Explorer responsive without showing badly stale sizes.
const INFO_TIMEOUT_MS: u32 = 1000;

pub struct WinFspProvider;

impl WinFspProvider {
    pub fn new() -> Self {
        WinFspProvider
    }
}

impl Default for WinFspProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MountProvider for WinFspProvider {
    fn name(&self) -> &'static str {
        "winfsp"
    }

    fn probe(&self) -> Result<()> {
        ensure_winfsp_loaded()?;
        winfsp::winfsp_init()
            .map(|_| ())
            .map_err(|e| Error::Unsupported(format!(
                "WinFsp is not available ({e}). Install it from https://winfsp.dev/rel/ and restart the agent"
            )))
    }

    fn supports(&self, _target: &MountTarget) -> bool {
        // WinFsp mounts both drive letters and directories.
        true
    }

    fn mount(
        &self,
        spec: &MountSpec,
        vfs: Arc<Vfs>,
        rt: Handle,
    ) -> Result<Box<dyn MountSession>> {
        let mount_string = match &spec.target {
            MountTarget::Drive(c) => format!("{}:", c.to_ascii_uppercase()),
            MountTarget::Path(p) => p.display().to_string(),
        };

        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();

        let volume_name = spec.volume_name.clone();
        let readonly = spec.readonly;
        let id = spec.id.clone();
        let mount_for_thread = mount_string.clone();

        // The FileSystemHost is not Send, so it lives entirely on this thread.
        let thread = std::thread::Builder::new()
            .name(format!("winfsp-{id}"))
            .spawn(move || {
                let built = (|| -> std::result::Result<FileSystemHost<MyDrivesFs>, String> {
                    let _ = ensure_winfsp_loaded();
                    let _init = winfsp::winfsp_init().map_err(|e| {
                        format!("WinFsp could not be initialized ({e}); install it from https://winfsp.dev/rel/")
                    })?;

                    let mut params = VolumeParams::new();
                    params
                        .sector_size(4096)
                        .sectors_per_allocation_unit(1)
                        .max_component_length(255)
                        .volume_creation_time(system_time_to_filetime(SystemTime::now()))
                        .volume_serial_number(serial_of(&id))
                        .file_info_timeout(INFO_TIMEOUT_MS)
                        .case_sensitive_search(false)
                        .case_preserved_names(true)
                        .unicode_on_disk(true)
                        .persistent_acls(false)
                        .reparse_points(false)
                        .named_streams(false)
                        // Only ask us to clean up when something actually changed.
                        .post_cleanup_when_modified_only(true)
                        // We do the matching ourselves in read_directory.
                        .pass_query_directory_pattern(false)
                        .read_only_volume(readonly)
                        .filesystem_name("MyDrives");

                    let fs = MyDrivesFs {
                        vfs,
                        rt,
                        readonly,
                        volume_name,
                    };
                    // The type must be spelled out: `FileSystemHost` has both a
                    // fine- and a coarse-grained guard flavour, and `new`/`start`
                    // exist on both.
                    let mut host: FileSystemHost<MyDrivesFs> =
                        FileSystemHost::new(params, fs).map_err(|e| format!("{e}"))?;
                    host.mount(mount_for_thread.as_str())
                        .map_err(|e| format!("mounting {mount_for_thread} failed: {e}"))?;
                    host.start().map_err(|e| format!("{e}"))?;
                    Ok(host)
                })();

                match built {
                    Ok(mut host) => {
                        let _ = ready_tx.send(Ok(()));
                        // Park until the session is told to unmount.
                        let _ = stop_rx.recv();
                        host.stop();
                        host.unmount();
                        tracing::info!(remote = %id, "WinFsp dispatcher stopped");
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| Error::Backend(format!("spawning the WinFsp thread failed: {e}")))?;

        match ready_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(Error::Backend(e)),
            Err(_) => {
                return Err(Error::Timeout(
                    "WinFsp did not finish mounting within 30s".into(),
                ))
            }
        }

        Ok(Box::new(WinFspMount {
            mountpoint: mount_string,
            stop: Mutex::new(Some(stop_tx)),
            thread: Mutex::new(Some(thread)),
        }))
    }
}

/// Loads `winfsp-x64.dll` from wherever WinFsp was installed.
///
/// `winfsp_init()` only does `LoadLibrary("winfsp-x64.dll")` by plain name (the
/// registry lookup needs the crate's `system` feature, which in turn needs the
/// WinFsp SDK at build time). WinFsp's installer does not add its `bin`
/// directory to `PATH`, so the plain name usually fails. Loading the DLL by
/// absolute path first means the later load-by-name finds the module that is
/// already in the process.
fn ensure_winfsp_loaded() -> Result<()> {
    static LOADED: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

    let found = LOADED.get_or_init(|| {
        for candidate in winfsp_dll_candidates() {
            if !candidate.exists() {
                continue;
            }
            // SAFETY: loading a DLL runs its entry point; this is the officially
            // documented way to initialize WinFsp.
            match unsafe { libloading::Library::new(&candidate) } {
                Ok(library) => {
                    // The module must stay loaded for the lifetime of the process.
                    std::mem::forget(library);
                    tracing::debug!(path = %candidate.display(), "loaded the WinFsp DLL");
                    return Some(candidate);
                }
                Err(e) => tracing::debug!(path = %candidate.display(), error = %e, "WinFsp DLL did not load"),
            }
        }
        None
    });

    if found.is_some() {
        return Ok(());
    }
    // Not fatal here: winfsp_init() may still succeed if the DLL sits next to
    // the executable or on PATH.
    Ok(())
}

fn winfsp_dll_candidates() -> Vec<std::path::PathBuf> {
    let dll = if cfg!(target_arch = "x86_64") {
        "winfsp-x64.dll"
    } else if cfg!(target_arch = "aarch64") {
        "winfsp-a64.dll"
    } else {
        "winfsp-x86.dll"
    };
    let mut out = Vec::new();
    if let Ok(explicit) = std::env::var("MYDRIVES_WINFSP_DLL") {
        out.push(std::path::PathBuf::from(explicit));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join(dll));
        }
    }
    for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
        if let Ok(base) = std::env::var(var) {
            out.push(std::path::PathBuf::from(base).join("WinFsp").join("bin").join(dll));
        }
    }
    out
}

fn serial_of(id: &str) -> u32 {
    // Stable per remote, so Windows does not treat a remount as a new volume.
    let mut hash: u32 = 2166136261;
    for b in id.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(16777619);
    }
    hash
}

struct WinFspMount {
    mountpoint: String,
    stop: Mutex<Option<mpsc::Sender<()>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl MountSession for WinFspMount {
    fn mountpoint(&self) -> String {
        self.mountpoint.clone()
    }

    fn unmount(self: Box<Self>) -> Result<()> {
        if let Some(stop) = self.stop.lock().take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.lock().take() {
            thread
                .join()
                .map_err(|_| Error::Backend("the WinFsp thread panicked".into()))?;
        }
        Ok(())
    }
}

impl Drop for WinFspMount {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.lock().take() {
            let _ = stop.send(());
        }
    }
}

struct MyDrivesFs {
    vfs: Arc<Vfs>,
    rt: Handle,
    readonly: bool,
    /// What Explorer prints next to the drive letter: the drive's name, not
    /// its source URL - an external disk shows "Backup", not a device path.
    volume_name: String,
}

/// What WinFsp hands back to us on every call for an open file.
pub struct FileCtx {
    path: RPath,
    fh: Option<Fh>,
    is_dir: bool,
    dir_buffer: DirBuffer,
}

fn fsp(e: Error) -> FspError {
    FspError::NTSTATUS(e.nt_status() as i32)
}

fn path_of(file_name: &U16CStr) -> RPath {
    // WinFsp paths are `\dir\file`; RPath::new normalizes separators.
    RPath::new(&file_name.to_string_lossy())
}

fn win_attributes(attr: &Attr) -> u32 {
    let mut out = 0;
    if attr.is_dir() {
        out |= FILE_ATTRIBUTE_DIRECTORY;
    }
    if attr.readonly {
        out |= FILE_ATTRIBUTE_READONLY;
    }
    if attr.hidden {
        out |= FILE_ATTRIBUTE_HIDDEN;
    }
    if out == 0 {
        out = FILE_ATTRIBUTE_NORMAL;
    }
    out
}

fn fill_info(attr: &Attr, info: &mut FileInfo) {
    info.file_attributes = win_attributes(attr);
    info.reparse_tag = 0;
    info.file_size = attr.size;
    // Round up to the sector size, like a real volume.
    info.allocation_size = attr.size.div_ceil(4096) * 4096;
    info.creation_time = system_time_to_filetime(attr.crtime);
    info.last_access_time = system_time_to_filetime(attr.atime);
    info.last_write_time = system_time_to_filetime(attr.mtime);
    info.change_time = system_time_to_filetime(attr.ctime);
    info.index_number = 0;
    info.hard_links = 0;
    info.ea_size = 0;
}

impl MyDrivesFs {
    fn stat(&self, path: &RPath) -> Result<Attr> {
        self.rt.block_on(self.vfs.stat_path(path))
    }

    fn deny_if_readonly(&self) -> winfsp::Result<()> {
        if self.readonly {
            // STATUS_MEDIA_WRITE_PROTECTED
            return Err(FspError::NTSTATUS(0xC000_00A2u32 as i32));
        }
        Ok(())
    }
}

impl FileSystemContext for MyDrivesFs {
    type FileContext = FileCtx;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let path = path_of(file_name);
        let attr = self.stat(&path).map_err(fsp)?;
        Ok(FileSecurity {
            reparse: false,
            // No ACLs are published (persistent_acls is off), so Windows applies
            // the volume default.
            sz_security_descriptor: 0,
            attributes: win_attributes(&attr),
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        granted_access: FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let path = path_of(file_name);
        let attr = self.stat(&path).map_err(fsp)?;

        if attr.is_dir() && create_options & FILE_NON_DIRECTORY_FILE != 0 {
            return Err(FspError::NTSTATUS(STATUS_FILE_IS_A_DIRECTORY));
        }
        if !attr.is_dir() && create_options & FILE_DIRECTORY_FILE != 0 {
            return Err(FspError::NTSTATUS(STATUS_NOT_A_DIRECTORY));
        }

        // FILE_ACCESS_RIGHTS is a plain u32 in winfsp-sys.
        let access = granted_access;
        let wants_write = access & (FILE_WRITE_DATA | FILE_APPEND_DATA) != 0;
        if wants_write {
            self.deny_if_readonly()?;
            // Windows semantics: opening a FILE_ATTRIBUTE_READONLY file for write
            // is STATUS_ACCESS_DENIED. (POSIX differs, which is why the VFS does
            // not enforce this and each mount layer decides.)
            if attr.readonly {
                return Err(FspError::NTSTATUS(0xC000_0022u32 as i32));
            }
        }

        let fh = if attr.is_dir() {
            None
        } else {
            let opts = OpenOpts {
                read: access & FILE_READ_DATA != 0 || !wants_write,
                write: wants_write,
                truncate: false,
                append: access & FILE_APPEND_DATA != 0,
            };
            Some(self.rt.block_on(self.vfs.open_path(&path, opts)).map_err(fsp)?)
        };

        fill_info(&attr, file_info.as_mut());
        Ok(FileCtx {
            path,
            fh,
            is_dir: attr.is_dir(),
            dir_buffer: DirBuffer::new(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        granted_access: FILE_ACCESS_RIGHTS,
        _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        self.deny_if_readonly()?;
        let path = path_of(file_name);

        if create_options & FILE_DIRECTORY_FILE != 0 {
            let (_ino, attr) = self.rt.block_on(self.vfs.mkdir_path(&path)).map_err(fsp)?;
            fill_info(&attr, file_info.as_mut());
            return Ok(FileCtx {
                path,
                fh: None,
                is_dir: true,
                dir_buffer: DirBuffer::new(),
            });
        }

        let access = granted_access;
        let opts = OpenOpts {
            read: access & FILE_READ_DATA != 0,
            write: true,
            truncate: false,
            append: access & FILE_APPEND_DATA != 0,
        };
        let (fh, _ino, attr) = self
            .rt
            .block_on(self.vfs.create_path(&path, opts))
            .map_err(fsp)?;
        fill_info(&attr, file_info.as_mut());
        Ok(FileCtx {
            path,
            fh: Some(fh),
            is_dir: false,
            dir_buffer: DirBuffer::new(),
        })
    }

    fn close(&self, context: Self::FileContext) {
        if let Some(fh) = context.fh {
            if let Err(e) = self.rt.block_on(self.vfs.release(fh)) {
                tracing::warn!(path = %context.path, error = %e, "closing the remote handle failed");
            }
        }
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        // This is where a delete actually happens on Windows: set_delete only
        // validates it.
        if !FspCleanupFlags::FspCleanupDelete.is_flagged(flags) {
            return;
        }
        let result = if context.is_dir {
            self.rt.block_on(self.vfs.rmdir_path(&context.path))
        } else {
            self.rt.block_on(self.vfs.unlink_path(&context.path))
        };
        if let Err(e) = result {
            tracing::warn!(path = %context.path, error = %e, "delete on cleanup failed");
        }
    }

    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let Some(context) = context else {
            // A null context means "flush the whole volume"; we have no
            // volume-wide buffer, so there is nothing to do.
            return Ok(());
        };
        if let Some(fh) = context.fh {
            self.rt.block_on(self.vfs.flush(fh)).map_err(fsp)?;
        }
        let attr = self.stat(&context.path).map_err(fsp)?;
        fill_info(&attr, file_info);
        Ok(())
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let attr = self.stat(&context.path).map_err(fsp)?;
        fill_info(&attr, file_info);
        Ok(())
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        file_attributes: u32,
        _creation_time: u64,
        last_access_time: u64,
        last_write_time: u64,
        _change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.deny_if_readonly()?;
        // 0 and INVALID_FILE_ATTRIBUTES both mean "leave unchanged" in WinFsp.
        let readonly = if file_attributes == 0 || file_attributes == u32::MAX {
            None
        } else {
            Some(file_attributes & FILE_ATTRIBUTE_READONLY != 0)
        };
        let set = SetAttr {
            size: None,
            mtime: filetime_opt(last_write_time),
            atime: filetime_opt(last_access_time),
            readonly,
        };
        let attr = self
            .rt
            .block_on(self.vfs.setattr_path(&context.path, set))
            .map_err(fsp)?;
        fill_info(&attr, file_info);
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.deny_if_readonly()?;
        // Allocation-size changes are advisory for a network filesystem; only the
        // end-of-file matters.
        if !set_allocation_size {
            match context.fh {
                Some(fh) => self.rt.block_on(self.vfs.truncate(fh, new_size)).map_err(fsp)?,
                None => {
                    self.rt
                        .block_on(self.vfs.setattr_path(
                            &context.path,
                            SetAttr {
                                size: Some(new_size),
                                ..Default::default()
                            },
                        ))
                        .map(|_| ())
                        .map_err(fsp)?
                }
            }
        }
        let attr = self.stat(&context.path).map_err(fsp)?;
        fill_info(&attr, file_info);
        Ok(())
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        let fh = context
            .fh
            .ok_or_else(|| FspError::NTSTATUS(STATUS_FILE_IS_A_DIRECTORY))?;
        let data = self
            .rt
            .block_on(self.vfs.read(fh, offset, buffer.len() as u32))
            .map_err(fsp)?;
        if data.is_empty() {
            // STATUS_END_OF_FILE: Windows expects this rather than a zero-length
            // successful read.
            return Err(FspError::NTSTATUS(0xC000_0011u32 as i32));
        }
        buffer[..data.len()].copy_from_slice(&data);
        Ok(data.len() as u32)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        self.deny_if_readonly()?;
        let fh = context
            .fh
            .ok_or_else(|| FspError::NTSTATUS(STATUS_FILE_IS_A_DIRECTORY))?;
        let attr = self.stat(&context.path).map_err(fsp)?;

        let offset = if write_to_eof { attr.size } else { offset };
        let mut buffer = buffer;
        if constrained_io {
            // Must not extend the file: clip, and report success for the part
            // that fits (this is how the cache manager flushes partial pages).
            if offset >= attr.size {
                fill_info(&attr, file_info);
                return Ok(0);
            }
            let room = (attr.size - offset) as usize;
            if buffer.len() > room {
                buffer = &buffer[..room];
            }
        }

        let written = self
            .rt
            .block_on(self.vfs.write(fh, offset, buffer))
            .map_err(fsp)?;
        let attr = self.stat(&context.path).map_err(fsp)?;
        fill_info(&attr, file_info);
        Ok(written)
    }

    fn rename(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        self.deny_if_readonly()?;
        let from = path_of(file_name);
        let to = path_of(new_file_name);
        self.rt
            .block_on(self.vfs.rename_path(&from, &to, replace_if_exists))
            .map_err(fsp)
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        self.deny_if_readonly()?;
        if !delete_file {
            return Ok(());
        }
        // Validate now so Explorer gets the real reason; the delete itself runs
        // in cleanup().
        if context.is_dir {
            let entries = self
                .rt
                .block_on(self.vfs.readdir_path(&context.path))
                .map_err(fsp)?;
            if !entries.is_empty() {
                // STATUS_DIRECTORY_NOT_EMPTY
                return Err(FspError::NTSTATUS(0xC000_0101u32 as i32));
            }
        }
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        if !context.is_dir {
            return Err(FspError::NTSTATUS(STATUS_NOT_A_DIRECTORY));
        }
        // The buffer is filled once per enumeration; WinFsp then serves the
        // marker-based continuation from it.
        if let Ok(lock) = context.dir_buffer.acquire(marker.is_none(), None) {
            let entries = self
                .rt
                .block_on(self.vfs.readdir_path(&context.path))
                .map_err(fsp)?;

            let mut dir_info = DirInfo::<255>::new();

            // Windows expects "." and ".." from every directory except the root.
            if !context.path.is_root() {
                let self_attr = self.stat(&context.path).map_err(fsp)?;
                for name in [".", ".."] {
                    dir_info.reset();
                    dir_info.set_name(name)?;
                    fill_info(&self_attr, dir_info.file_info_mut());
                    lock.write(&mut dir_info)?;
                }
            }

            for entry in entries.iter() {
                dir_info.reset();
                dir_info.set_name(entry.name.as_str())?;
                fill_info(&entry.attr, dir_info.file_info_mut());
                lock.write(&mut dir_info)?;
            }
        }
        Ok(context.dir_buffer.read(marker, buffer))
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        let st = self.rt.block_on(self.vfs.statfs()).map_err(fsp)?;
        out_volume_info.total_size = st.total_bytes;
        out_volume_info.free_size = st.free_bytes;
        // WinFsp keeps 32 UTF-16 units for the label; longer names are cut
        // rather than rejected, which would fail the whole call.
        let label: String = self.volume_name.chars().take(32).collect();
        out_volume_info.set_volume_label(if label.is_empty() {
            self.vfs.backend().describe()
        } else {
            label
        });
        Ok(())
    }
}

fn filetime_opt(ft: u64) -> Option<SystemTime> {
    if ft == 0 {
        None
    } else {
        Some(mydrives_core::types::filetime_to_system_time(ft))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mydrives_core::types::FileKind;

    #[test]
    fn attributes_map_both_ways() {
        let mut attr = Attr::file(10);
        assert_eq!(win_attributes(&attr), FILE_ATTRIBUTE_NORMAL);
        attr.readonly = true;
        assert_eq!(win_attributes(&attr), FILE_ATTRIBUTE_READONLY);
        let dir = Attr::dir();
        assert_eq!(win_attributes(&dir), FILE_ATTRIBUTE_DIRECTORY);
        assert_eq!(dir.kind, FileKind::Dir);
    }

    #[test]
    fn windows_paths_are_normalized() {
        assert_eq!(RPath::new("\\a\\b.txt").as_str(), "/a/b.txt");
        assert_eq!(RPath::new("\\").as_str(), "/");
    }

    #[test]
    fn serial_is_stable_per_remote() {
        assert_eq!(serial_of("nas"), serial_of("nas"));
        assert_ne!(serial_of("nas"), serial_of("nas2"));
    }
}
