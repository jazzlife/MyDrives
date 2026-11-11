//! The filesystem semantics every cloud drive gets, checked against an
//! in-memory service that behaves the way the real ones do: ids, whole-file
//! transfers, listings by folder, and nothing else.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use mydrives_backend_cloud::{CloudApi, CloudBackend, CloudCaps, Item, ItemId};
use mydrives_core::backend::Backend;
use mydrives_core::error::{Error, Result};
use mydrives_core::path::RPath;
use mydrives_core::types::{FileKind, FsStat, OpenOpts};

#[derive(Clone)]
struct Node {
    parent: Option<ItemId>,
    name: String,
    kind: FileKind,
    data: Vec<u8>,
}

/// Counts how often each kind of transfer happened, so a test can say "this
/// cost one upload" rather than hoping.
#[derive(Default)]
struct Calls {
    lists: AtomicU64,
    downloads: AtomicU64,
    uploads: AtomicU64,
}

struct FakeCloud {
    nodes: Mutex<HashMap<ItemId, Node>>,
    next: AtomicU64,
    calls: Arc<Calls>,
    range_reads: bool,
}

impl FakeCloud {
    fn new(range_reads: bool) -> (FakeCloud, Arc<Calls>) {
        let calls = Arc::new(Calls::default());
        let mut nodes = HashMap::new();
        nodes.insert(
            "root".to_string(),
            Node {
                parent: None,
                name: "/".into(),
                kind: FileKind::Dir,
                data: vec![],
            },
        );
        (
            FakeCloud {
                nodes: Mutex::new(nodes),
                next: AtomicU64::new(1),
                calls: calls.clone(),
                range_reads,
            },
            calls,
        )
    }

    fn fresh_id(&self) -> ItemId {
        format!("id{}", self.next.fetch_add(1, Ordering::Relaxed))
    }

    fn item(id: &str, n: &Node) -> Item {
        Item {
            id: id.to_string(),
            name: n.name.clone(),
            kind: n.kind,
            size: n.data.len() as u64,
            created: SystemTime::UNIX_EPOCH,
            modified: SystemTime::UNIX_EPOCH,
            hidden: false,
        }
    }

    /// Puts a folder tree in place without going through the API counters.
    fn seed(&self, parent: &str, name: &str, data: Option<&[u8]>) -> ItemId {
        let id = self.fresh_id();
        self.nodes.lock().insert(
            id.clone(),
            Node {
                parent: Some(parent.to_string()),
                name: name.into(),
                kind: if data.is_some() { FileKind::File } else { FileKind::Dir },
                data: data.map(|d| d.to_vec()).unwrap_or_default(),
            },
        );
        id
    }
}

#[async_trait]
impl CloudApi for FakeCloud {
    fn describe(&self) -> String {
        "fake://".into()
    }
    fn caps(&self) -> CloudCaps {
        CloudCaps {
            range_reads: self.range_reads,
            case_sensitive: true,
        }
    }
    fn root_id(&self) -> ItemId {
        "root".into()
    }
    async fn connect(&self) -> Result<()> {
        Ok(())
    }
    async fn list(&self, folder: &str) -> Result<Vec<Item>> {
        self.calls.lists.fetch_add(1, Ordering::Relaxed);
        let nodes = self.nodes.lock();
        if !nodes.contains_key(folder) {
            return Err(Error::NotFound(folder.into()));
        }
        Ok(nodes
            .iter()
            .filter(|(_, n)| n.parent.as_deref() == Some(folder))
            .map(|(id, n)| Self::item(id, n))
            .collect())
    }
    async fn mkdir(&self, parent: &str, name: &str) -> Result<Item> {
        let id = self.seed(parent, name, None);
        let nodes = self.nodes.lock();
        Ok(Self::item(&id, &nodes[&id]))
    }
    async fn delete(&self, id: &str) -> Result<()> {
        self.nodes
            .lock()
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| Error::NotFound(id.into()))
    }
    async fn rename(&self, id: &str, name: &str) -> Result<()> {
        let mut nodes = self.nodes.lock();
        let n = nodes.get_mut(id).ok_or_else(|| Error::NotFound(id.into()))?;
        n.name = name.into();
        Ok(())
    }
    async fn move_to(&self, id: &str, parent: &str) -> Result<()> {
        let mut nodes = self.nodes.lock();
        let n = nodes.get_mut(id).ok_or_else(|| Error::NotFound(id.into()))?;
        n.parent = Some(parent.into());
        Ok(())
    }
    async fn download(&self, id: &str, into: &mut tokio::fs::File) -> Result<u64> {
        self.calls.downloads.fetch_add(1, Ordering::Relaxed);
        let data = self
            .nodes
            .lock()
            .get(id)
            .map(|n| n.data.clone())
            .ok_or_else(|| Error::NotFound(id.into()))?;
        into.write_all(&data).await.map_err(Error::Io)?;
        Ok(data.len() as u64)
    }
    async fn download_range(&self, id: &str, offset: u64, len: u32) -> Result<Vec<u8>> {
        self.calls.downloads.fetch_add(1, Ordering::Relaxed);
        let nodes = self.nodes.lock();
        let n = nodes.get(id).ok_or_else(|| Error::NotFound(id.into()))?;
        let start = (offset as usize).min(n.data.len());
        let end = (start + len as usize).min(n.data.len());
        Ok(n.data[start..end].to_vec())
    }
    async fn upload(
        &self,
        parent: &str,
        name: &str,
        mut from: tokio::fs::File,
        len: u64,
        _modified: Option<SystemTime>,
    ) -> Result<()> {
        self.calls.uploads.fetch_add(1, Ordering::Relaxed);
        let mut data = Vec::new();
        from.read_to_end(&mut data).await.map_err(Error::Io)?;
        assert_eq!(data.len() as u64, len, "the declared size matches the bytes sent");
        let mut nodes = self.nodes.lock();
        // Like the real services: an upload under an existing name replaces
        // it, and the replacement gets a new id.
        let existing: Vec<ItemId> = nodes
            .iter()
            .filter(|(_, n)| n.parent.as_deref() == Some(parent) && n.name == name)
            .map(|(id, _)| id.clone())
            .collect();
        for id in existing {
            nodes.remove(&id);
        }
        let id = format!("id{}", self.next.fetch_add(1, Ordering::Relaxed));
        nodes.insert(
            id,
            Node {
                parent: Some(parent.into()),
                name: name.into(),
                kind: FileKind::File,
                data,
            },
        );
        Ok(())
    }
    async fn quota(&self) -> Result<FsStat> {
        Ok(FsStat {
            total_bytes: 1 << 40,
            free_bytes: 1 << 39,
            block_size: 4096,
            total_files: 0,
            free_files: 0,
        })
    }
}

fn drive(range_reads: bool) -> (Arc<CloudBackend<FakeCloud>>, Arc<Calls>) {
    let (cloud, calls) = FakeCloud::new(range_reads);
    (Arc::new(CloudBackend::new("test", cloud, None)), calls)
}

async fn write_file(b: &CloudBackend<FakeCloud>, path: &str, data: &[u8]) {
    let f = b.create(&RPath::new(path)).await.unwrap();
    b.write_at(f, 0, data).await.unwrap();
    b.close(f).await.unwrap();
}

async fn read_all(b: &CloudBackend<FakeCloud>, path: &str) -> Vec<u8> {
    let f = b
        .open(
            &RPath::new(path),
            OpenOpts {
                read: true,
                ..OpenOpts::default()
            },
        )
        .await
        .unwrap();
    let size = b.stat(&RPath::new(path)).await.unwrap().size;
    let mut out = Vec::new();
    let mut at = 0u64;
    while at < size {
        let chunk = b.read_at(f, at, 7).await.unwrap();
        assert!(!chunk.is_empty(), "a read inside the file returns bytes");
        at += chunk.len() as u64;
        out.extend(chunk);
    }
    b.close(f).await.unwrap();
    out
}

#[tokio::test]
async fn a_file_written_comes_back_and_cost_one_upload() {
    let (b, calls) = drive(false);
    b.connect().await.unwrap();
    write_file(&b, "/notes.txt", b"hello cloud").await;
    assert_eq!(calls.uploads.load(Ordering::Relaxed), 1);
    assert_eq!(read_all(&b, "/notes.txt").await, b"hello cloud");
    // Without range reads the whole file came down once for that read.
    assert_eq!(calls.downloads.load(Ordering::Relaxed), 1);
    assert_eq!(b.stat(&RPath::new("/notes.txt")).await.unwrap().size, 11);
}

#[tokio::test]
async fn writes_out_of_order_and_in_pieces_make_one_file() {
    let (b, calls) = drive(false);
    b.connect().await.unwrap();
    let f = b.create(&RPath::new("/parts.bin")).await.unwrap();
    // Second half first, then the first half: what the kernel does.
    b.write_at(f, 6, b"world!").await.unwrap();
    b.write_at(f, 0, b"hello ").await.unwrap();
    b.flush(f).await.unwrap();
    b.write_at(f, 12, b" again").await.unwrap();
    b.close(f).await.unwrap();
    assert_eq!(read_all(&b, "/parts.bin").await, b"hello world! again");
    // One upload per flush that had something new.
    assert_eq!(calls.uploads.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn editing_the_middle_keeps_the_rest() {
    let (b, _) = drive(false);
    b.connect().await.unwrap();
    write_file(&b, "/a.txt", b"0123456789").await;
    let f = b
        .open(
            &RPath::new("/a.txt"),
            OpenOpts {
                read: true,
                write: true,
                ..OpenOpts::default()
            },
        )
        .await
        .unwrap();
    b.write_at(f, 3, b"XY").await.unwrap();
    // The handle sees its own write before the flush.
    assert_eq!(b.read_at(f, 0, 10).await.unwrap(), b"012XY56789");
    b.close(f).await.unwrap();
    assert_eq!(read_all(&b, "/a.txt").await, b"012XY56789");
}

#[tokio::test]
async fn a_service_with_range_reads_is_read_without_a_local_copy() {
    let (b, calls) = drive(true);
    b.connect().await.unwrap();
    write_file(&b, "/r.txt", b"abcdefghijklmnopqrstuvwxyz").await;
    assert_eq!(read_all(&b, "/r.txt").await, b"abcdefghijklmnopqrstuvwxyz");
    // 26 bytes in reads of 7: four range downloads, no whole-file fetch.
    assert_eq!(calls.downloads.load(Ordering::Relaxed), 4);
}

#[tokio::test]
async fn folders_list_what_is_in_them_and_paths_resolve_through_ids() {
    let (b, calls) = drive(false);
    let docs = b.api().seed("root", "Docs", None);
    b.api().seed(&docs, "plan.md", Some(b"# plan"));
    b.api().seed(&docs, "2026", None);
    b.connect().await.unwrap();

    let root: Vec<String> = b
        .readdir(&RPath::root())
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(root, vec!["Docs"]);
    let mut inside: Vec<String> = b
        .readdir(&RPath::new("/Docs"))
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    inside.sort();
    assert_eq!(inside, vec!["2026", "plan.md"]);
    assert_eq!(read_all(&b, "/Docs/plan.md").await, b"# plan");
    assert!(b.stat(&RPath::new("/Docs/2026")).await.unwrap().is_dir());
    // Two folders were listed, each once; the deep stat reused them.
    assert_eq!(calls.lists.load(Ordering::Relaxed), 2);
    assert!(matches!(
        b.stat(&RPath::new("/Docs/nope")).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn create_refuses_a_taken_name_and_touch_still_makes_a_file() {
    let (b, calls) = drive(false);
    b.connect().await.unwrap();
    let f = b.create(&RPath::new("/empty.txt")).await.unwrap();
    b.close(f).await.unwrap();
    assert_eq!(calls.uploads.load(Ordering::Relaxed), 1, "touch uploads once");
    assert_eq!(b.stat(&RPath::new("/empty.txt")).await.unwrap().size, 0);
    assert!(matches!(
        b.create(&RPath::new("/empty.txt")).await,
        Err(Error::AlreadyExists(_))
    ));
}

#[tokio::test]
async fn rename_moves_across_folders_and_refuses_to_clobber_unless_told() {
    let (b, _) = drive(false);
    b.connect().await.unwrap();
    b.mkdir(&RPath::new("/dst")).await.unwrap();
    write_file(&b, "/one.txt", b"1").await;
    write_file(&b, "/dst/two.txt", b"2").await;

    b.rename(&RPath::new("/one.txt"), &RPath::new("/dst/moved.txt"), false)
        .await
        .unwrap();
    assert!(matches!(
        b.stat(&RPath::new("/one.txt")).await,
        Err(Error::NotFound(_))
    ));
    assert_eq!(read_all(&b, "/dst/moved.txt").await, b"1");

    assert!(matches!(
        b.rename(&RPath::new("/dst/moved.txt"), &RPath::new("/dst/two.txt"), false)
            .await,
        Err(Error::AlreadyExists(_))
    ));
    b.rename(&RPath::new("/dst/moved.txt"), &RPath::new("/dst/two.txt"), true)
        .await
        .unwrap();
    assert_eq!(read_all(&b, "/dst/two.txt").await, b"1");
    let names: Vec<String> = b
        .readdir(&RPath::new("/dst"))
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, vec!["two.txt"]);
}

#[tokio::test]
async fn delete_and_rmdir_behave_like_a_filesystem() {
    let (b, _) = drive(false);
    b.connect().await.unwrap();
    b.mkdir(&RPath::new("/d")).await.unwrap();
    write_file(&b, "/d/f.txt", b"x").await;
    assert!(matches!(
        b.rmdir(&RPath::new("/d")).await,
        Err(Error::NotEmpty(_))
    ));
    assert!(matches!(
        b.unlink(&RPath::new("/d")).await,
        Err(Error::IsADirectory(_))
    ));
    b.unlink(&RPath::new("/d/f.txt")).await.unwrap();
    b.rmdir(&RPath::new("/d")).await.unwrap();
    assert!(b.readdir(&RPath::root()).await.unwrap().is_empty());
}

#[tokio::test]
async fn truncating_shortens_and_zero_empties() {
    let (b, _) = drive(false);
    b.connect().await.unwrap();
    write_file(&b, "/t.txt", b"0123456789").await;
    b.set_len(&RPath::new("/t.txt"), None, 4).await.unwrap();
    assert_eq!(read_all(&b, "/t.txt").await, b"0123");
    b.set_len(&RPath::new("/t.txt"), None, 0).await.unwrap();
    assert_eq!(b.stat(&RPath::new("/t.txt")).await.unwrap().size, 0);
}

#[tokio::test]
async fn opening_with_truncate_does_not_download_first() {
    let (b, calls) = drive(false);
    b.connect().await.unwrap();
    write_file(&b, "/big.bin", &vec![7u8; 5000]).await;
    let before = calls.downloads.load(Ordering::Relaxed);
    let f = b
        .open(
            &RPath::new("/big.bin"),
            OpenOpts {
                write: true,
                truncate: true,
                ..OpenOpts::default()
            },
        )
        .await
        .unwrap();
    b.write_at(f, 0, b"new").await.unwrap();
    b.close(f).await.unwrap();
    assert_eq!(calls.downloads.load(Ordering::Relaxed), before);
    assert_eq!(read_all(&b, "/big.bin").await, b"new");
}

#[tokio::test]
async fn a_subpath_is_the_top_the_mount_sees() {
    let (cloud, _) = FakeCloud::new(false);
    let work = cloud.seed("root", "Work", None);
    let inner = cloud.seed(&work, "2026", None);
    cloud.seed(&inner, "todo.txt", Some(b"ship it"));
    cloud.seed("root", "Private", None);
    let b = CloudBackend::new("sub", cloud, Some("/Work/2026"));
    b.connect().await.unwrap();
    let names: Vec<String> = b
        .readdir(&RPath::root())
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, vec!["todo.txt"]);
    assert_eq!(read_all(&b, "/todo.txt").await, b"ship it");
    assert!(matches!(
        b.stat(&RPath::new("/Private")).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn a_missing_subpath_fails_at_connect_with_a_clear_message() {
    let (cloud, _) = FakeCloud::new(false);
    let b = CloudBackend::new("sub", cloud, Some("/Nope"));
    match b.connect().await {
        Err(Error::NotFound(msg)) => assert!(msg.contains("Nope"), "{msg}"),
        other => panic!("expected not found, got {other:?}"),
    }
}
