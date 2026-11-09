//! Backends that need no external dependencies live here; protocol backends get
//! their own crate so that a build for one platform does not have to compile the
//! client libraries of every cloud provider.

pub mod mem;

pub use mem::MemBackend;
