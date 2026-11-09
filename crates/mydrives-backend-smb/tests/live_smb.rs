//! Live SMB tests against a real server.
//!
//! Skipped unless `MYDRIVES_SMB_HOST` is set, so `cargo test` stays green on a
//! machine with no NAS. Point them at any SMB2+ server:
//!
//! ```sh
//! MYDRIVES_SMB_HOST=127.0.0.1 MYDRIVES_SMB_PORT=4455 \
//! MYDRIVES_SMB_SHARE=data MYDRIVES_SMB_USER=mdtest MYDRIVES_SMB_PASS=mdpass \
//!   cargo test -p mydrives-backend-smb --test live_smb -- --test-threads=1 --nocapture
//! ```
//!
//! Every test works inside its own directory under the share root and cleans up
//! after itself, so they can run against a real NAS without making a mess.

use std::sync::Arc;

use mydrives_backend_smb::{SmbBackend, SmbTarget};
use mydrives_core::types::{FileKind, OpenOpts, SetAttr};
use mydrives_core::{Backend, Error, RPath, Vfs, VfsOptions};

fn target() -> Option<SmbTarget> {
    let host = std::env::var("MYDRIVES_SMB_HOST").ok()?;
    Some(SmbTarget {
        host,
        port: std::env::var("MYDRIVES_SMB_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(445),
        share: std::env::var("MYDRIVES_SMB_SHARE").unwrap_or_else(|_| "data".into()),
        user: std::env::var("MYDRIVES_SMB_USER").unwrap_or_default(),
        password: std::env::var("MYDRIVES_SMB_PASS").unwrap_or_default(),
        subpath: None,
    })
}

/// Connects and creates a scratch directory; returns the backend and its path.
async fn setup(name: &str) -> Option<(Arc<SmbBackend>, RPath)> {
    let target = target()?;
    let backend = Arc::new(SmbBackend::new("live", target));
    backend.connect().await.expect("connect");
    let dir = RPath::new(&format!("/mydrives-test-{name}"));
    // Leftovers from an interrupted run must not fail the test.
    let _ = cleanup(&backend, &dir).await;
    backend.mkdir(&dir).await.expect("mkdir scratch");
    Some((backend, dir))
}

/// Recursive removal. `async fn` recursion needs an explicit box.
fn cleanup<'a>(
    backend: &'a SmbBackend,
    dir: &'a RPath,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = mydrives_core::Result<()>> + 'a>> {
    Box::pin(async move {
        if let Ok(entries) = backend.readdir(dir).await {
            for e in entries {
                let child = dir.join(&e.name);
                if e.attr.kind == FileKind::Dir {
                    let _ = cleanup(backend, &child).await;
                } else {
                    let _ = backend.unlink(&child).await;
                }
            }
        }
        backend.rmdir(dir).await
    })
}

macro_rules! live {
    ($name:ident) => {
        match setup(stringify!($name)).await {
            Some(x) => x,
            None => {
                eprintln!("skipping: MYDRIVES_SMB_HOST not set");
                return;
            }
        }
    };
}

#[tokio::test]
async fn connects_and_lists_the_share_root() {
    let (backend, dir) = live!(root);
    let entries = backend.readdir(&RPath::root()).await.expect("readdir root");
    assert!(
        entries.iter().any(|e| e.name == dir.name()),
        "scratch dir should show up in the root listing: {:?}",
        entries.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
    // "." and ".." must never leak out of the backend.
    assert!(!entries.iter().any(|e| e.name == "." || e.name == ".."));
    cleanup(&backend, &dir).await.expect("cleanup");
}

#[tokio::test]
async fn write_read_roundtrip_including_large_file() {
    let (backend, dir) = live!(io);

    let path = dir.join("payload.bin");
    let f = backend.create(&path).await.expect("create");
    // 3 MiB of non-repeating data, written in 256 KiB pieces.
    let data: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let mut off = 0u64;
    for piece in data.chunks(256 * 1024) {
        let n = backend.write_at(f, off, piece).await.expect("write");
        assert_eq!(n as usize, piece.len());
        off += n as u64;
    }
    backend.flush(f).await.expect("flush");
    backend.close(f).await.expect("close");

    let attr = backend.stat(&path).await.expect("stat");
    assert_eq!(attr.size, data.len() as u64);
    assert_eq!(attr.kind, FileKind::File);

    let f = backend
        .open(&path, OpenOpts::read_only())
        .await
        .expect("open");
    let mut got = Vec::new();
    let mut off = 0u64;
    loop {
        let part = backend.read_at(f, off, 512 * 1024).await.expect("read");
        if part.is_empty() {
            break;
        }
        off += part.len() as u64;
        got.extend_from_slice(&part);
    }
    // Reading exactly at EOF must be an empty read, not an error.
    assert!(backend
        .read_at(f, data.len() as u64, 4096)
        .await
        .expect("read at EOF")
        .is_empty());
    backend.close(f).await.expect("close");
    assert_eq!(got.len(), data.len());
    assert_eq!(got, data);

    cleanup(&backend, &dir).await.expect("cleanup");
}

#[tokio::test]
async fn truncate_and_timestamps() {
    let (backend, dir) = live!(meta);
    let path = dir.join("meta.txt");

    let f = backend.create(&path).await.expect("create");
    backend.write_at(f, 0, b"0123456789").await.expect("write");
    backend.close(f).await.expect("close");

    backend.set_len(&path, None, 4).await.expect("set_len");
    assert_eq!(backend.stat(&path).await.expect("stat").size, 4);

    backend.set_len(&path, None, 9).await.expect("extend");
    let attr = backend.stat(&path).await.expect("stat");
    assert_eq!(attr.size, 9);

    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    backend
        .set_attr(
            &path,
            None,
            &SetAttr {
                mtime: Some(when),
                ..Default::default()
            },
        )
        .await
        .expect("set_attr");
    let attr = backend.stat(&path).await.expect("stat");
    let delta = attr
        .mtime
        .duration_since(when)
        .or_else(|_| when.duration_since(attr.mtime))
        .unwrap();
    assert!(
        delta < std::time::Duration::from_secs(2),
        "mtime should be the one we set, got {:?}",
        attr.mtime
    );

    cleanup(&backend, &dir).await.expect("cleanup");
}

#[tokio::test]
async fn rename_moves_files_and_directories() {
    let (backend, dir) = live!(rename);

    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    let f = backend.create(&a).await.expect("create");
    backend.write_at(f, 0, b"content").await.expect("write");
    backend.close(f).await.expect("close");

    backend.rename(&a, &b, false).await.expect("rename file");
    assert!(backend.stat(&a).await.is_err());
    assert_eq!(backend.stat(&b).await.expect("stat b").size, 7);

    let sub = dir.join("subdir");
    backend.mkdir(&sub).await.expect("mkdir");
    let inner = sub.join("inner.txt");
    let f = backend.create(&inner).await.expect("create inner");
    backend.close(f).await.expect("close");

    let moved = dir.join("moved");
    backend.rename(&sub, &moved, false).await.expect("rename dir");
    let entries = backend.readdir(&moved).await.expect("readdir moved");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "inner.txt");

    cleanup(&backend, &dir).await.expect("cleanup");
}

/// The `sed -i` / "save in Office" pattern: write a temp file, then rename it
/// over the original. If `replace_if_exists` does not work, the edit silently
/// has no effect - which is what a user would call data loss.
#[tokio::test]
async fn rename_over_an_existing_file_replaces_it() {
    let (backend, dir) = live!(replace);

    let target = dir.join("doc.txt");
    let temp = dir.join("doc.txt.tmp");

    let f = backend.create(&target).await.expect("create target");
    backend.write_at(f, 0, b"old content").await.expect("write");
    backend.close(f).await.expect("close");

    let f = backend.create(&temp).await.expect("create temp");
    backend.write_at(f, 0, b"new content").await.expect("write");
    backend.close(f).await.expect("close");

    backend
        .rename(&temp, &target, true)
        .await
        .expect("rename over the existing file");

    assert!(backend.stat(&temp).await.is_err(), "temp file should be gone");
    let attr = backend.stat(&target).await.expect("target still exists");
    assert_eq!(attr.size, 11);

    let f = backend
        .open(&target, OpenOpts::read_only())
        .await
        .expect("open");
    let data = backend.read_at(f, 0, 64).await.expect("read");
    backend.close(f).await.expect("close");
    assert_eq!(&data, b"new content", "the replacement must win");

    // Without `replace`, the same rename must fail instead of destroying data.
    let f = backend.create(&temp).await.expect("recreate temp");
    backend.close(f).await.expect("close");
    let err = backend.rename(&temp, &target, false).await.unwrap_err();
    assert!(
        matches!(err, Error::AlreadyExists(_)),
        "expected EEXIST, got {err}"
    );

    cleanup(&backend, &dir).await.expect("cleanup");
}

#[tokio::test]
async fn error_cases_map_to_the_right_errno() {
    let (backend, dir) = live!(errors);

    // missing file
    let err = backend.stat(&dir.join("nope.txt")).await.unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err}");
    assert_eq!(err.errno(), 2);

    // create twice
    let path = dir.join("dup.txt");
    let f = backend.create(&path).await.expect("create");
    backend.close(f).await.expect("close");
    let err = backend.create(&path).await.unwrap_err();
    assert!(matches!(err, Error::AlreadyExists(_)), "{err}");

    // rmdir on a non-empty directory
    let err = backend.rmdir(&dir).await.unwrap_err();
    assert!(
        matches!(err, Error::NotEmpty(_)),
        "expected ENOTEMPTY, got {err}"
    );

    // unlink on a directory / rmdir on a file
    let sub = dir.join("d");
    backend.mkdir(&sub).await.expect("mkdir");
    let err = backend.unlink(&sub).await.unwrap_err();
    assert!(matches!(err, Error::IsADirectory(_)), "{err}");
    let err = backend.rmdir(&path).await.unwrap_err();
    assert!(matches!(err, Error::NotADirectory(_)), "{err}");

    // opening a directory as a file
    let err = backend.open(&sub, OpenOpts::read_only()).await.unwrap_err();
    assert!(matches!(err, Error::IsADirectory(_)), "{err}");

    cleanup(&backend, &dir).await.expect("cleanup");
}

#[tokio::test]
async fn statfs_reports_real_numbers() {
    let (backend, dir) = live!(statfs);
    let st = backend.statfs().await.expect("statfs");
    assert!(st.total_bytes > 0, "{st:?}");
    assert!(st.free_bytes <= st.total_bytes, "{st:?}");
    assert!(st.block_size >= 512, "{st:?}");
    cleanup(&backend, &dir).await.expect("cleanup");
}

#[tokio::test]
async fn vfs_over_smb_behaves_like_a_filesystem() {
    let (backend, dir) = live!(vfs);
    let vfs = Vfs::new(
        backend.clone(),
        VfsOptions {
            read_chunk: 64 * 1024,
            ..Default::default()
        },
    );

    let path = dir.join("through-vfs.bin");
    let (fh, _ino, _attr) = vfs
        .create_path(&path, OpenOpts::read_write())
        .await
        .expect("create");
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 97) as u8).collect();
    let mut off = 0u64;
    for piece in data.chunks(32 * 1024) {
        vfs.write(fh, off, piece).await.expect("write");
        off += piece.len() as u64;
    }
    vfs.release(fh).await.expect("release");

    assert_eq!(vfs.stat_path(&path).await.expect("stat").size, data.len() as u64);

    // Small sequential reads must be served from the chunk cache.
    let fh = vfs
        .open_path(&path, OpenOpts::read_only())
        .await
        .expect("open");
    let mut got = Vec::new();
    let mut off = 0u64;
    loop {
        let part = vfs.read(fh, off, 4096).await.expect("read");
        if part.is_empty() {
            break;
        }
        off += part.len() as u64;
        got.extend_from_slice(&part);
    }
    vfs.release(fh).await.expect("release");
    assert_eq!(got, data);

    let stats = vfs.stats();
    assert!(
        stats.cache_hits > 20,
        "chunked reads should mostly hit the cache over the network: {stats:?}"
    );

    // Directory listing through the VFS, with inode identity.
    let (dir_ino, _) = vfs
        .lookup(mydrives_core::ROOT_INO, dir.name())
        .await
        .expect("lookup scratch dir");
    let entries = vfs.readdir(dir_ino).await.expect("readdir");
    assert!(entries.iter().any(|(_, e)| e.name == "through-vfs.bin"));

    cleanup(&backend, &dir).await.expect("cleanup");
}
