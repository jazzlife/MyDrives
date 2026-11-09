//! Windows mount provider.
//!
//! Empty on other platforms; see [`imp`] for the WinFsp implementation.

#[cfg(windows)]
mod imp;

#[cfg(windows)]
pub use imp::WinFspProvider;

#[cfg(not(windows))]
/// Placeholder so that `cargo build --workspace` works on Linux and macOS.
pub struct WinFspProvider;
