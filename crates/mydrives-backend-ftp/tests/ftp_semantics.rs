//! The FTP backend against a real FTP server.
//!
//! The server is `libunftp` started inside the test process on a loopback port
//! over a temporary directory, so these are not mocks: every assertion here
//! went over a control connection and a data connection, through MLSD, REST,
//! RETR, STOR and the rest of it. Nothing has to be installed to run them.

use std::net::TcpListener;
use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use mydrives_backend_ftp::{FtpSecurity, FtpTarget};
use mydrives_core::backend::Backend;
use mydrives_core::error::Error;
use mydrives_core::path::RPath;
use mydrives_core::types::{FileKind, OpenOpts, SetAttr};

/// Ports for one test's server: the control port and a passive range.
///
/// Every test runs its own server, in parallel, and passive mode means the
/// server hands out a port for each data connection. Two servers sharing a
/// passive range therefore race for the same socket - a listing that lands on
/// another test's data connection, failing once in a while and passing when
/// run alone. So each server gets a block of its own: the block index comes
/// from a counter, the base from the process id so two `cargo test` runs on one
/// machine do not line up, and every port in the block is checked to be free
/// before it is handed out.
fn port_block() -> (u16, RangeInclusive<u16>) {
    const BLOCK: u16 = 24;
    static NEXT: AtomicU16 = AtomicU16::new(0);

    let spread = (std::process::id() % 400) as u16 * BLOCK;
    for attempt in 0..64u16 {
        let index = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = 20_000 + (spread + index.wrapping_mul(BLOCK) + attempt) % 40_000;
        if (base..base + BLOCK).all(|p| TcpListener::bind(("127.0.0.1", p)).is_ok()) {
            return (base, base + 1..=base + BLOCK - 1);
        }
    }
    panic!("no free block of ports for the test server");
}

struct Server {
    _dir: tempfile::TempDir,
    backend: Arc<dyn Backend>,
}

async fn server() -> Server {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().to_path_buf();
    let (port, passive) = port_block();

    let ftp = libunftp::ServerBuilder::new(Box::new(move || {
        unftp_sbe_fs::Filesystem::new(root.clone()).expect("fs backend")
    }))
    .greeting("MyDrives test server")
    .passive_ports(passive)
    .build()
    .expect("build server");

    tokio::spawn(async move {
        let _ = ftp.listen(format!("127.0.0.1:{port}")).await;
    });

    let backend = mydrives_backend_ftp::backend(
        "test".into(),
        FtpTarget {
            host: "127.0.0.1".into(),
            port,
            user: "anonymous".into(),
            password: "test@example.com".into(),
            security: FtpSecurity::Plain,
            ..Default::default()
        },
    );

    // The listener needs a moment before the first connect lands.
    for attempt in 0..50 {
        if backend.connect().await.is_ok() {
            return Server { _dir: dir, backend };
        }
        if attempt == 49 {
            panic!("the test FTP server never came up");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    unreachable!()
}

async fn write_file(backend: &Arc<dyn Backend>, path: &RPath, data: &[u8]) {
    let f = backend.create(path).await.expect("create");
    let mut written = 0usize;
    while written < data.len() {
        let n = backend
            .write_at(f, written as u64, &data[written..])
            .await
            .expect("write") as usize;
        written += n;
    }
    backend.close(f).await.expect("close");
}

async fn read_all(backend: &Arc<dyn Backend>, path: &RPath) -> Vec<u8> {
    let f = backend.open(path, OpenOpts::read_only()).await.expect("open");
    let mut out = Vec::new();
    loop {
        let chunk = backend
            .read_at(f, out.len() as u64, 64 * 1024)
            .await
            .expect("read");
        if chunk.is_empty() {
            break;
        }
        out.extend_from_slice(&chunk);
    }
    backend.close(f).await.expect("close");
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_come_back_byte_for_byte() {
    let s = server().await;
    let path = RPath::new("/hello.txt");
    write_file(&s.backend, &path, b"hello, world!").await;

    assert_eq!(read_all(&s.backend, &path).await, b"hello, world!");
    let attr = s.backend.stat(&path).await.expect("stat");
    assert_eq!(attr.kind, FileKind::File);
    assert_eq!(attr.size, 13);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_larger_than_one_chunk_survives_the_round_trip() {
    let s = server().await;
    let path = RPath::new("/big.bin");
    // Not a round number, and not compressible into a pattern a buggy chunk
    // boundary would still get right.
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    write_file(&s.backend, &path, &data).await;

    let back = read_all(&s.backend, &path).await;
    assert_eq!(back.len(), data.len());
    assert!(back == data, "the bytes that came back are not the ones sent");
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_from_an_offset_restarts_the_transfer_there() {
    let s = server().await;
    let path = RPath::new("/offset.bin");
    let data: Vec<u8> = (0..100_000u32).map(|i| (i % 256) as u8).collect();
    write_file(&s.backend, &path, &data).await;

    let f = s.backend.open(&path, OpenOpts::read_only()).await.expect("open");
    // Forwards, then backwards: both have to re-issue REST + RETR.
    let tail = s.backend.read_at(f, 90_000, 1_000).await.expect("read tail");
    assert_eq!(tail, data[90_000..91_000]);
    let head = s.backend.read_at(f, 10, 20).await.expect("read head");
    assert_eq!(head, data[10..30]);
    // Reading past the end is a normal thing for a filesystem to do.
    let past = s.backend.read_at(f, 100_000, 4_096).await.expect("read past end");
    assert!(past.is_empty());
    s.backend.close(f).await.expect("close");
}

#[tokio::test(flavor = "multi_thread")]
async fn directories_list_what_is_in_them() {
    let s = server().await;
    let dir = RPath::new("/photos");
    s.backend.mkdir(&dir).await.expect("mkdir");
    write_file(&s.backend, &dir.join("a.txt"), b"a").await;
    write_file(&s.backend, &dir.join("b.txt"), b"bb").await;
    s.backend.mkdir(&dir.join("sub")).await.expect("mkdir sub");

    let mut names: Vec<String> = s
        .backend
        .readdir(&dir)
        .await
        .expect("readdir")
        .into_iter()
        .map(|e| e.name)
        .collect();
    names.sort();
    assert_eq!(names, vec!["a.txt", "b.txt", "sub"]);

    // The listing is not supposed to contain "." or "..".
    let root = s.backend.readdir(&RPath::root()).await.expect("readdir root");
    assert!(root.iter().all(|e| e.name != "." && e.name != ".."));
    assert_eq!(s.backend.stat(&dir).await.expect("stat").kind, FileKind::Dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn create_refuses_a_name_that_is_taken() {
    let s = server().await;
    let path = RPath::new("/once.txt");
    write_file(&s.backend, &path, b"x").await;

    match s.backend.create(&path).await {
        Err(Error::AlreadyExists(_)) => {}
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_file_is_not_found_rather_than_a_backend_error() {
    let s = server().await;
    match s.backend.stat(&RPath::new("/nope.txt")).await {
        Err(Error::NotFound(_)) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_moves_a_file_and_refuses_to_clobber() {
    let s = server().await;
    let from = RPath::new("/from.txt");
    let to = RPath::new("/to.txt");
    write_file(&s.backend, &from, b"data").await;
    write_file(&s.backend, &to, b"other").await;

    match s.backend.rename(&from, &to, false).await {
        Err(Error::AlreadyExists(_)) => {}
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    // With replace, the target goes first - FTP has no atomic replace.
    s.backend.rename(&from, &to, true).await.expect("rename");
    assert_eq!(read_all(&s.backend, &to).await, b"data");
    assert!(matches!(
        s.backend.stat(&from).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_and_rmdir_behave_like_a_filesystem() {
    let s = server().await;
    let dir = RPath::new("/gone");
    s.backend.mkdir(&dir).await.expect("mkdir");
    let file = dir.join("file.txt");
    write_file(&s.backend, &file, b"bye").await;

    // A directory with something in it must not just disappear.
    assert!(s.backend.rmdir(&dir).await.is_err());
    s.backend.unlink(&file).await.expect("unlink");
    s.backend.rmdir(&dir).await.expect("rmdir");
    assert!(matches!(
        s.backend.stat(&dir).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn truncating_shortens_the_file_and_zero_empties_it() {
    let s = server().await;
    let path = RPath::new("/trim.txt");
    write_file(&s.backend, &path, b"0123456789").await;

    s.backend.set_len(&path, None, 4).await.expect("truncate");
    assert_eq!(read_all(&s.backend, &path).await, b"0123");

    s.backend.set_len(&path, None, 0).await.expect("empty");
    assert_eq!(s.backend.stat(&path).await.expect("stat").size, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn opening_with_truncate_empties_before_the_first_write() {
    let s = server().await;
    let path = RPath::new("/replace.txt");
    write_file(&s.backend, &path, b"old contents").await;

    let f = s
        .backend
        .open(
            &path,
            OpenOpts {
                read: true,
                write: true,
                truncate: true,
                append: false,
            },
        )
        .await
        .expect("open");
    s.backend.write_at(f, 0, b"new").await.expect("write");
    s.backend.close(f).await.expect("close");
    assert_eq!(read_all(&s.backend, &path).await, b"new");
}

#[tokio::test(flavor = "multi_thread")]
async fn setting_a_timestamp_either_takes_or_is_ignored_but_never_fails() {
    let s = server().await;
    let path = RPath::new("/dated.txt");
    write_file(&s.backend, &path, b"x").await;
    let before = s.backend.stat(&path).await.expect("stat").mtime;

    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_768_048_496);
    let set = SetAttr {
        mtime: Some(when),
        ..Default::default()
    };
    // MFMT is not universal. A server without it must not turn `cp -p` or a
    // Finder copy into an error after the bytes are already safely written, so
    // the call succeeds either way.
    s.backend.set_attr(&path, None, &set).await.expect("set_attr");

    let after = s.backend.stat(&path).await.expect("stat").mtime;
    let secs = |t: SystemTime| t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
    assert!(
        secs(after) == secs(when) || secs(after) == secs(before),
        "the time became {}, which is neither what was asked for nor what it was",
        secs(after)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subpath_becomes_the_root_the_mount_sees() {
    let s = server().await;
    s.backend.mkdir(&RPath::new("/public")).await.expect("mkdir");
    write_file(&s.backend, &RPath::new("/public/inside.txt"), b"in").await;
    write_file(&s.backend, &RPath::new("/outside.txt"), b"out").await;

    let scoped = mydrives_backend_ftp::backend(
        "scoped".into(),
        FtpTarget {
            host: "127.0.0.1".into(),
            port: port_of(&s.backend),
            user: "anonymous".into(),
            password: "test@example.com".into(),
            security: FtpSecurity::Plain,
            subpath: Some("/public".into()),
            ..Default::default()
        },
    );
    scoped.connect().await.expect("connect scoped");

    let names: Vec<String> = scoped
        .readdir(&RPath::root())
        .await
        .expect("readdir")
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, vec!["inside.txt"]);
    assert_eq!(read_all(&scoped, &RPath::new("/inside.txt")).await, b"in");
}

/// The port the backend under test is talking to, read back out of its own
/// description so the test does not have to thread it around separately.
fn port_of(backend: &Arc<dyn Backend>) -> u16 {
    backend
        .describe()
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .expect("the description ends in the port")
}
