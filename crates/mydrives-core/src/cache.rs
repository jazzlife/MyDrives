//! Metadata caching.
//!
//! A network round trip is ~100x a local stat, and Explorer/Finder issue stat
//! storms (every window refresh, every icon, every `.DS_Store` probe). Without a
//! short-lived metadata cache a mounted share feels broken; with too long a TTL
//! the user sees stale sizes. Default: 2 s for attributes, 2 s for listings, and
//! an explicit invalidation on every mutation we perform ourselves.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::path::RPath;
use crate::types::{Attr, DirEntry};

pub struct MetaCache {
    attrs: Mutex<HashMap<String, (Attr, Instant)>>,
    dirs: Mutex<HashMap<String, (Arc<Vec<DirEntry>>, Instant)>>,
    /// Names the server said do not exist. Finder probes several of these in
    /// every folder it opens (`.DS_Store`, `._*`, `.localized`, `Icon\r`), and
    /// each probe answered by the server costs a full round trip set.
    absent: Mutex<HashMap<String, Instant>>,
    attr_ttl: Duration,
    dir_ttl: Duration,
}

impl MetaCache {
    pub fn new(attr_ttl: Duration, dir_ttl: Duration) -> Self {
        MetaCache {
            attrs: Mutex::new(HashMap::new()),
            dirs: Mutex::new(HashMap::new()),
            absent: Mutex::new(HashMap::new()),
            attr_ttl,
            dir_ttl,
        }
    }

    /// Was `path` recently reported missing? Shares the attribute TTL: a file
    /// that appears from elsewhere shows up within the same delay as a changed
    /// size would.
    pub fn is_absent(&self, path: &RPath) -> bool {
        let map = self.absent.lock();
        matches!(map.get(path.as_str()), Some(at) if at.elapsed() <= self.attr_ttl)
    }

    pub fn put_absent(&self, path: &RPath) {
        self.absent
            .lock()
            .insert(path.as_str().to_string(), Instant::now());
    }

    pub fn attr(&self, path: &RPath) -> Option<Attr> {
        let map = self.attrs.lock();
        let (attr, at) = map.get(path.as_str())?;
        if at.elapsed() <= self.attr_ttl {
            Some(*attr)
        } else {
            None
        }
    }

    pub fn put_attr(&self, path: &RPath, attr: Attr) {
        self.attrs
            .lock()
            .insert(path.as_str().to_string(), (attr, Instant::now()));
    }

    pub fn dir(&self, path: &RPath) -> Option<Arc<Vec<DirEntry>>> {
        let map = self.dirs.lock();
        let (entries, at) = map.get(path.as_str())?;
        if at.elapsed() <= self.dir_ttl {
            Some(entries.clone())
        } else {
            None
        }
    }

    /// Caches a listing and - the point of doing it - every child's attributes, so
    /// that the `ls -l` / Explorer "details view" pattern costs one round trip
    /// instead of one per file.
    pub fn put_dir(&self, path: &RPath, entries: Arc<Vec<DirEntry>>) {
        {
            let now = Instant::now();
            let mut attrs = self.attrs.lock();
            let mut absent = self.absent.lock();
            for e in entries.iter() {
                let child = path.join(&e.name).into_string();
                // A name the listing shows is not absent, whatever was said
                // before.
                absent.remove(&child);
                attrs.insert(child, (e.attr, now));
            }
        }
        self.dirs
            .lock()
            .insert(path.as_str().to_string(), (entries, Instant::now()));
    }

    /// Drops everything we know about `path`, plus its parent's listing (the file
    /// may have appeared, grown or vanished).
    pub fn invalidate(&self, path: &RPath) {
        self.attrs.lock().remove(path.as_str());
        self.absent.lock().remove(path.as_str());
        self.dirs.lock().remove(path.as_str());
        if let Some(parent) = path.parent() {
            self.dirs.lock().remove(parent.as_str());
        }
    }

    /// Drops a whole subtree - used after a directory rename or an unmount.
    pub fn invalidate_tree(&self, path: &RPath) {
        self.invalidate(path);
        self.attrs
            .lock()
            .retain(|k, _| !path.is_ancestor_of(&RPath::new(k)));
        self.absent
            .lock()
            .retain(|k, _| !path.is_ancestor_of(&RPath::new(k)));
        self.dirs
            .lock()
            .retain(|k, _| !path.is_ancestor_of(&RPath::new(k)));
    }

    pub fn clear(&self) {
        self.attrs.lock().clear();
        self.absent.lock().clear();
        self.dirs.lock().clear();
    }

    pub fn len(&self) -> (usize, usize) {
        (self.attrs.lock().len(), self.dirs.lock().len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FileKind;

    #[test]
    fn dir_listing_seeds_child_attrs() {
        let c = MetaCache::new(Duration::from_secs(5), Duration::from_secs(5));
        let dir = RPath::new("/a");
        c.put_dir(
            &dir,
            Arc::new(vec![DirEntry {
                name: "f.txt".into(),
                attr: Attr::file(7),
            }]),
        );
        let got = c.attr(&RPath::new("/a/f.txt")).expect("seeded");
        assert_eq!(got.size, 7);
        assert_eq!(got.kind, FileKind::File);
    }

    #[test]
    fn invalidate_drops_parent_listing() {
        let c = MetaCache::new(Duration::from_secs(5), Duration::from_secs(5));
        c.put_dir(&RPath::new("/a"), Arc::new(vec![]));
        c.put_attr(&RPath::new("/a/f"), Attr::file(1));
        c.invalidate(&RPath::new("/a/f"));
        assert!(c.attr(&RPath::new("/a/f")).is_none());
        assert!(c.dir(&RPath::new("/a")).is_none());
    }

    #[test]
    fn ttl_expires() {
        let c = MetaCache::new(Duration::from_millis(0), Duration::from_millis(0));
        c.put_attr(&RPath::new("/x"), Attr::file(1));
        std::thread::sleep(Duration::from_millis(5));
        assert!(c.attr(&RPath::new("/x")).is_none());
    }

    #[test]
    fn invalidate_tree_removes_descendants() {
        let c = MetaCache::new(Duration::from_secs(5), Duration::from_secs(5));
        c.put_attr(&RPath::new("/a/b/c"), Attr::file(1));
        c.put_attr(&RPath::new("/a2"), Attr::file(1));
        c.invalidate_tree(&RPath::new("/a"));
        assert!(c.attr(&RPath::new("/a/b/c")).is_none());
        assert!(c.attr(&RPath::new("/a2")).is_some());
    }
}
