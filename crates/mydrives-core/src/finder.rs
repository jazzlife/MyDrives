//! Finder's sidebar: putting a drive there, by name, where one click opens it.
//!
//! The row Finder makes for a FUSE-T drive on its own is the *server* the
//! volume came from, and a server row opens a list of shares - one folder,
//! named after the mount, that has to be opened again. Measured on macOS 15
//! with every FUSE-T backend and naming the server after the volume; it is
//! how Finder treats every SMB server, a real NAS included.
//!
//! What opens straight into the files is a **pinned volume**: the row a person
//! gets by dragging a volume into the sidebar. Apple's API for that list
//! (`LSSharedFileList`) is dead - it reports no items and crashes on insert -
//! so this writes the list file itself, the `.sfl3` keyed archive that
//! `sharedfilelistd` owns. An item added to the *Favorites* list that turns
//! out to be a volume is moved by the daemon into the *Locations* list, which
//! is exactly where it should be. Finder reads the files once, so a change is
//! followed by a Finder restart; it relaunches by itself.
//!
//! Only the pieces of the archive format that these two lists use are
//! handled: dictionaries, arrays, dates, and plain values.

#![cfg_attr(not(target_os = "macos"), allow(dead_code, unused_variables))]

use std::path::Path;

use crate::error::Result;

/// Pin the volume at `mountpoint` to the sidebar. `Ok(true)` when the list
/// changed (and Finder was restarted), `Ok(false)` when it was already there.
pub fn pin_volume(mountpoint: &Path, name: &str) -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        macos::pin(mountpoint, name)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = name;
        Ok(false)
    }
}

/// Take the volume at `mountpoint` out of the sidebar again.
pub fn unpin_volume(mountpoint: &Path) -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        macos::unpin(mountpoint)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(false)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::path::{Path, PathBuf};

    use crate::error::{Error, Result};

    use super::archive::{self, Node};

    const LIST_DIR: &str = "Library/Application Support/com.apple.sharedfilelist";
    const FAVORITES: &str = "com.apple.LSSharedFileList.FavoriteItems.sfl3";
    const LOCATIONS: &str = "com.apple.LSSharedFileList.FavoriteVolumes.sfl3";

    fn list_path(name: &str) -> Result<PathBuf> {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| Error::Backend("HOME is not set".into()))?;
        Ok(PathBuf::from(home).join(LIST_DIR).join(name))
    }

    pub fn pin(mountpoint: &Path, name: &str) -> Result<bool> {
        // One pin at a time. Each one reads the list, appends and writes it
        // back, so two mounts finishing together used to lose an entry.
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // The row's label comes out of the bookmark, and a bookmark made
        // before the mount is live describes the disk the folder sits on -
        // which is why every drive once read "Macintosh HD". Wait for the
        // mount to show up, and skip the pin rather than record the wrong
        // volume. (The list carries no name of its own: adding a `Name` key
        // makes macOS reject the whole file and the sidebar falls back to
        // its defaults. Tried on 2026-11-11.)
        let _ = name;
        if !wait_until_mounted(mountpoint) {
            return Ok(false);
        }
        let wanted = normalize(mountpoint);
        // Already pinned - in either list - means nothing to do, and no
        // Finder restart.
        for list_name in [LOCATIONS, FAVORITES] {
            if let Some(list) = read_list(list_name)? {
                if list.items().any(|item| item_points_at(item, &wanted)) {
                    return Ok(false);
                }
            }
        }
        let Some(mut list) = read_list(FAVORITES)? else {
            return Err(Error::Backend(format!("{FAVORITES} does not exist")));
        };
        let bookmark = super::bookmark::create(mountpoint)?;
        list.push_item(Node::Dict {
            mutable: false,
            entries: vec![
                ("Bookmark".into(), Node::Data(bookmark)),
                ("uuid".into(), Node::Str(uuid())),
                ("visibility".into(), Node::Int(0)),
                (
                    "CustomItemProperties".into(),
                    Node::Dict {
                        mutable: false,
                        entries: vec![],
                    },
                ),
            ],
        });
        write_list(FAVORITES, &list)?;
        restart_finder();
        Ok(true)
    }

    /// Whether `path` is its own volume yet, giving the mount a moment to
    /// appear. A mount point differs from its parent in device number.
    fn wait_until_mounted(path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let Some(parent) = path.parent() else {
            return false;
        };
        for attempt in 0..20 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            let (Ok(here), Ok(up)) = (std::fs::metadata(path), std::fs::metadata(parent)) else {
                continue;
            };
            if here.dev() != up.dev() {
                return true;
            }
        }
        false
    }

    pub fn unpin(mountpoint: &Path) -> Result<bool> {
        let wanted = normalize(mountpoint);
        let mut changed = false;
        for name in [LOCATIONS, FAVORITES] {
            let Some(mut list) = read_list(name)? else {
                continue;
            };
            let before = list.items().count();
            list.retain_items(|item| !item_points_at(item, &wanted));
            if list.items().count() != before {
                write_list(name, &list)?;
                changed = true;
            }
        }
        if changed {
            restart_finder();
        }
        Ok(changed)
    }

    fn read_list(name: &str) -> Result<Option<archive::List>> {
        let path = list_path(name)?;
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Backend(format!("read {}: {e}", path.display()))),
        };
        archive::List::parse(&bytes)
            .map(Some)
            .map_err(|e| Error::Backend(format!("{}: {e}", path.display())))
    }

    fn write_list(name: &str, list: &archive::List) -> Result<()> {
        let path = list_path(name)?;
        let bytes = list.to_bytes()?;
        let tmp = path.with_extension("sfl3.mydrives-tmp");
        std::fs::write(&tmp, bytes)
            .and_then(|()| std::fs::rename(&tmp, &path))
            .map_err(|e| Error::Backend(format!("write {}: {e}", path.display())))
    }

    /// Does this list item's bookmark point at `wanted`?
    fn item_points_at(item: &Node, wanted: &str) -> bool {
        let Node::Dict { entries, .. } = item else {
            return false;
        };
        entries
            .iter()
            .find(|(k, _)| k == "Bookmark")
            .and_then(|(_, v)| match v {
                Node::Data(d) => super::bookmark::resolve(d),
                _ => None,
            })
            .map(|p| normalize(&p) == wanted)
            .unwrap_or(false)
    }

    fn normalize(path: &Path) -> String {
        let s = path.to_string_lossy();
        let s = s.trim_end_matches('/');
        // The resolver hands back `/private/tmp/...` for `/tmp/...`.
        s.strip_prefix("/private").unwrap_or(s).to_string()
    }

    fn uuid() -> String {
        // Finder only needs it unique; the random bytes come from the OS.
        let mut b = [0u8; 16];
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
            .is_err()
        {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            b[..16].copy_from_slice(&t.to_le_bytes());
        }
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        format!(
            "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
        )
    }

    /// The lists live in `sharedfilelistd`'s memory once read, and Finder
    /// asks it rather than the files. So the daemon goes first - it rereads
    /// the files when it comes back, and that is also when it moves a volume
    /// out of Favorites into Locations - and Finder second, so it asks the new
    /// daemon. Measured: either one alone leaves the sidebar as it was. Both
    /// relaunch on their own.
    fn restart_finder() {
        for name in ["sharedfilelistd", "Finder"] {
            let _ = std::process::Command::new("/usr/bin/killall")
                .arg(name)
                .output();
            std::thread::sleep(std::time::Duration::from_millis(800));
        }
    }
}

/// Bookmarks: the opaque blob a sidebar item stores instead of a path.
#[cfg(target_os = "macos")]
mod bookmark {
    use std::ffi::c_void;
    use std::os::raw::{c_long, c_ulong};
    use std::path::{Path, PathBuf};

    use crate::error::{Error, Result};

    type CFTypeRef = *const c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(
            alloc: CFTypeRef,
            buffer: *const u8,
            len: c_long,
            is_directory: u8,
        ) -> CFTypeRef;
        fn CFURLCreateBookmarkData(
            alloc: CFTypeRef,
            url: CFTypeRef,
            options: c_ulong,
            keys: CFTypeRef,
            relative_to: CFTypeRef,
            error: *mut CFTypeRef,
        ) -> CFTypeRef;
        fn CFURLCreateByResolvingBookmarkData(
            alloc: CFTypeRef,
            bookmark: CFTypeRef,
            options: c_ulong,
            relative_to: CFTypeRef,
            keys: CFTypeRef,
            is_stale: *mut u8,
            error: *mut CFTypeRef,
        ) -> CFTypeRef;
        fn CFURLGetFileSystemRepresentation(
            url: CFTypeRef,
            resolve_against_base: u8,
            buffer: *mut u8,
            max_len: c_long,
        ) -> u8;
        fn CFDataCreate(alloc: CFTypeRef, bytes: *const u8, len: c_long) -> CFTypeRef;
        fn CFDataGetLength(data: CFTypeRef) -> c_long;
        fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
        fn CFRelease(cf: CFTypeRef);
    }

    extern "C" {
        fn CFStringCreateWithCString(alloc: CFTypeRef, cstr: *const u8, encoding: u32) -> CFTypeRef;
        fn CFStringGetCString(s: CFTypeRef, buffer: *mut u8, size: c_long, encoding: u32) -> u8;
        fn CFURLCreateResourcePropertyForKeyFromBookmarkData(
            alloc: CFTypeRef,
            key: CFTypeRef,
            bookmark: CFTypeRef,
        ) -> CFTypeRef;
        fn CFGetTypeID(cf: CFTypeRef) -> c_ulong;
        fn CFStringGetTypeID() -> c_ulong;
    }

    const WITHOUT_UI: c_ulong = 1 << 8;
    const WITHOUT_MOUNTING: c_ulong = 1 << 9;
    const UTF8: u32 = 0x0800_0100;
    /// `kCFURLPathKey`: the path stored inside a bookmark when it was made.
    const PATH_KEY: &[u8] = b"_NSURLPathKey\0";

    unsafe fn recorded_path(data: CFTypeRef) -> Option<PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let key = Owned(CFStringCreateWithCString(std::ptr::null(), PATH_KEY.as_ptr(), UTF8));
        if key.0.is_null() {
            return None;
        }
        let value = Owned(CFURLCreateResourcePropertyForKeyFromBookmarkData(
            std::ptr::null(),
            key.0,
            data,
        ));
        if value.0.is_null() || CFGetTypeID(value.0) != CFStringGetTypeID() {
            return None;
        }
        let mut buf = vec![0u8; 4096];
        if CFStringGetCString(value.0, buf.as_mut_ptr(), buf.len() as c_long, UTF8) == 0 {
            return None;
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        (end > 0).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..end])))
    }

    struct Owned(CFTypeRef);
    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) }
            }
        }
    }

    pub fn create(path: &Path) -> Result<Vec<u8>> {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        unsafe {
            let url = Owned(CFURLCreateFromFileSystemRepresentation(
                std::ptr::null(),
                bytes.as_ptr(),
                bytes.len() as c_long,
                1,
            ));
            if url.0.is_null() {
                return Err(Error::Backend(format!("not a path: {}", path.display())));
            }
            let mut err: CFTypeRef = std::ptr::null();
            let data = Owned(CFURLCreateBookmarkData(
                std::ptr::null(),
                url.0,
                0,
                std::ptr::null(),
                std::ptr::null(),
                &mut err,
            ));
            if data.0.is_null() {
                if !err.is_null() {
                    CFRelease(err);
                }
                return Err(Error::Backend(format!(
                    "cannot make a bookmark for {}",
                    path.display()
                )));
            }
            let len = CFDataGetLength(data.0) as usize;
            let ptr = CFDataGetBytePtr(data.0);
            Ok(std::slice::from_raw_parts(ptr, len).to_vec())
        }
    }

    /// The path a bookmark points at, without mounting anything or asking.
    ///
    /// The path recorded when the bookmark was made comes first: it is there
    /// whether or not the volume is mounted right now, and a drive is taken
    /// out of the sidebar precisely when it is *not* mounted. Resolving is the
    /// fallback for a bookmark made by something that left the path out.
    pub fn resolve(bookmark: &[u8]) -> Option<PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        unsafe {
            let data = Owned(CFDataCreate(
                std::ptr::null(),
                bookmark.as_ptr(),
                bookmark.len() as c_long,
            ));
            if data.0.is_null() {
                return None;
            }
            if let Some(p) = recorded_path(data.0) {
                return Some(p);
            }
            let mut stale: u8 = 0;
            let mut err: CFTypeRef = std::ptr::null();
            let url = Owned(CFURLCreateByResolvingBookmarkData(
                std::ptr::null(),
                data.0,
                WITHOUT_UI | WITHOUT_MOUNTING,
                std::ptr::null(),
                std::ptr::null(),
                &mut stale,
                &mut err,
            ));
            if !err.is_null() {
                CFRelease(err);
            }
            if url.0.is_null() {
                return None;
            }
            let mut buf = vec![0u8; 4096];
            if CFURLGetFileSystemRepresentation(url.0, 1, buf.as_mut_ptr(), buf.len() as c_long) == 0 {
                return None;
            }
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..end])))
        }
    }
}

/// The `NSKeyedArchiver` shape of an `.sfl3` file, as far as these lists go.
///
/// The file is a binary plist with one `$objects` table; every container
/// refers to its members by index (`CF$UID`). Reading flattens that into a
/// tree, writing lays the tree out again.
#[cfg(target_os = "macos")]
mod archive {
    use std::collections::BTreeMap;

    use plist::{Dictionary, Uid, Value};

    use crate::error::{Error, Result};

    #[derive(Debug, Clone, PartialEq)]
    pub enum Node {
        Null,
        Str(String),
        Int(i64),
        Real(f64),
        Bool(bool),
        Data(Vec<u8>),
        /// Seconds since 2001-01-01, as `NSDate` stores it.
        Date(f64),
        Dict { mutable: bool, entries: Vec<(String, Node)> },
        Array { mutable: bool, items: Vec<Node> },
    }

    /// A list file: a root dictionary whose `items` array holds the rows.
    pub struct List {
        root: Node,
    }

    impl List {
        pub fn parse(bytes: &[u8]) -> Result<List> {
            let top = Value::from_reader(std::io::Cursor::new(bytes))
                .map_err(|e| Error::Backend(format!("not a plist: {e}")))?;
            let top = top
                .into_dictionary()
                .ok_or_else(|| Error::Backend("archive root is not a dictionary".into()))?;
            let objects = top
                .get("$objects")
                .and_then(Value::as_array)
                .ok_or_else(|| Error::Backend("archive has no $objects".into()))?;
            let root_uid = top
                .get("$top")
                .and_then(Value::as_dictionary)
                .and_then(|t| t.get("root"))
                .and_then(Value::as_uid)
                .ok_or_else(|| Error::Backend("archive has no root".into()))?;
            let root = decode(objects, root_uid.get(), 0)?;
            if !matches!(root, Node::Dict { .. }) {
                return Err(Error::Backend("archive root is not a dictionary".into()));
            }
            Ok(List { root })
        }

        pub fn items(&self) -> impl Iterator<Item = &Node> {
            self.items_node()
                .into_iter()
                .flat_map(|items| match items {
                    Node::Array { items, .. } => items.iter(),
                    _ => [].iter(),
                })
        }

        fn items_node(&self) -> Option<&Node> {
            match &self.root {
                Node::Dict { entries, .. } => {
                    entries.iter().find(|(k, _)| k == "items").map(|(_, v)| v)
                }
                _ => None,
            }
        }

        fn items_mut(&mut self) -> &mut Vec<Node> {
            let Node::Dict { entries, .. } = &mut self.root else {
                unreachable!("checked in parse");
            };
            if !entries.iter().any(|(k, _)| k == "items") {
                entries.push((
                    "items".into(),
                    Node::Array {
                        mutable: false,
                        items: vec![],
                    },
                ));
            }
            let (_, node) = entries.iter_mut().find(|(k, _)| k == "items").unwrap();
            if !matches!(node, Node::Array { .. }) {
                *node = Node::Array {
                    mutable: false,
                    items: vec![],
                };
            }
            match node {
                Node::Array { items, .. } => items,
                _ => unreachable!(),
            }
        }

        pub fn push_item(&mut self, item: Node) {
            self.items_mut().push(item);
        }

        pub fn retain_items(&mut self, keep: impl FnMut(&Node) -> bool) {
            self.items_mut().retain(keep);
        }

        pub fn to_bytes(&self) -> Result<Vec<u8>> {
            let mut enc = Encoder::default();
            enc.objects.push(Value::String("$null".into()));
            let root = enc.encode(&self.root);
            let mut top = Dictionary::new();
            top.insert("$archiver".into(), Value::String("NSKeyedArchiver".into()));
            top.insert("$version".into(), Value::Integer(100_000.into()));
            let mut t = Dictionary::new();
            t.insert("root".into(), Value::Uid(root));
            top.insert("$top".into(), Value::Dictionary(t));
            top.insert("$objects".into(), Value::Array(enc.objects));
            let mut out = Vec::new();
            Value::Dictionary(top)
                .to_writer_binary(&mut out)
                .map_err(|e| Error::Backend(format!("encode archive: {e}")))?;
            Ok(out)
        }
    }

    fn decode(objects: &[Value], uid: u64, depth: usize) -> Result<Node> {
        if depth > 64 {
            return Err(Error::Backend("archive nests too deep".into()));
        }
        let value = objects
            .get(uid as usize)
            .ok_or_else(|| Error::Backend(format!("dangling object {uid}")))?;
        Ok(match value {
            Value::String(s) if s == "$null" && uid == 0 => Node::Null,
            Value::String(s) => Node::Str(s.clone()),
            Value::Integer(i) => Node::Int(i.as_signed().unwrap_or_default()),
            Value::Real(r) => Node::Real(*r),
            Value::Boolean(b) => Node::Bool(*b),
            Value::Data(d) => Node::Data(d.clone()),
            Value::Date(d) => {
                let t = std::time::SystemTime::from(*d);
                Node::Date(cocoa_seconds(t))
            }
            Value::Dictionary(d) => {
                let class = d
                    .get("$class")
                    .and_then(Value::as_uid)
                    .and_then(|c| objects.get(c.get() as usize))
                    .and_then(Value::as_dictionary)
                    .and_then(|c| c.get("$classname"))
                    .and_then(Value::as_string)
                    .unwrap_or("");
                if let Some(t) = d.get("NS.time").and_then(Value::as_real) {
                    Node::Date(t)
                } else if let Some(keys) = d.get("NS.keys").and_then(Value::as_array) {
                    let vals = d
                        .get("NS.objects")
                        .and_then(Value::as_array)
                        .ok_or_else(|| Error::Backend("dictionary without values".into()))?;
                    let mut entries = Vec::with_capacity(keys.len());
                    for (k, v) in keys.iter().zip(vals) {
                        let key = match decode(objects, uid_of(k)?, depth + 1)? {
                            Node::Str(s) => s,
                            other => format!("{other:?}"),
                        };
                        entries.push((key, decode(objects, uid_of(v)?, depth + 1)?));
                    }
                    Node::Dict {
                        mutable: class.starts_with("NSMutable"),
                        entries,
                    }
                } else if let Some(vals) = d.get("NS.objects").and_then(Value::as_array) {
                    let mut items = Vec::with_capacity(vals.len());
                    for v in vals {
                        items.push(decode(objects, uid_of(v)?, depth + 1)?);
                    }
                    Node::Array {
                        mutable: class.starts_with("NSMutable"),
                        items,
                    }
                } else {
                    return Err(Error::Backend(format!(
                        "unsupported archived class '{class}'"
                    )));
                }
            }
            other => {
                return Err(Error::Backend(format!(
                    "unsupported archive value: {other:?}"
                )))
            }
        })
    }

    fn uid_of(v: &Value) -> Result<u64> {
        v.as_uid()
            .map(|u| u.get())
            .ok_or_else(|| Error::Backend("container member is not a reference".into()))
    }

    fn cocoa_seconds(t: std::time::SystemTime) -> f64 {
        const EPOCH_2001: f64 = 978_307_200.0;
        match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs_f64() - EPOCH_2001,
            Err(e) => -(e.duration().as_secs_f64()) - EPOCH_2001,
        }
    }

    #[derive(Default)]
    struct Encoder {
        objects: Vec<Value>,
        classes: BTreeMap<&'static str, Uid>,
    }

    impl Encoder {
        fn class(&mut self, name: &'static str) -> Uid {
            if let Some(u) = self.classes.get(name) {
                return *u;
            }
            let chain: &[&str] = match name {
                "NSMutableDictionary" => &["NSMutableDictionary", "NSDictionary", "NSObject"],
                "NSDictionary" => &["NSDictionary", "NSObject"],
                "NSMutableArray" => &["NSMutableArray", "NSArray", "NSObject"],
                "NSArray" => &["NSArray", "NSObject"],
                _ => &["NSDate", "NSObject"],
            };
            let mut d = Dictionary::new();
            d.insert("$classname".into(), Value::String(name.into()));
            d.insert(
                "$classes".into(),
                Value::Array(chain.iter().map(|c| Value::String((*c).into())).collect()),
            );
            let uid = self.push(Value::Dictionary(d));
            self.classes.insert(name, uid);
            uid
        }

        fn push(&mut self, v: Value) -> Uid {
            self.objects.push(v);
            Uid::new((self.objects.len() - 1) as u64)
        }

        fn encode(&mut self, node: &Node) -> Uid {
            match node {
                Node::Null => Uid::new(0),
                Node::Str(s) => self.push(Value::String(s.clone())),
                Node::Int(i) => self.push(Value::Integer((*i).into())),
                Node::Real(r) => self.push(Value::Real(*r)),
                Node::Bool(b) => self.push(Value::Boolean(*b)),
                Node::Data(d) => self.push(Value::Data(d.clone())),
                Node::Date(t) => {
                    let slot = self.push(Value::Dictionary(Dictionary::new()));
                    let class = self.class("NSDate");
                    let mut d = Dictionary::new();
                    d.insert("NS.time".into(), Value::Real(*t));
                    d.insert("$class".into(), Value::Uid(class));
                    self.objects[slot.get() as usize] = Value::Dictionary(d);
                    slot
                }
                Node::Dict { mutable, entries } => {
                    let slot = self.push(Value::Dictionary(Dictionary::new()));
                    let keys: Vec<Value> = entries
                        .iter()
                        .map(|(k, _)| Value::Uid(self.push(Value::String(k.clone()))))
                        .collect();
                    let vals: Vec<Value> = entries
                        .iter()
                        .map(|(_, v)| Value::Uid(self.encode(v)))
                        .collect();
                    let class = self.class(if *mutable {
                        "NSMutableDictionary"
                    } else {
                        "NSDictionary"
                    });
                    let mut d = Dictionary::new();
                    d.insert("NS.keys".into(), Value::Array(keys));
                    d.insert("NS.objects".into(), Value::Array(vals));
                    d.insert("$class".into(), Value::Uid(class));
                    self.objects[slot.get() as usize] = Value::Dictionary(d);
                    slot
                }
                Node::Array { mutable, items } => {
                    let slot = self.push(Value::Dictionary(Dictionary::new()));
                    let vals: Vec<Value> = items
                        .iter()
                        .map(|v| Value::Uid(self.encode(v)))
                        .collect();
                    let class = self.class(if *mutable { "NSMutableArray" } else { "NSArray" });
                    let mut d = Dictionary::new();
                    d.insert("NS.objects".into(), Value::Array(vals));
                    d.insert("$class".into(), Value::Uid(class));
                    self.objects[slot.get() as usize] = Value::Dictionary(d);
                    slot
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn sample() -> List {
            List {
                root: Node::Dict {
                    mutable: true,
                    entries: vec![
                        (
                            "items".into(),
                            Node::Array {
                                mutable: false,
                                items: vec![Node::Dict {
                                    mutable: false,
                                    entries: vec![
                                        ("Bookmark".into(), Node::Data(vec![1, 2, 3])),
                                        ("uuid".into(), Node::Str("A-B".into())),
                                        ("visibility".into(), Node::Int(0)),
                                    ],
                                }],
                            },
                        ),
                        (
                            "properties".into(),
                            Node::Dict {
                                mutable: false,
                                entries: vec![
                                    ("com.apple.LSSharedFileList.DateLastSeen".into(), Node::Date(1.5)),
                                    ("flag".into(), Node::Bool(true)),
                                ],
                            },
                        ),
                    ],
                },
            }
        }

        #[test]
        fn a_list_survives_a_round_trip_through_the_archive() {
            let list = sample();
            let bytes = list.to_bytes().unwrap();
            let back = List::parse(&bytes).unwrap();
            assert_eq!(back.root, list.root);
            assert_eq!(back.items().count(), 1);
        }

        #[test]
        fn items_can_be_added_and_taken_out() {
            let mut list = sample();
            list.push_item(Node::Dict {
                mutable: false,
                entries: vec![("uuid".into(), Node::Str("X".into()))],
            });
            assert_eq!(list.items().count(), 2);
            list.retain_items(|item| match item {
                Node::Dict { entries, .. } => {
                    !entries.iter().any(|(k, v)| k == "uuid" && *v == Node::Str("X".into()))
                }
                _ => true,
            });
            assert_eq!(list.items().count(), 1);
            // 다시 써도 깨지지 않습니다.
            List::parse(&list.to_bytes().unwrap()).unwrap();
        }
    }
}
