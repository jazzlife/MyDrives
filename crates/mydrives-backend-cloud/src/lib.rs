//! Cloud drives: services that hand files out whole, over HTTPS.
//!
//! 네이버 MYBOX, and after it Google Drive, OneDrive and Dropbox, all look
//! alike from a filesystem's point of view: every file and folder has an
//! **id**, folders are listed by id, and a file is downloaded or uploaded as
//! one piece through a URL the service hands out. None of them is a
//! filesystem with offsets. So the work of being one - turning paths into
//! ids, keeping a local copy of an open file, uploading it when it closes -
//! is done once, here, in [`CloudBackend`], and each service only has to
//! answer the handful of calls in [`CloudApi`].
//!
//! Reads: a service that honours HTTP range requests serves them straight
//! from the download URL, a chunk at a time; one that does not (MYBOX) has
//! the whole file fetched into a local copy on the first read. Writes always
//! go to the local copy and reach the service as one upload at flush or
//! close - the same trade the FTP backend makes, because the kernel hands
//! writes over out of order and no service takes them that way.

pub mod dropbox;
pub mod gdrive;
pub mod mybox;
pub mod onedrive;
mod rfc3339;
mod stage;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use parking_lot::Mutex;

use mydrives_core::backend::{BFile, Backend};
use mydrives_core::error::{Error, Result};
use mydrives_core::path::RPath;
use mydrives_core::types::{Attr, Caps, DirEntry, FileKind, FsStat, OpenOpts, SetAttr};

use stage::Staged;

/// A service's own identifier for a file or folder.
pub type ItemId = String;

/// One file or folder as the service describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: ItemId,
    pub name: String,
    pub kind: FileKind,
    pub size: u64,
    pub created: SystemTime,
    pub modified: SystemTime,
    pub hidden: bool,
}

impl Item {
    pub fn folder(id: impl Into<ItemId>, name: impl Into<String>) -> Item {
        let now = SystemTime::now();
        Item {
            id: id.into(),
            name: name.into(),
            kind: FileKind::Dir,
            size: 0,
            created: now,
            modified: now,
            hidden: false,
        }
    }

    fn attr(&self) -> Attr {
        let size = if self.kind == FileKind::Dir { 0 } else { self.size };
        let mut attr = Attr::new(self.kind, size, self.modified);
        attr.crtime = self.created;
        attr.ctime = self.modified;
        attr.atime = self.modified;
        attr.hidden = self.hidden;
        attr
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CloudCaps {
    /// `download_range` works: reads need no local copy.
    pub range_reads: bool,
    /// Two names differing only in case are two different files.
    pub case_sensitive: bool,
}

/// What a service has to answer. Ids are the service's; paths never appear.
#[async_trait]
pub trait CloudApi: Send + Sync + 'static {
    /// For logs and the drive's "source" line: `mybox:///Docs`.
    fn describe(&self) -> String;
    fn caps(&self) -> CloudCaps;
    /// The id of the account's top folder.
    fn root_id(&self) -> ItemId;
    /// Prove the credentials work - cheap, called on every mount.
    async fn connect(&self) -> Result<()>;
    /// Every item directly inside `folder`, all pages.
    async fn list(&self, folder: &str) -> Result<Vec<Item>>;
    async fn mkdir(&self, parent: &str, name: &str) -> Result<Item>;
    /// Removes a file, or an empty folder.
    async fn delete(&self, id: &str) -> Result<()>;
    async fn rename(&self, id: &str, name: &str) -> Result<()>;
    async fn move_to(&self, id: &str, parent: &str) -> Result<()>;
    /// The whole file, written to `into` from its current position.
    async fn download(&self, id: &str, into: &mut tokio::fs::File) -> Result<u64>;
    /// `len` bytes from `offset`; only called when `caps().range_reads`.
    async fn download_range(&self, _id: &str, _offset: u64, _len: u32) -> Result<Vec<u8>> {
        Err(Error::unsupported("range reads"))
    }
    /// Store `len` bytes read from `from` as `name` in `parent`, replacing a
    /// file of that name if there is one.
    async fn upload(
        &self,
        parent: &str,
        name: &str,
        from: tokio::fs::File,
        len: u64,
        modified: Option<SystemTime>,
    ) -> Result<()>;
    async fn quota(&self) -> Result<FsStat>;
}

/// How long a folder listing is trusted for turning names into ids. The VFS
/// above has its own cache for attributes; this one exists so that a path
/// deep in the tree does not list every folder on the way down twice.
const LISTING_TTL: Duration = Duration::from_secs(10);

struct Listing {
    items: Arc<Vec<Item>>,
    at: Instant,
}

struct OpenFile {
    path: RPath,
    item: Mutex<Item>,
    opts: OpenOpts,
    /// Created or truncated by this handle: nothing on the service is worth
    /// downloading before the first write.
    starts_empty: bool,
    staged: tokio::sync::Mutex<Option<Staged>>,
}

/// A filesystem over a [`CloudApi`].
pub struct CloudBackend<A: CloudApi> {
    id: String,
    api: A,
    /// The folder inside the account that is the drive's top, as a path from
    /// the account's root. `None` mounts the whole account.
    subpath: Option<RPath>,
    /// Resolved at connect: the item that is the drive's top.
    root: Mutex<Option<Item>>,
    /// Keyed by path relative to the drive's top.
    listings: Mutex<HashMap<String, Listing>>,
    open: Mutex<HashMap<u64, Arc<OpenFile>>>,
    next_handle: AtomicU64,
    stage_dir: PathBuf,
}

impl<A: CloudApi> CloudBackend<A> {
    pub fn new(id: impl Into<String>, api: A, subpath: Option<&str>) -> Self {
        let id = id.into();
        let subpath = subpath
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "/")
            .map(RPath::new);
        let stage_dir = std::env::temp_dir().join(format!("mydrives-cloud-{id}"));
        CloudBackend {
            id,
            api,
            subpath,
            root: Mutex::new(None),
            listings: Mutex::new(HashMap::new()),
            open: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
            stage_dir,
        }
    }

    pub fn api(&self) -> &A {
        &self.api
    }

    fn names_match(&self, a: &str, b: &str) -> bool {
        if self.api.caps().case_sensitive {
            a == b
        } else {
            a.to_lowercase() == b.to_lowercase()
        }
    }

    /// The item at the drive's top, finding it inside the account on the
    /// first call (and on a call after a disconnect).
    async fn root_item(&self) -> Result<Item> {
        if let Some(item) = self.root.lock().clone() {
            return Ok(item);
        }
        let mut current = Item::folder(self.api.root_id(), "/");
        if let Some(sub) = &self.subpath {
            for part in sub.as_str().split('/').filter(|p| !p.is_empty()) {
                let items = self.api.list(&current.id).await?;
                let next = items
                    .into_iter()
                    .find(|i| i.kind == FileKind::Dir && self.names_match(&i.name, part))
                    .ok_or_else(|| {
                        Error::NotFound(format!(
                            "{}: 계정 안에 '{}' 폴더가 없습니다",
                            self.api.describe(),
                            sub.as_str()
                        ))
                    })?;
                current = next;
            }
        }
        *self.root.lock() = Some(current.clone());
        Ok(current)
    }

    /// The item at `path`, found by walking down from the drive's top one
    /// listing at a time - each from cache when it is fresh, so a path deep
    /// in the tree costs one request per folder on the way the first time
    /// and none after.
    async fn item_of(&self, path: &RPath) -> Result<Item> {
        let mut current = self.root_item().await?;
        let mut here = RPath::root();
        for part in path.as_str().split('/').filter(|p| !p.is_empty()) {
            if current.kind != FileKind::Dir {
                return Err(Error::NotADirectory(here.to_string()));
            }
            let items = self.cached_or_list(&here, &current.id).await?;
            let next = items
                .iter()
                .find(|i| self.names_match(&i.name, part))
                .cloned()
                .ok_or_else(|| Error::NotFound(here.join(part).to_string()))?;
            here = here.join(part);
            current = next;
        }
        Ok(current)
    }

    /// The listing of the folder at `dir`.
    async fn listing_of(&self, dir: &RPath) -> Result<Arc<Vec<Item>>> {
        if let Some(l) = self.listings.lock().get(dir.as_str()) {
            if l.at.elapsed() <= LISTING_TTL {
                return Ok(l.items.clone());
            }
        }
        let folder = self.item_of(dir).await?;
        if folder.kind != FileKind::Dir {
            return Err(Error::NotADirectory(dir.to_string()));
        }
        self.cached_or_list(dir, &folder.id).await
    }

    /// The listing of the folder `folder_id` at path `dir`, from cache when
    /// fresh.
    async fn cached_or_list(&self, dir: &RPath, folder_id: &str) -> Result<Arc<Vec<Item>>> {
        if let Some(l) = self.listings.lock().get(dir.as_str()) {
            if l.at.elapsed() <= LISTING_TTL {
                return Ok(l.items.clone());
            }
        }
        let items = Arc::new(self.api.list(folder_id).await?);
        self.listings.lock().insert(
            dir.as_str().to_string(),
            Listing {
                items: items.clone(),
                at: Instant::now(),
            },
        );
        Ok(items)
    }

    fn forget(&self, dir: &RPath) {
        self.listings.lock().remove(dir.as_str());
    }

    /// Drops the cached listing of `path`'s parent - the folder whose
    /// contents just changed.
    fn forget_parent(&self, path: &RPath) {
        if let Some(parent) = path.parent() {
            self.forget(&parent);
        }
    }

    fn handle(&self, f: BFile) -> Result<Arc<OpenFile>> {
        self.open
            .lock()
            .get(&f.0)
            .cloned()
            .ok_or_else(|| Error::InvalidArgument(format!("bad cloud handle {}", f.0)))
    }

    fn register(&self, path: &RPath, item: Item, opts: OpenOpts, starts_empty: bool) -> BFile {
        let id = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.open.lock().insert(
            id,
            Arc::new(OpenFile {
                path: path.clone(),
                item: Mutex::new(item),
                opts,
                starts_empty,
                staged: tokio::sync::Mutex::new(None),
            }),
        );
        BFile(id)
    }

    /// Makes sure `slot` holds a local copy of the file: an empty one if the
    /// handle created or truncated it, otherwise what the service has.
    async fn stage(&self, file: &OpenFile, slot: &mut Option<Staged>) -> Result<()> {
        if slot.is_some() {
            return Ok(());
        }
        let mut staged = Staged::create(&self.stage_dir).await?;
        if !file.starts_empty {
            let item = file.item.lock().clone();
            if item.size > 0 {
                let got = self.api.download(&item.id, &mut staged.file).await?;
                staged.len = got;
            }
        }
        *slot = Some(staged);
        Ok(())
    }

    /// Sends the local copy to the service, if anything was written.
    async fn upload(&self, file: &OpenFile, staged: &mut Staged) -> Result<()> {
        if !staged.dirty {
            return Ok(());
        }
        let parent_path = file.path.parent().unwrap_or_else(RPath::root);
        let parent = self.item_of(&parent_path).await?;
        let reader = staged.reader().await?;
        self.api
            .upload(
                &parent.id,
                file.path.name(),
                reader,
                staged.len,
                Some(SystemTime::now()),
            )
            .await?;
        staged.dirty = false;
        // The service may have given the file a new id (an overwrite often
        // does); the next look at the folder finds out.
        self.forget(&parent_path);
        if let Ok(fresh) = self.item_of(&file.path).await {
            *file.item.lock() = fresh;
        } else {
            let mut item = file.item.lock();
            item.size = staged.len;
            item.modified = SystemTime::now();
        }
        Ok(())
    }
}

#[async_trait]
impl<A: CloudApi> Backend for CloudBackend<A> {
    fn id(&self) -> &str {
        &self.id
    }

    fn describe(&self) -> String {
        self.api.describe()
    }

    fn caps(&self) -> Caps {
        Caps {
            write: true,
            // No service here lets a timestamp be set on its own; it is what
            // the upload says it is.
            set_times: false,
            truncate: true,
            // A move onto an existing name is a delete-then-move in this
            // layer, not one atomic step on the service.
            rename_replace: false,
            case_sensitive: self.api.caps().case_sensitive,
            max_io: 1 << 20,
        }
    }

    async fn connect(&self) -> Result<()> {
        self.api.connect().await?;
        *self.root.lock() = None;
        self.listings.lock().clear();
        self.root_item().await?;
        tracing::info!(target = %self.api.describe(), "cloud drive connected");
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        let open: Vec<Arc<OpenFile>> = self.open.lock().drain().map(|(_, v)| v).collect();
        for file in open {
            let mut slot = file.staged.lock().await;
            if let Some(staged) = slot.as_mut() {
                if let Err(e) = self.upload(&file, staged).await {
                    tracing::warn!(path = %file.path.as_str(), error = %e, "연결을 닫기 전에 올리지 못한 변경이 있습니다");
                }
            }
            *slot = None;
        }
        *self.root.lock() = None;
        self.listings.lock().clear();
        Ok(())
    }

    async fn stat(&self, path: &RPath) -> Result<Attr> {
        Ok(self.item_of(path).await?.attr())
    }

    async fn readdir(&self, path: &RPath) -> Result<Vec<DirEntry>> {
        let items = self.listing_of(path).await?;
        Ok(items
            .iter()
            .map(|i| DirEntry {
                name: i.name.clone(),
                attr: i.attr(),
            })
            .collect())
    }

    async fn open(&self, path: &RPath, opts: OpenOpts) -> Result<BFile> {
        let item = self.item_of(path).await?;
        if item.kind == FileKind::Dir {
            return Err(Error::IsADirectory(path.to_string()));
        }
        Ok(self.register(path, item, opts, opts.truncate))
    }

    async fn create(&self, path: &RPath) -> Result<BFile> {
        let parent = path.parent().unwrap_or_else(RPath::root);
        let siblings = self.listing_of(&parent).await?;
        if siblings.iter().any(|i| self.names_match(&i.name, path.name())) {
            return Err(Error::AlreadyExists(path.to_string()));
        }
        // The service learns of the file at the first flush; until then the
        // handle is the only place it exists, and the listing is told so a
        // stat in between does not say "no such file".
        let now = SystemTime::now();
        let item = Item {
            id: String::new(),
            name: path.name().to_string(),
            kind: FileKind::File,
            size: 0,
            created: now,
            modified: now,
            hidden: false,
        };
        {
            let mut listings = self.listings.lock();
            if let Some(l) = listings.get_mut(parent.as_str()) {
                let mut items = (*l.items).clone();
                items.push(item.clone());
                l.items = Arc::new(items);
            }
        }
        let opts = OpenOpts {
            read: true,
            write: true,
            ..OpenOpts::default()
        };
        let fh = self.register(path, item, opts, true);
        // An empty file is still a file: the local copy starts dirty, so a
        // create that is never written to (`touch`) still reaches the service
        // at close - as one upload, not an empty one followed by the real one.
        let file = self.handle(fh)?;
        let mut slot = file.staged.lock().await;
        self.stage(&file, &mut slot).await?;
        if let Some(staged) = slot.as_mut() {
            staged.dirty = true;
        }
        Ok(fh)
    }

    async fn read_at(&self, f: BFile, offset: u64, len: u32) -> Result<Vec<u8>> {
        let file = self.handle(f)?;
        let mut slot = file.staged.lock().await;
        if slot.is_none() && self.api.caps().range_reads && !file.starts_empty {
            let item = file.item.lock().clone();
            if offset >= item.size {
                return Ok(Vec::new());
            }
            let want = len.min((item.size - offset) as u32);
            return self.api.download_range(&item.id, offset, want).await;
        }
        self.stage(&file, &mut slot).await?;
        let staged = slot.as_mut().expect("staged just above");
        staged.read_at(offset, len).await
    }

    async fn write_at(&self, f: BFile, offset: u64, data: &[u8]) -> Result<u32> {
        let file = self.handle(f)?;
        if !file.opts.write {
            return Err(Error::PermissionDenied(file.path.as_str().to_string()));
        }
        let mut slot = file.staged.lock().await;
        self.stage(&file, &mut slot).await?;
        let staged = slot.as_mut().expect("staged just above");
        staged.write_at(offset, data).await?;
        Ok(data.len() as u32)
    }

    async fn flush(&self, f: BFile) -> Result<()> {
        let file = self.handle(f)?;
        let mut slot = file.staged.lock().await;
        match slot.as_mut() {
            Some(staged) => self.upload(&file, staged).await,
            None => Ok(()),
        }
    }

    async fn close(&self, f: BFile) -> Result<()> {
        let file = self.open.lock().remove(&f.0);
        let Some(file) = file else { return Ok(()) };
        let mut slot = file.staged.lock().await;
        let uploaded = match slot.as_mut() {
            Some(staged) => self.upload(&file, staged).await,
            None => Ok(()),
        };
        *slot = None;
        uploaded
    }

    async fn set_len(&self, path: &RPath, f: Option<BFile>, len: u64) -> Result<()> {
        if let Some(f) = f {
            if let Ok(file) = self.handle(f) {
                let mut slot = file.staged.lock().await;
                self.stage(&file, &mut slot).await?;
                let staged = slot.as_mut().expect("staged just above");
                return staged.set_len(len).await;
            }
        }
        // No handle: open one just for this, which stages, truncates and
        // uploads on close.
        let item = self.item_of(path).await?;
        if item.kind == FileKind::Dir {
            return Err(Error::IsADirectory(path.to_string()));
        }
        if item.size == len {
            return Ok(());
        }
        let opts = OpenOpts {
            read: true,
            write: true,
            truncate: len == 0,
            ..OpenOpts::default()
        };
        let fh = self.register(path, item, opts, len == 0);
        let result = async {
            let file = self.handle(fh)?;
            let mut slot = file.staged.lock().await;
            self.stage(&file, &mut slot).await?;
            slot.as_mut().expect("staged just above").set_len(len).await
        }
        .await;
        let closed = self.close(fh).await;
        result.and(closed)
    }

    async fn set_attr(&self, path: &RPath, f: Option<BFile>, set: &SetAttr) -> Result<()> {
        if let Some(len) = set.size {
            self.set_len(path, f, len).await?;
        }
        // Timestamps and the read-only bit have no home on these services.
        Ok(())
    }

    async fn mkdir(&self, path: &RPath) -> Result<()> {
        let parent = path.parent().unwrap_or_else(RPath::root);
        let siblings = self.listing_of(&parent).await?;
        if siblings.iter().any(|i| self.names_match(&i.name, path.name())) {
            return Err(Error::AlreadyExists(path.to_string()));
        }
        let folder = self.item_of(&parent).await?;
        self.api.mkdir(&folder.id, path.name()).await?;
        self.forget(&parent);
        Ok(())
    }

    async fn unlink(&self, path: &RPath) -> Result<()> {
        let item = self.item_of(path).await?;
        if item.kind == FileKind::Dir {
            return Err(Error::IsADirectory(path.to_string()));
        }
        self.api.delete(&item.id).await?;
        self.forget_parent(path);
        Ok(())
    }

    async fn rmdir(&self, path: &RPath) -> Result<()> {
        if path.is_root() {
            return Err(Error::PermissionDenied("the drive's top folder".into()));
        }
        let item = self.item_of(path).await?;
        if item.kind != FileKind::Dir {
            return Err(Error::NotADirectory(path.to_string()));
        }
        // POSIX removes only empty directories; the services remove whole
        // trees, so the check is ours to make.
        let inside = self.listing_of(path).await?;
        if !inside.is_empty() {
            return Err(Error::NotEmpty(path.to_string()));
        }
        self.api.delete(&item.id).await?;
        self.forget(path);
        self.forget_parent(path);
        Ok(())
    }

    async fn rename(&self, from: &RPath, to: &RPath, replace: bool) -> Result<()> {
        if from.is_root() || to.is_root() {
            return Err(Error::PermissionDenied("the drive's top folder".into()));
        }
        let item = self.item_of(from).await?;
        let from_parent = from.parent().unwrap_or_else(RPath::root);
        let to_parent = to.parent().unwrap_or_else(RPath::root);
        if let Ok(existing) = self.item_of(to).await {
            if !replace {
                return Err(Error::AlreadyExists(to.to_string()));
            }
            if existing.kind == FileKind::Dir {
                return Err(Error::IsADirectory(to.to_string()));
            }
            self.api.delete(&existing.id).await?;
            self.forget(&to_parent);
        }
        let same_parent = from_parent == to_parent;
        if !same_parent {
            let target = self.item_of(&to_parent).await?;
            if target.kind != FileKind::Dir {
                return Err(Error::NotADirectory(to_parent.to_string()));
            }
            self.api.move_to(&item.id, &target.id).await?;
        }
        if from.name() != to.name() {
            self.api.rename(&item.id, to.name()).await?;
        }
        self.forget(&from_parent);
        self.forget(&to_parent);
        if item.kind == FileKind::Dir {
            // Paths under the folder changed; their cached listings are keyed
            // by the old path.
            let prefix = format!("{}/", from.as_str());
            self.listings
                .lock()
                .retain(|k, _| k != from.as_str() && !k.starts_with(&prefix));
        }
        Ok(())
    }

    async fn statfs(&self) -> Result<FsStat> {
        self.api.quota().await
    }
}
