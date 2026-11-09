//! VFS semantics over the in-memory backend.
//!
//! These are the invariants the OS mount layers depend on. If one of these breaks,
//! Explorer/Finder break in ways that are much harder to diagnose through a kernel
//! driver, so they are checked here first.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use mydrives_core::backends::mem::{fixture, MemBackend};
use mydrives_core::types::{FileKind, OpenOpts, SetAttr};
use mydrives_core::{Backend, RPath, Vfs, VfsOptions, ROOT_INO};

fn vfs(backend: MemBackend) -> Arc<Vfs> {
    Vfs::new(Arc::new(backend), VfsOptions::default())
}

#[tokio::test]
async fn lookup_readdir_and_inode_identity() {
    let vfs = vfs(fixture("t"));

    let (docs_ino, docs_attr) = vfs.lookup(ROOT_INO, "docs").await.unwrap();
    assert_eq!(docs_attr.kind, FileKind::Dir);

    let entries = vfs.readdir(docs_ino).await.unwrap();
    let mut names: Vec<_> = entries.iter().map(|(_, e)| e.name.clone()).collect();
    names.sort();
    assert_eq!(names, vec!["big.bin", "readme.txt"]);

    // The same path must always resolve to the same inode.
    let (again, _) = vfs.lookup(ROOT_INO, "docs").await.unwrap();
    assert_eq!(docs_ino, again);

    // Missing names must be ENOENT, not an I/O error.
    let err = vfs.lookup(ROOT_INO, "nope").await.unwrap_err();
    assert_eq!(err.errno(), 2);
}

#[tokio::test]
async fn read_is_chunked_but_byte_exact() {
    let backend = MemBackend::new("t");
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    backend.put_file("/f.bin", &data);

    let vfs = Vfs::new(
        Arc::new(backend),
        VfsOptions {
            read_chunk: 64 * 1024,
            ..Default::default()
        },
    );
    let fh = vfs
        .open_path(&RPath::new("/f.bin"), OpenOpts::read_only())
        .await
        .unwrap();

    // Sequential 4 KiB reads: must reassemble exactly, and the chunk cache must
    // collapse them into far fewer backend calls than requests.
    let mut got = Vec::new();
    let mut off = 0u64;
    loop {
        let part = vfs.read(fh, off, 4096).await.unwrap();
        if part.is_empty() {
            break;
        }
        off += part.len() as u64;
        got.extend_from_slice(&part);
    }
    assert_eq!(got, data);

    let stats = vfs.stats();
    assert!(
        stats.cache_hits > 50,
        "expected most reads to hit the chunk cache, got {stats:?}"
    );

    // Random access across chunk boundaries.
    let mid = vfs.read(fh, 64 * 1024 - 10, 20).await.unwrap();
    assert_eq!(mid, &data[64 * 1024 - 10..64 * 1024 + 10]);

    // Reads past EOF are empty, not an error.
    assert!(vfs.read(fh, 10_000_000, 4096).await.unwrap().is_empty());
    vfs.release(fh).await.unwrap();
}

#[tokio::test]
async fn write_then_read_sees_new_bytes_through_other_handle() {
    let backend = MemBackend::new("t");
    backend.put_file("/f.txt", b"aaaaaaaaaa");
    let vfs = vfs_from(backend);

    let reader = vfs
        .open_path(&RPath::new("/f.txt"), OpenOpts::read_only())
        .await
        .unwrap();
    assert_eq!(vfs.read(reader, 0, 10).await.unwrap(), b"aaaaaaaaaa");

    let writer = vfs
        .open_path(&RPath::new("/f.txt"), OpenOpts::read_write())
        .await
        .unwrap();
    vfs.write(writer, 0, b"bb").await.unwrap();
    vfs.flush(writer).await.unwrap();

    // The read cache of the first handle must not serve the old bytes.
    assert_eq!(vfs.read(reader, 0, 10).await.unwrap(), b"bbaaaaaaaa");

    vfs.release(writer).await.unwrap();
    vfs.release(reader).await.unwrap();
}

fn vfs_from(backend: MemBackend) -> Arc<Vfs> {
    Vfs::new(Arc::new(backend), VfsOptions::default())
}

#[tokio::test]
async fn create_write_grow_and_truncate() {
    let vfs = vfs(MemBackend::new("t"));
    let path = RPath::new("/new.txt");

    let (fh, _ino, attr) = vfs.create_path(&path, OpenOpts::read_write()).await.unwrap();
    assert_eq!(attr.size, 0);

    assert_eq!(vfs.write(fh, 0, b"hello ").await.unwrap(), 6);
    assert_eq!(vfs.write(fh, 6, b"world").await.unwrap(), 5);
    // The size must be visible while the file is still open, or Explorer shows 0.
    assert_eq!(vfs.stat_path(&path).await.unwrap().size, 11);
    vfs.release(fh).await.unwrap();

    assert_eq!(vfs.stat_path(&path).await.unwrap().size, 11);

    vfs.setattr_path(
        &path,
        SetAttr {
            size: Some(5),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(vfs.stat_path(&path).await.unwrap().size, 5);

    let fh = vfs.open_path(&path, OpenOpts::read_only()).await.unwrap();
    assert_eq!(vfs.read(fh, 0, 100).await.unwrap(), b"hello");
    vfs.release(fh).await.unwrap();

    // Creating the same name twice must collide.
    let err = vfs
        .create_path(&path, OpenOpts::read_write())
        .await
        .unwrap_err();
    assert_eq!(err.errno(), 17);
}

#[tokio::test]
async fn open_truncate_resets_size() {
    let backend = MemBackend::new("t");
    backend.put_file("/f.txt", b"0123456789");
    let vfs = vfs_from(backend);
    let path = RPath::new("/f.txt");
    let fh = vfs
        .open_path(
            &path,
            OpenOpts {
                read: true,
                write: true,
                truncate: true,
                append: false,
            },
        )
        .await
        .unwrap();
    assert!(vfs.read(fh, 0, 10).await.unwrap().is_empty());
    assert_eq!(vfs.stat_path(&path).await.unwrap().size, 0);
    vfs.release(fh).await.unwrap();
}

#[tokio::test]
async fn rename_directory_keeps_inodes_and_invalidates_cache() {
    let vfs = vfs(fixture("t"));

    let (docs, _) = vfs.lookup(ROOT_INO, "docs").await.unwrap();
    let (readme, _) = vfs.lookup(docs, "readme.txt").await.unwrap();
    // Warm the caches, so a stale entry would be noticed.
    vfs.readdir(docs).await.unwrap();

    vfs.rename(ROOT_INO, "docs", ROOT_INO, "papers", false)
        .await
        .unwrap();

    assert_eq!(vfs.path_of(docs).unwrap().as_str(), "/papers");
    assert_eq!(vfs.path_of(readme).unwrap().as_str(), "/papers/readme.txt");
    assert_eq!(vfs.getattr(readme).await.unwrap().size, 15);
    assert!(vfs.lookup(ROOT_INO, "docs").await.is_err());

    let names: Vec<String> = vfs
        .readdir(ROOT_INO)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, e)| e.name)
        .collect();
    assert!(names.contains(&"papers".to_string()), "{names:?}");
    assert!(!names.contains(&"docs".to_string()), "{names:?}");
}

#[tokio::test]
async fn rename_rejects_moving_into_own_subtree() {
    let vfs = vfs(fixture("t"));
    let err = vfs
        .rename_path(&RPath::new("/docs"), &RPath::new("/docs/inner"), false)
        .await
        .unwrap_err();
    assert_eq!(err.errno(), 22);
}

#[tokio::test]
async fn unlink_and_rmdir_semantics() {
    let vfs = vfs(fixture("t"));

    // unlink on a directory must be EISDIR, rmdir on a file ENOTDIR-ish.
    assert_eq!(
        vfs.unlink_path(&RPath::new("/docs")).await.unwrap_err().errno(),
        21
    );
    assert_eq!(
        vfs.rmdir_path(&RPath::new("/docs"))
            .await
            .unwrap_err()
            .errno(),
        39 // ENOTEMPTY
    );

    vfs.unlink_path(&RPath::new("/docs/readme.txt")).await.unwrap();
    vfs.unlink_path(&RPath::new("/docs/big.bin")).await.unwrap();
    vfs.rmdir_path(&RPath::new("/docs")).await.unwrap();
    assert!(vfs.stat_path(&RPath::new("/docs")).await.is_err());

    // The root must never be removable.
    assert!(vfs.rmdir_path(&RPath::root()).await.is_err());
}

#[tokio::test]
async fn readonly_mount_blocks_every_mutation() {
    let vfs = Vfs::new(
        Arc::new(fixture("t")),
        VfsOptions {
            readonly: true,
            ..Default::default()
        },
    );

    assert_eq!(
        vfs.create_path(&RPath::new("/x"), OpenOpts::read_write())
            .await
            .unwrap_err()
            .errno(),
        30 // EROFS
    );
    assert!(vfs.mkdir_path(&RPath::new("/d")).await.is_err());
    assert!(vfs.unlink_path(&RPath::new("/docs/readme.txt")).await.is_err());
    assert!(vfs
        .open_path(&RPath::new("/docs/readme.txt"), OpenOpts::read_write())
        .await
        .is_err());

    // Reading still works.
    let fh = vfs
        .open_path(&RPath::new("/docs/readme.txt"), OpenOpts::read_only())
        .await
        .unwrap();
    assert_eq!(vfs.read(fh, 0, 5).await.unwrap(), b"hello");
    vfs.release(fh).await.unwrap();
}

#[tokio::test]
async fn metadata_cache_cuts_round_trips_but_not_correctness() {
    let backend = Arc::new(fixture("t").with_latency(Duration::from_millis(1)));
    let vfs = Vfs::new(backend.clone(), VfsOptions::default());

    let t0 = SystemTime::now();
    for _ in 0..50 {
        vfs.stat_path(&RPath::new("/docs/readme.txt")).await.unwrap();
    }
    let cached = t0.elapsed().unwrap();
    let stats = vfs.stats();
    assert!(
        stats.cache_hits >= 49,
        "50 stats of the same path should hit the cache: {stats:?}"
    );

    // How long the same 50 calls take with the cache turned off, measured here
    // rather than hard-coded: an absolute millisecond bound only measures how
    // fast the machine running the test is, and fails on a slow or emulated one
    // for reasons that have nothing to do with the cache.
    let uncached_vfs = Vfs::new(
        backend.clone(),
        VfsOptions {
            attr_ttl: Duration::ZERO,
            dir_ttl: Duration::ZERO,
            ..VfsOptions::default()
        },
    );
    let t1 = SystemTime::now();
    for _ in 0..50 {
        uncached_vfs
            .stat_path(&RPath::new("/docs/readme.txt"))
            .await
            .unwrap();
    }
    let uncached = t1.elapsed().unwrap();
    assert!(
        cached * 2 < uncached,
        "cached stats should not pay backend latency: {cached:?} cached vs {uncached:?} uncached"
    );

    // Our own mutation must be reflected immediately, cache or not.
    vfs.unlink_path(&RPath::new("/docs/readme.txt")).await.unwrap();
    assert!(vfs.stat_path(&RPath::new("/docs/readme.txt")).await.is_err());
}

#[tokio::test]
async fn statfs_is_reported() {
    let vfs = vfs(fixture("t"));
    let st = vfs.statfs().await.unwrap();
    assert!(st.total_bytes > 0);
    assert!(st.free_bytes <= st.total_bytes);
    assert!(st.block_size > 0);
}

#[tokio::test]
async fn stale_handle_is_rejected() {
    let vfs = vfs(fixture("t"));
    let fh = vfs
        .open_path(&RPath::new("/docs/readme.txt"), OpenOpts::read_only())
        .await
        .unwrap();
    vfs.release(fh).await.unwrap();
    assert!(vfs.read(fh, 0, 1).await.is_err());
    // Releasing twice must be harmless - the kernel does this on forced unmount.
    vfs.release(fh).await.unwrap();
}

#[tokio::test]
async fn concurrent_readers_share_one_backend() {
    let vfs = vfs(fixture("t"));
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let vfs = vfs.clone();
        tasks.push(tokio::spawn(async move {
            let fh = vfs
                .open_path(&RPath::new("/docs/big.bin"), OpenOpts::read_only())
                .await
                .unwrap();
            let mut total = 0usize;
            let mut off = 0u64;
            while let Ok(part) = vfs.read(fh, off, 128 * 1024).await {
                if part.is_empty() {
                    break;
                }
                total += part.len();
                off += part.len() as u64;
            }
            vfs.release(fh).await.unwrap();
            total
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 3 * 1024 * 1024);
    }
    assert_eq!(vfs.stats().open_handles, 0);
}
