//! A file's local copy while it is open.
//!
//! Cloud services hand a file out whole and take it back whole: there is no
//! byte-range write, and for some no byte-range read either. So an open file
//! lives on local disk while it is open - downloaded on the first read or
//! write that needs the existing bytes, edited in place, uploaded in one piece
//! when it is flushed or closed. The same shape the FTP backend settled on,
//! for the same reason: the kernel delivers writes out of order and in pieces,
//! and only a complete local copy can be turned back into one upload.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use mydrives_core::error::{Error, Result};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// Distinct names for staging files across every backend in the process.
static SEQ: AtomicU64 = AtomicU64::new(1);

pub struct Staged {
    pub file: tokio::fs::File,
    path: PathBuf,
    pub len: u64,
    /// Bytes the service has not been given yet.
    pub dirty: bool,
}

impl Staged {
    /// An empty staging file in `dir` (created if needed).
    pub async fn create(dir: &std::path::Path) -> Result<Staged> {
        tokio::fs::create_dir_all(dir).await.map_err(Error::Io)?;
        let path = dir.join(format!(
            "stage-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map_err(Error::Io)?;
        Ok(Staged {
            file,
            path,
            len: 0,
            dirty: false,
        })
    }

    pub async fn read_at(&mut self, offset: u64, len: u32) -> Result<Vec<u8>> {
        if offset >= self.len {
            return Ok(Vec::new());
        }
        let want = len.min((self.len - offset) as u32) as usize;
        let mut buf = vec![0u8; want];
        self.file
            .seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(Error::Io)?;
        self.file.read_exact(&mut buf).await.map_err(Error::Io)?;
        Ok(buf)
    }

    pub async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.file
            .seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(Error::Io)?;
        self.file.write_all(data).await.map_err(Error::Io)?;
        self.len = self.len.max(offset + data.len() as u64);
        self.dirty = true;
        Ok(())
    }

    pub async fn set_len(&mut self, len: u64) -> Result<()> {
        self.file.set_len(len).await.map_err(Error::Io)?;
        self.len = len;
        self.dirty = true;
        Ok(())
    }

    /// A second handle on the same bytes, for an upload to read from the
    /// start while this one keeps its position.
    pub async fn reader(&mut self) -> Result<tokio::fs::File> {
        self.file.flush().await.map_err(Error::Io)?;
        let mut f = self.file.try_clone().await.map_err(Error::Io)?;
        f.seek(std::io::SeekFrom::Start(0)).await.map_err(Error::Io)?;
        Ok(f)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        // Uploaded or not, the local copy must not outlive the handle.
        let _ = std::fs::remove_file(&self.path);
    }
}
