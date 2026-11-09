use std::io;
use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;
use std::sync::Arc;

use nix::errno::Errno;

use crate::dev_fuse::DevFuse;
use crate::passthrough::BackingId;

/// A raw communication channel to the FUSE kernel driver
#[derive(Debug, Clone)]
pub(crate) struct Channel(Arc<DevFuse>);

impl AsFd for Channel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Channel {
    /// Create a new communication channel to the kernel driver by mounting the
    /// given path. The kernel driver will delegate filesystem operations of
    /// the given path to the channel.
    pub(crate) fn new(device: Arc<DevFuse>) -> Self {
        Self(device)
    }

    /// Receives data up to the capacity of the given buffer (can block).
    fn receive(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        nix::unistd::read(&self.0, buffer)
    }

    /// Receives data up to the capacity of the given buffer (can block),
    /// retrying on errors that are safe to retry (ENOENT, EINTR, EAGAIN).
    ///
    /// - ENOENT: Operation interrupted. According to FUSE, this is safe to retry.
    /// - EINTR: Interrupted system call, retry.
    /// - EAGAIN: Explicitly instructed to try again.
    pub(crate) fn receive_retrying(&self, buffer: &mut [u8]) -> nix::Result<usize> {
        let read = loop {
            match self.receive(buffer) {
                Ok(size) => break size,
                Err(Errno::ENOENT | Errno::EINTR | Errno::EAGAIN) => continue,
                Err(err) => return Err(err),
            }
        };
        self.receive_rest(buffer, read)
    }

    /// Reads the rest of a request that arrived in pieces.
    ///
    /// `/dev/fuse` hands over exactly one request per `read`, so upstream needs
    /// nothing here. FUSE-T's channel is a **socket**, and a stream socket may
    /// deliver a 32 KiB write request in several chunks - upstream then reports
    /// "Short read of FUSE request (80 < 32848)" and the write hangs forever.
    /// The length in the request header says how much is coming, so the rest is
    /// simply read until the message is whole.
    fn receive_rest(&self, buffer: &mut [u8], read: usize) -> nix::Result<usize> {
        const HEADER_LEN: usize = std::mem::size_of::<u32>();
        if read < HEADER_LEN {
            return Ok(read);
        }
        let total = u32::from_ne_bytes(buffer[..HEADER_LEN].try_into().unwrap()) as usize;
        if total <= read || total > buffer.len() {
            return Ok(read);
        }
        let mut have = read;
        while have < total {
            match self.receive(&mut buffer[have..total]) {
                // The peer went away mid-request; let the caller see a short
                // read rather than spin.
                Ok(0) => break,
                Ok(size) => have += size,
                Err(Errno::ENOENT | Errno::EINTR | Errno::EAGAIN) => continue,
                Err(err) => return Err(err),
            }
        }
        Ok(have)
    }

    /// Returns a sender object for this channel. The sender object can be
    /// used to send to the channel. Multiple sender objects can be used
    /// and they can safely be sent to other threads.
    pub(crate) fn sender(&self) -> ChannelSender {
        // Since write/writev syscalls are threadsafe, we can simply create
        // a sender by using the same file and use it in other threads.
        ChannelSender(self.0.clone())
    }

    /// Clone the FUSE device fd using FUSE_DEV_IOC_CLONE ioctl.
    ///
    /// This creates a new fd that can read FUSE requests independently,
    /// enabling true parallel request processing. The kernel distributes
    /// requests across all cloned fds.
    ///
    /// Requires Linux 4.5+. Returns an error on older kernels or non-Linux.
    #[cfg(target_os = "linux")]
    pub(crate) fn clone_fd(&self) -> io::Result<Channel> {
        use std::os::fd::AsRawFd;

        let new_dev = DevFuse::open()?;

        let mut source_fd = self.0.as_raw_fd() as u32;
        // SAFETY: fuse_dev_ioc_clone is a valid ioctl for /dev/fuse
        unsafe {
            crate::ll::ioctl::fuse_dev_ioc_clone(new_dev.as_raw_fd(), &mut source_fd)?;
        }

        Ok(Channel::new(Arc::new(new_dev)))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ChannelSender(Arc<DevFuse>);

impl ChannelSender {
    /// Wakes a reader that is blocked on the channel.
    ///
    /// `read` on `/dev/fuse` returns as soon as the mount is gone, so upstream
    /// needs nothing here. FUSE-T's channel is a socket to its helper, and the
    /// helper keeps it open after the volume is unmounted: the session thread
    /// then blocks forever and `umount_and_join` never returns. Shutting the
    /// socket down ends that read. A channel that is not a socket - the usual
    /// case - fails with ENOTSOCK, which is exactly the "nothing to do" answer.
    pub(crate) fn shutdown(&self) {
        use std::os::fd::AsRawFd;
        unsafe { libc::shutdown(self.0.as_raw_fd(), libc::SHUT_RDWR) };
    }

    pub(crate) fn send(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        let rc = nix::sys::uio::writev(&self.0, bufs)?;
        // writev is atomic, so do not need to check how many bytes are written.
        // libfuse does not do it either
        // https://github.com/libfuse/libfuse/blob/6278995cca991978abd25ebb2c20ebd3fc9e8a13/lib/fuse_lowlevel.c#L267
        debug_assert_eq!(bufs.iter().map(|b| b.len()).sum::<usize>(), rc);
        Ok(())
    }

    pub(crate) fn open_backing(&self, fd: BorrowedFd<'_>) -> std::io::Result<BackingId> {
        BackingId::create(&self.0, fd)
    }

    pub(crate) unsafe fn wrap_backing(&self, id: u32) -> BackingId {
        unsafe { BackingId::wrap_raw(&self.0, id) }
    }
}
