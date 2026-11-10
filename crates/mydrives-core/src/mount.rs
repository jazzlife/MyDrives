//! The mount-layer contract.
//!
//! This is the seam that keeps the project portable. Everything above it (VFS,
//! backends, agent, UI) is plain Rust; everything below it is OS kernel plumbing
//! with its own licence, its own installer and its own failure modes:
//!
//! | provider | OS            | kernel component        | licence             |
//! |----------|---------------|-------------------------|---------------------|
//! | `winfsp` | Windows       | WinFsp FSD (installed)  | GPLv3 / commercial  |
//! | `dokan`  | Windows       | Dokany driver           | LGPL + MIT          |
//! | `fuse`   | Linux, macOS  | fuse / macFUSE kext     | GPL / macFUSE terms |
//! | `nfs`    | macOS, Linux  | in-box NFS client       | none needed         |
//!
//! Adding a provider must never require touching the VFS.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::vfs::Vfs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MountTarget {
    /// Windows drive letter, stored without the colon ("Z").
    Drive(char),
    /// Directory mount point (POSIX, and Windows directory mounts).
    Path(PathBuf),
}

impl MountTarget {
    pub fn display(&self) -> String {
        match self {
            MountTarget::Drive(c) => format!("{c}:"),
            MountTarget::Path(p) => p.display().to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MountSpec {
    pub id: String,
    pub label: String,
    pub target: MountTarget,
    pub readonly: bool,
    /// Shown as the volume name in Explorer/Finder.
    pub volume_name: String,
    /// Backend description, used for the Finder/Explorer "network path" column.
    pub source: String,
    /// May the mount layer escalate with `sudo -n`? Comes from the config
    /// (`[general] sudo_mount`), because the agent is started in ways that
    /// carry no environment of their own.
    pub sudo_mount: bool,
}

/// A live mount. Dropping it must not leave the OS with a dangling mount point,
/// so implementations unmount on `Drop` as a backstop and `unmount()` is the
/// orderly path.
pub trait MountSession: Send + Sync {
    fn mountpoint(&self) -> String;
    fn unmount(self: Box<Self>) -> Result<()>;
}

pub trait MountProvider: Send + Sync {
    fn name(&self) -> &'static str;

    /// Is the kernel component present and usable on this machine? Returns a
    /// human-readable reason when not, which the tray shows to the user.
    fn probe(&self) -> Result<()>;

    /// Can this provider serve the requested target shape?
    fn supports(&self, target: &MountTarget) -> bool;

    /// Blocks until the filesystem is mounted and visible to the OS, then returns.
    /// The provider owns whatever threads it needs; `rt` is the runtime its
    /// callbacks must use to drive the (async) VFS.
    fn mount(
        &self,
        spec: &MountSpec,
        vfs: Arc<Vfs>,
        rt: tokio::runtime::Handle,
    ) -> Result<Box<dyn MountSession>>;
}

/// Picks a provider: the one named in the config if given, otherwise the first
/// that probes successfully and supports the target.
pub fn select<'a>(
    providers: &'a [Arc<dyn MountProvider>],
    requested: Option<&str>,
    target: &MountTarget,
) -> Result<&'a Arc<dyn MountProvider>> {
    if let Some(name) = requested {
        let p = providers
            .iter()
            .find(|p| p.name() == name)
            .ok_or_else(|| Error::InvalidArgument(format!("unknown mount provider '{name}'")))?;
        p.probe()?;
        if !p.supports(target) {
            return Err(Error::InvalidArgument(format!(
                "provider '{name}' cannot mount {}",
                target.display()
            )));
        }
        return Ok(p);
    }
    let mut reasons = Vec::new();
    for p in providers.iter().filter(|p| p.supports(target)) {
        match p.probe() {
            Ok(()) => return Ok(p),
            Err(e) => reasons.push(format!("{}: {e}", p.name())),
        }
    }
    Err(Error::Unsupported(format!(
        "no usable mount provider for {} ({})",
        target.display(),
        if reasons.is_empty() {
            "none compiled in".to_string()
        } else {
            reasons.join("; ")
        }
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        name: &'static str,
        ok: bool,
        drives: bool,
    }

    impl MountProvider for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn probe(&self) -> Result<()> {
            if self.ok {
                Ok(())
            } else {
                Err(Error::Unsupported("driver missing".into()))
            }
        }
        fn supports(&self, target: &MountTarget) -> bool {
            match target {
                MountTarget::Drive(_) => self.drives,
                MountTarget::Path(_) => !self.drives,
            }
        }
        fn mount(
            &self,
            _spec: &MountSpec,
            _vfs: Arc<Vfs>,
            _rt: tokio::runtime::Handle,
        ) -> Result<Box<dyn MountSession>> {
            unimplemented!()
        }
    }

    fn providers() -> Vec<Arc<dyn MountProvider>> {
        vec![
            Arc::new(Fake {
                name: "broken",
                ok: false,
                drives: false,
            }),
            Arc::new(Fake {
                name: "nfs",
                ok: true,
                drives: false,
            }),
            Arc::new(Fake {
                name: "winfsp",
                ok: true,
                drives: true,
            }),
        ]
    }

    #[test]
    fn picks_first_working_provider_for_target() {
        let ps = providers();
        let p = select(&ps, None, &MountTarget::Path("/mnt/x".into())).unwrap();
        assert_eq!(p.name(), "nfs");
        let p = select(&ps, None, &MountTarget::Drive('Z')).unwrap();
        assert_eq!(p.name(), "winfsp");
    }

    #[test]
    fn explicit_choice_is_validated() {
        let ps = providers();
        assert!(select(&ps, Some("broken"), &MountTarget::Path("/x".into())).is_err());
        assert!(select(&ps, Some("nope"), &MountTarget::Path("/x".into())).is_err());
        // right provider, wrong target shape
        assert!(select(&ps, Some("winfsp"), &MountTarget::Path("/x".into())).is_err());
    }

    #[test]
    fn error_lists_why_each_failed() {
        let ps: Vec<Arc<dyn MountProvider>> = vec![Arc::new(Fake {
            name: "broken",
            ok: false,
            drives: false,
        })];
        let err = select(&ps, None, &MountTarget::Path("/x".into()))
            .err()
            .expect("no provider should work");
        assert!(err.to_string().contains("driver missing"), "{err}");
    }
}
