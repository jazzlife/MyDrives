//! Delay-load WinFsp in the agent executable.
//!
//! This has to live in the package that produces the binary: cargo only applies
//! `rustc-link-arg` to targets of the package whose build script emitted it, so
//! putting it in `mydrives-mount-windows` (an rlib) has no effect on the linked
//! exe. Without it, `mydrivesd.exe` fails to start with STATUS_DLL_NOT_FOUND
//! (0xC0000135) on any machine where WinFsp is not installed - before `main`
//! runs, so the user gets no message at all.

fn main() {
    #[cfg(windows)]
    winfsp::build::winfsp_link_delayload();
}
