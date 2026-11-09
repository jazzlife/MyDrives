//! In-memory backend.
//!
//! Not a toy: it is the reference implementation of the [`Backend`] contract and
//! the only way to test mount-layer semantics (FUSE/NFS/WinFsp) without a server
//! in the loop. `latency` optionally simulates a WAN so cache behaviour can be
//! observed in tests.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::backend::{BFile, Backend};
use crate::error::{Error, Result};
use crate::path::RPath;
use crate::types::{Attr, Caps, DirEntry, FsStat, OpenOpts, SetAttr};

struct MemNode {
    attr: Attr,
    data: Vec<u8>,
}

pub struct MemBackend {
    id: String,
    nodes: Mutex<BTreeMap<String, MemNode>>,
    handles: Mutex<HashMap<u64, String>>,
    next_handle: AtomicU64,
    latency: Option<Duration>,
    readonly: bool,
}

impl MemBackend {
    pub fn new(id: impl Into<String>) -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            "/".to_string(),
            MemNode {
                attr: Attr::dir(),
                data: Vec::new(),
            },
        );
        MemBackend {
            id: id.into(),
            nodes: Mutex::new(nodes),
            handles: Mutex::new(HashMap::new()),
            next_handle: AtomicU64::new(1),
            latency: None,
            readonly: false,
        }
    }

    pub fn with_latency(mut self, latency: Duration) -> Self {
        self.latency = Some(latency);
        self
    }

    pub fn read_only(mut self) -> Self {
        self.readonly = true;
        self
    }

    /// Seeds a file, as a fixture would.
    pub fn put_file(&self, path: &str, contents: &[u8]) {
        let p = RPath::new(path);
        let mut nodes = self.nodes.lock();
        let mut cur = RPath::root();
        for c in p.parent().unwrap_or_else(RPath::root).components() {
            cur = cur.join(c);
            nodes.entry(cur.as_str().to_string()).or_insert_with(|| MemNode {
                attr: Attr::dir(),
                data: Vec::new(),
            });
        }
        nodes.insert(
            p.into_string(),
            MemNode {
                attr: Attr::file(contents.len() as u64),
                data: contents.to_vec(),
            },
        );
    }

    pub fn put_dir(&self, path: &str) {
        self.nodes.lock().insert(
            RPath::new(path).into_string(),
            MemNode {
                attr: Attr::dir(),
                data: Vec::new(),
            },
        );
    }

    pub fn file_count(&self) -> usize {
        self.nodes.lock().len()
    }

    async fn delay(&self) {
        if let Some(d) = self.latency {
            tokio::time::sleep(d).await;
        }
    }

    fn path_of(&self, f: BFile) -> Result<String> {
        self.handles
            .lock()
            .get(&f.0)
            .cloned()
            .ok_or_else(|| Error::InvalidArgument(format!("bad handle {}", f.0)))
    }

    fn require_parent(nodes: &BTreeMap<String, MemNode>, path: &RPath) -> Result<()> {
        let parent = match path.parent() {
            Some(p) => p,
            None => return Ok(()),
        };
        match nodes.get(parent.as_str()) {
            Some(n) if n.attr.is_dir() => Ok(()),
            Some(_) => Err(Error::NotADirectory(parent.to_string())),
            None => Err(Error::NotFound(parent.to_string())),
        }
    }
}

#[async_trait]
impl Backend for MemBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn describe(&self) -> String {
        format!("mem://{}", self.id)
    }

    fn caps(&self) -> Caps {
        Caps {
            write: !self.readonly,
            case_sensitive: true,
            ..Default::default()
        }
    }

    async fn connect(&self) -> Result<()> {
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        Ok(())
    }

    async fn stat(&self, path: &RPath) -> Result<Attr> {
        self.delay().await;
        self.nodes
            .lock()
            .get(path.as_str())
            .map(|n| n.attr)
            .ok_or_else(|| Error::NotFound(path.to_string()))
    }

    async fn readdir(&self, path: &RPath) -> Result<Vec<DirEntry>> {
        self.delay().await;
        let nodes = self.nodes.lock();
        match nodes.get(path.as_str()) {
            Some(n) if n.attr.is_dir() => {}
            Some(_) => return Err(Error::NotADirectory(path.to_string())),
            None => return Err(Error::NotFound(path.to_string())),
        }
        let prefix = if path.is_root() {
            "/".to_string()
        } else {
            format!("{}/", path.as_str())
        };
        let mut out = Vec::new();
        for (key, node) in nodes.iter() {
            if key == path.as_str() || !key.starts_with(&prefix) {
                continue;
            }
            let rest = &key[prefix.len()..];
            if rest.is_empty() || rest.contains('/') {
                continue;
            }
            out.push(DirEntry {
                name: rest.to_string(),
                attr: node.attr,
            });
        }
        Ok(out)
    }

    async fn open(&self, path: &RPath, opts: OpenOpts) -> Result<BFile> {
        self.delay().await;
        {
            let nodes = self.nodes.lock();
            match nodes.get(path.as_str()) {
                Some(n) if n.attr.is_dir() => return Err(Error::IsADirectory(path.to_string())),
                Some(_) => {}
                None => return Err(Error::NotFound(path.to_string())),
            }
        }
        if opts.write && self.readonly {
            return Err(Error::ReadOnly(path.to_string()));
        }
        let id = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().insert(id, path.as_str().to_string());
        Ok(BFile(id))
    }

    async fn create(&self, path: &RPath) -> Result<BFile> {
        self.delay().await;
        if self.readonly {
            return Err(Error::ReadOnly(path.to_string()));
        }
        {
            let mut nodes = self.nodes.lock();
            Self::require_parent(&nodes, path)?;
            if nodes.contains_key(path.as_str()) {
                return Err(Error::AlreadyExists(path.to_string()));
            }
            nodes.insert(
                path.as_str().to_string(),
                MemNode {
                    attr: Attr::file(0),
                    data: Vec::new(),
                },
            );
        }
        let id = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().insert(id, path.as_str().to_string());
        Ok(BFile(id))
    }

    async fn read_at(&self, f: BFile, offset: u64, len: u32) -> Result<Vec<u8>> {
        self.delay().await;
        let path = self.path_of(f)?;
        let nodes = self.nodes.lock();
        let node = nodes
            .get(&path)
            .ok_or_else(|| Error::NotFound(path.clone()))?;
        let start = offset.min(node.data.len() as u64) as usize;
        let end = (start + len as usize).min(node.data.len());
        Ok(node.data[start..end].to_vec())
    }

    async fn write_at(&self, f: BFile, offset: u64, data: &[u8]) -> Result<u32> {
        self.delay().await;
        if self.readonly {
            return Err(Error::ReadOnly(String::new()));
        }
        let path = self.path_of(f)?;
        let mut nodes = self.nodes.lock();
        let node = nodes
            .get_mut(&path)
            .ok_or_else(|| Error::NotFound(path.clone()))?;
        let end = offset as usize + data.len();
        if node.data.len() < end {
            node.data.resize(end, 0);
        }
        node.data[offset as usize..end].copy_from_slice(data);
        node.attr.size = node.data.len() as u64;
        node.attr.mtime = SystemTime::now();
        Ok(data.len() as u32)
    }

    async fn flush(&self, _f: BFile) -> Result<()> {
        Ok(())
    }

    async fn close(&self, f: BFile) -> Result<()> {
        self.handles.lock().remove(&f.0);
        Ok(())
    }

    async fn set_len(&self, path: &RPath, _f: Option<BFile>, len: u64) -> Result<()> {
        self.delay().await;
        let mut nodes = self.nodes.lock();
        let node = nodes
            .get_mut(path.as_str())
            .ok_or_else(|| Error::NotFound(path.to_string()))?;
        node.data.resize(len as usize, 0);
        node.attr.size = len;
        Ok(())
    }

    async fn set_attr(&self, path: &RPath, _f: Option<BFile>, set: &SetAttr) -> Result<()> {
        self.delay().await;
        let mut nodes = self.nodes.lock();
        let node = nodes
            .get_mut(path.as_str())
            .ok_or_else(|| Error::NotFound(path.to_string()))?;
        if let Some(t) = set.mtime {
            node.attr.mtime = t;
        }
        if let Some(t) = set.atime {
            node.attr.atime = t;
        }
        if let Some(ro) = set.readonly {
            node.attr.readonly = ro;
        }
        if let Some(size) = set.size {
            node.data.resize(size as usize, 0);
            node.attr.size = size;
        }
        Ok(())
    }

    async fn mkdir(&self, path: &RPath) -> Result<()> {
        self.delay().await;
        if self.readonly {
            return Err(Error::ReadOnly(path.to_string()));
        }
        let mut nodes = self.nodes.lock();
        Self::require_parent(&nodes, path)?;
        if nodes.contains_key(path.as_str()) {
            return Err(Error::AlreadyExists(path.to_string()));
        }
        nodes.insert(
            path.as_str().to_string(),
            MemNode {
                attr: Attr::dir(),
                data: Vec::new(),
            },
        );
        Ok(())
    }

    async fn unlink(&self, path: &RPath) -> Result<()> {
        self.delay().await;
        if self.readonly {
            return Err(Error::ReadOnly(path.to_string()));
        }
        let mut nodes = self.nodes.lock();
        match nodes.get(path.as_str()) {
            Some(n) if n.attr.is_dir() => return Err(Error::IsADirectory(path.to_string())),
            Some(_) => {}
            None => return Err(Error::NotFound(path.to_string())),
        }
        nodes.remove(path.as_str());
        Ok(())
    }

    async fn rmdir(&self, path: &RPath) -> Result<()> {
        self.delay().await;
        if self.readonly {
            return Err(Error::ReadOnly(path.to_string()));
        }
        let mut nodes = self.nodes.lock();
        match nodes.get(path.as_str()) {
            Some(n) if !n.attr.is_dir() => return Err(Error::NotADirectory(path.to_string())),
            Some(_) => {}
            None => return Err(Error::NotFound(path.to_string())),
        }
        let prefix = format!("{}/", path.as_str());
        if nodes.keys().any(|k| k.starts_with(&prefix)) {
            return Err(Error::NotEmpty(path.to_string()));
        }
        nodes.remove(path.as_str());
        Ok(())
    }

    async fn rename(&self, from: &RPath, to: &RPath, replace: bool) -> Result<()> {
        self.delay().await;
        if self.readonly {
            return Err(Error::ReadOnly(from.to_string()));
        }
        let mut nodes = self.nodes.lock();
        if !nodes.contains_key(from.as_str()) {
            return Err(Error::NotFound(from.to_string()));
        }
        Self::require_parent(&nodes, to)?;
        if nodes.contains_key(to.as_str()) {
            if !replace {
                return Err(Error::AlreadyExists(to.to_string()));
            }
            nodes.remove(to.as_str());
        }
        // Move the subtree, keeping paths consistent.
        let moved: Vec<String> = nodes
            .keys()
            .filter(|k| {
                let p = RPath::new(k);
                &p == from || from.is_ancestor_of(&p)
            })
            .cloned()
            .collect();
        for key in moved {
            let node = nodes.remove(&key).expect("just listed");
            let new_path = RPath::new(&key)
                .reparent(from, to)
                .expect("filtered to the subtree");
            nodes.insert(new_path.into_string(), node);
        }
        // Handles address paths, so follow them too.
        let mut handles = self.handles.lock();
        for path in handles.values_mut() {
            if let Some(p) = RPath::new(path).reparent(from, to) {
                *path = p.into_string();
            }
        }
        Ok(())
    }

    async fn statfs(&self) -> Result<FsStat> {
        let used: u64 = self.nodes.lock().values().map(|n| n.data.len() as u64).sum();
        Ok(FsStat {
            total_bytes: 1 << 30,
            free_bytes: (1 << 30) - used,
            block_size: 4096,
            total_files: 1 << 20,
            free_files: (1 << 20) - self.nodes.lock().len() as u64,
        })
    }
}

/// Convenience: the fixture tree used by several tests.
pub fn fixture(id: &str) -> MemBackend {
    let b = MemBackend::new(id);
    b.put_dir("/docs");
    b.put_file("/docs/readme.txt", b"hello mydrives\n");
    b.put_file("/docs/big.bin", &vec![7u8; 3 * 1024 * 1024]);
    b.put_dir("/empty");
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FileKind;

    #[tokio::test]
    async fn readdir_lists_only_direct_children() {
        let b = fixture("t");
        let entries = b.readdir(&RPath::root()).await.unwrap();
        let mut names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["docs", "empty"]);
        let docs = b.readdir(&RPath::new("/docs")).await.unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(
            docs.iter().find(|e| e.name == "readme.txt").unwrap().attr.kind,
            FileKind::File
        );
    }

    #[tokio::test]
    async fn rename_moves_subtree() {
        let b = fixture("t");
        b.rename(&RPath::new("/docs"), &RPath::new("/d2"), false)
            .await
            .unwrap();
        assert!(b.stat(&RPath::new("/d2/readme.txt")).await.is_ok());
        assert!(b.stat(&RPath::new("/docs/readme.txt")).await.is_err());
    }

    #[tokio::test]
    async fn rmdir_refuses_non_empty() {
        let b = fixture("t");
        assert!(matches!(
            b.rmdir(&RPath::new("/docs")).await,
            Err(Error::NotEmpty(_))
        ));
        b.rmdir(&RPath::new("/empty")).await.unwrap();
    }
}
