//! Times every backend call without changing one.
//!
//! A drive that feels slow gives no clue on its own whether the time went to
//! the network, to the service, or to a round trip we did not need to make.
//! Measured on 2026-11-12: one `echo hello > file` on an SMB share cost 206 ms
//! against a NAS 12 ms away, and the same write on OneDrive took anywhere from
//! 30 ms to four seconds with nothing in the log either way.
//!
//! This wrapper is deliberately the dullest thing that answers that: it
//! forwards every method untouched and records how long each one took. It
//! holds no state beyond a threshold, takes no locks, and cannot change what
//! the layers above see - the only way it affects behaviour is the time spent
//! reading a clock twice per call.
//!
//! Turn it up with `MYDRIVES_SLOW_CALL_MS` (default 2000) to pick what counts
//! as worth a warning; everything is logged at `debug` regardless, so
//! `RUST_LOG=mydrives_core::timed=debug` gives a full trace of calls and their
//! latency.

use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::backend::{BFile, Backend};
use crate::error::Result;
use crate::path::RPath;
use crate::types::{Attr, Caps, DirEntry, FsStat, OpenOpts, SetAttr};

/// How long a single backend call may take before it is worth a warning.
fn slow_threshold() -> Duration {
    let ms = std::env::var("MYDRIVES_SLOW_CALL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    Duration::from_millis(ms)
}

pub struct Timed {
    inner: std::sync::Arc<dyn Backend>,
    slow: Duration,
}

impl Timed {
    pub fn new(inner: std::sync::Arc<dyn Backend>) -> Self {
        Timed {
            inner,
            slow: slow_threshold(),
        }
    }

    fn done(&self, op: &'static str, path: &str, started: Instant, ok: bool) {
        let took = started.elapsed();
        if took >= self.slow {
            tracing::warn!(
                remote = self.inner.id(),
                op,
                path,
                ms = took.as_millis() as u64,
                ok,
                "slow backend call"
            );
        } else {
            tracing::debug!(
                remote = self.inner.id(),
                op,
                path,
                ms = took.as_millis() as u64,
                ok,
                "backend call"
            );
        }
    }
}

/// Forwards one call and times it. Written out rather than hidden behind a
/// closure so each arm stays a plain delegation anyone can check by eye.
macro_rules! timed {
    ($self:ident, $op:literal, $path:expr, $call:expr) => {{
        let started = Instant::now();
        let r = $call.await;
        $self.done($op, $path, started, r.is_ok());
        r
    }};
}

#[async_trait]
impl Backend for Timed {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn caps(&self) -> Caps {
        self.inner.caps()
    }

    async fn connect(&self) -> Result<()> {
        timed!(self, "connect", "", self.inner.connect())
    }
    async fn disconnect(&self) -> Result<()> {
        timed!(self, "disconnect", "", self.inner.disconnect())
    }
    async fn stat(&self, path: &RPath) -> Result<Attr> {
        timed!(self, "stat", path.as_str(), self.inner.stat(path))
    }
    async fn readdir(&self, path: &RPath) -> Result<Vec<DirEntry>> {
        timed!(self, "readdir", path.as_str(), self.inner.readdir(path))
    }
    async fn readdir_pages(
        &self,
        path: &RPath,
        pages: tokio::sync::mpsc::Sender<Vec<DirEntry>>,
    ) -> Result<()> {
        timed!(
            self,
            "readdir_pages",
            path.as_str(),
            self.inner.readdir_pages(path, pages)
        )
    }
    async fn open(&self, path: &RPath, opts: OpenOpts) -> Result<BFile> {
        timed!(self, "open", path.as_str(), self.inner.open(path, opts))
    }
    async fn create(&self, path: &RPath) -> Result<BFile> {
        timed!(self, "create", path.as_str(), self.inner.create(path))
    }
    async fn read_at(&self, f: BFile, offset: u64, len: u32) -> Result<Vec<u8>> {
        timed!(self, "read_at", "", self.inner.read_at(f, offset, len))
    }
    async fn write_at(&self, f: BFile, offset: u64, data: &[u8]) -> Result<u32> {
        timed!(self, "write_at", "", self.inner.write_at(f, offset, data))
    }
    async fn flush(&self, f: BFile) -> Result<()> {
        timed!(self, "flush", "", self.inner.flush(f))
    }
    async fn close(&self, f: BFile) -> Result<()> {
        timed!(self, "close", "", self.inner.close(f))
    }
    async fn set_len(&self, path: &RPath, f: Option<BFile>, len: u64) -> Result<()> {
        timed!(self, "set_len", path.as_str(), self.inner.set_len(path, f, len))
    }
    async fn set_attr(&self, path: &RPath, f: Option<BFile>, attr: &SetAttr) -> Result<()> {
        timed!(self, "set_attr", path.as_str(), self.inner.set_attr(path, f, attr))
    }
    async fn mkdir(&self, path: &RPath) -> Result<()> {
        timed!(self, "mkdir", path.as_str(), self.inner.mkdir(path))
    }
    async fn unlink(&self, path: &RPath) -> Result<()> {
        timed!(self, "unlink", path.as_str(), self.inner.unlink(path))
    }
    async fn rmdir(&self, path: &RPath) -> Result<()> {
        timed!(self, "rmdir", path.as_str(), self.inner.rmdir(path))
    }
    async fn rename(&self, from: &RPath, to: &RPath, replace: bool) -> Result<()> {
        timed!(self, "rename", from.as_str(), self.inner.rename(from, to, replace))
    }
    async fn statfs(&self) -> Result<FsStat> {
        timed!(self, "statfs", "", self.inner.statfs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::mem::MemBackend;
    use std::sync::Arc;

    /// The whole value of this wrapper is that it changes nothing. Put the
    /// same sequence through a backend and through a wrapped one and the
    /// answers have to match, byte for byte and error for error.
    #[tokio::test]
    async fn wrapping_a_backend_changes_none_of_its_answers() {
        let bare: Arc<dyn Backend> = Arc::new(MemBackend::new("same-id"));
        let wrapped: Arc<dyn Backend> =
            Arc::new(Timed::new(Arc::new(MemBackend::new("same-id"))));

        let dir = RPath::new("/w");
        let file = RPath::new("/w/f.txt");
        let missing = RPath::new("/nope");

        for b in [&bare, &wrapped] {
            b.connect().await.expect("connect");
            b.mkdir(&dir).await.expect("mkdir");
            let h = b.create(&file).await.expect("create");
            b.write_at(h, 0, b"hello timed").await.expect("write");
            b.close(h).await.expect("close");
        }

        let mut bytes = Vec::new();
        for b in [&bare, &wrapped] {
            let h = b.open(&file, OpenOpts::read_only()).await.expect("open");
            bytes.push(b.read_at(h, 0, 64).await.expect("read"));
            b.close(h).await.expect("close");
        }
        assert_eq!(bytes[0], b"hello timed");
        assert_eq!(bytes[0], bytes[1], "the wrapper changed the bytes read");

        assert_eq!(
            bare.stat(&file).await.unwrap().size,
            wrapped.stat(&file).await.unwrap().size
        );
        assert_eq!(
            bare.readdir(&dir).await.unwrap().len(),
            wrapped.readdir(&dir).await.unwrap().len()
        );
        assert_eq!(bare.id(), wrapped.id());

        // Errors have to come through unchanged too: the VFS decides what is
        // an answer and what is a failure by matching on these.
        for b in [&bare, &wrapped] {
            assert!(
                matches!(b.stat(&missing).await, Err(crate::error::Error::NotFound(_))),
                "a missing path must still report NotFound"
            );
        }
    }
}
