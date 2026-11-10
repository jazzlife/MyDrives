//! Mount lifecycle: what the tray icon and the CLI actually drive.
//!
//! Owns one state machine per configured remote and a watchdog that notices when
//! the NAS went away (sleep/wake, Wi-Fi change, power cut) and brings the mount
//! back without the user having to re-add anything.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::backend::Backend;
use crate::config::{AppConfig, RemoteConfig};
use crate::error::{Error, Result};
use crate::mount::{self, MountProvider, MountSession, MountSpec, MountTarget};
use crate::path::RPath;
use crate::vfs::{Vfs, VfsOptions, VfsStats};

/// Builds a backend from configuration. Implemented by the daemon, which is the
/// only component that knows which backend crates are linked in.
pub trait BackendFactory: Send + Sync + 'static {
    fn build(&self, cfg: &RemoteConfig) -> Result<Arc<dyn Backend>>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum MountState {
    Unmounted,
    Mounting,
    Mounted { mountpoint: String, provider: String },
    /// Mounted, but the remote is currently unreachable; the watchdog is retrying.
    Offline { mountpoint: String, reason: String },
    Failed { reason: String },
}

impl MountState {
    pub fn is_live(&self) -> bool {
        matches!(self, MountState::Mounted { .. } | MountState::Offline { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteStatus {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub source: String,
    pub readonly: bool,
    pub state: MountState,
    pub uptime_secs: u64,
    pub stats: Option<StatsSnapshot>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct StatsSnapshot {
    pub reads: u64,
    pub writes: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub backend_calls: u64,
    pub cache_hits: u64,
    pub errors: u64,
    pub open_handles: u64,
}

impl From<VfsStats> for StatsSnapshot {
    fn from(s: VfsStats) -> Self {
        StatsSnapshot {
            reads: s.reads,
            writes: s.writes,
            bytes_read: s.bytes_read,
            bytes_written: s.bytes_written,
            backend_calls: s.backend_calls,
            cache_hits: s.cache_hits,
            errors: s.errors,
            open_handles: s.open_handles,
        }
    }
}

struct Remote {
    cfg: RemoteConfig,
    state: MountState,
    session: Option<Box<dyn MountSession>>,
    vfs: Option<Arc<Vfs>>,
    mounted_at: Option<Instant>,
    watchdog: Option<tokio::task::JoinHandle<()>>,
}

pub struct MountManager {
    rt: tokio::runtime::Handle,
    providers: Vec<Arc<dyn MountProvider>>,
    factory: Arc<dyn BackendFactory>,
    remotes: Mutex<HashMap<String, Remote>>,
    order: Mutex<Vec<String>>,
    probe_interval: Duration,
    mount_root: std::path::PathBuf,
    sudo_mount: bool,
}

impl MountManager {
    pub fn new(
        rt: tokio::runtime::Handle,
        providers: Vec<Arc<dyn MountProvider>>,
        factory: Arc<dyn BackendFactory>,
        config: &AppConfig,
    ) -> Arc<Self> {
        let mount_root = config
            .general
            .mount_root
            .as_ref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(crate::config::default_mount_root);
        let mgr = Arc::new(MountManager {
            rt,
            providers,
            factory,
            remotes: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            probe_interval: Duration::from_secs(config.general.probe_secs.max(5)),
            mount_root,
            sudo_mount: config.general.sudo_mount,
        });
        for cfg in &config.remotes {
            mgr.put(cfg.clone());
        }
        mgr
    }

    pub fn providers(&self) -> Vec<(&'static str, Option<String>)> {
        self.providers
            .iter()
            .map(|p| (p.name(), p.probe().err().map(|e| e.to_string())))
            .collect()
    }

    /// Adds or replaces a remote definition (does not mount it).
    pub fn put(&self, cfg: RemoteConfig) {
        let id = cfg.id.clone();
        let mut remotes = self.remotes.lock();
        match remotes.get_mut(&id) {
            Some(existing) if existing.state.is_live() => {
                existing.cfg = cfg;
            }
            _ => {
                remotes.insert(
                    id.clone(),
                    Remote {
                        cfg,
                        state: MountState::Unmounted,
                        session: None,
                        vfs: None,
                        mounted_at: None,
                        watchdog: None,
                    },
                );
                let mut order = self.order.lock();
                if !order.contains(&id) {
                    order.push(id);
                }
            }
        }
    }

    pub fn forget(&self, id: &str) -> Result<()> {
        if self.remotes.lock().get(id).map(|r| r.state.is_live()) == Some(true) {
            return Err(Error::InvalidArgument(format!(
                "{id} is still mounted; unmount it first"
            )));
        }
        self.remotes.lock().remove(id);
        self.order.lock().retain(|x| x != id);
        Ok(())
    }

    pub fn config_of(&self, id: &str) -> Option<RemoteConfig> {
        self.remotes.lock().get(id).map(|r| r.cfg.clone())
    }

    pub fn vfs_of(&self, id: &str) -> Option<Arc<Vfs>> {
        self.remotes.lock().get(id).and_then(|r| r.vfs.clone())
    }

    pub fn status(&self) -> Vec<RemoteStatus> {
        let order = self.order.lock().clone();
        let remotes = self.remotes.lock();
        order
            .iter()
            .filter_map(|id| remotes.get(id))
            .map(|r| RemoteStatus {
                id: r.cfg.id.clone(),
                label: r.cfg.display_label().to_string(),
                kind: r.cfg.backend.kind().to_string(),
                source: r
                    .vfs
                    .as_ref()
                    .map(|v| v.backend().describe())
                    .unwrap_or_else(|| describe_config(&r.cfg)),
                readonly: r.cfg.readonly,
                state: r.state.clone(),
                uptime_secs: r
                    .mounted_at
                    .map(|t| t.elapsed().as_secs())
                    .unwrap_or_default(),
                stats: r.vfs.as_ref().map(|v| v.stats().into()),
            })
            .collect()
    }

    /// Resolves where a remote should appear on this OS.
    pub fn target_for(&self, cfg: &RemoteConfig) -> Result<MountTarget> {
        if let Some(drive) = &cfg.drive {
            let letter = drive
                .chars()
                .next()
                .filter(|c| c.is_ascii_alphabetic())
                .ok_or_else(|| Error::InvalidArgument(format!("bad drive letter '{drive}'")))?;
            if cfg!(windows) {
                return Ok(MountTarget::Drive(letter.to_ascii_uppercase()));
            }
            tracing::debug!(
                remote = %cfg.id,
                "drive letters only apply to Windows; using a directory mount"
            );
        }
        let path = match &cfg.mountpoint {
            Some(p) => expand_tilde(p),
            None => self.mount_root.join(&cfg.id),
        };
        Ok(MountTarget::Path(path))
    }

    pub async fn mount(self: &Arc<Self>, id: &str) -> Result<String> {
        let cfg = self
            .config_of(id)
            .ok_or_else(|| Error::NotFound(format!("remote '{id}'")))?;

        if let Some(r) = self.remotes.lock().get(id) {
            if let MountState::Mounted { mountpoint, .. } = &r.state {
                return Ok(mountpoint.clone());
            }
        }
        self.set_state(id, MountState::Mounting);

        let result = self.mount_inner(&cfg).await;
        match result {
            Ok((mountpoint, provider, session, vfs)) => {
                {
                    let mut remotes = self.remotes.lock();
                    if let Some(r) = remotes.get_mut(id) {
                        r.session = Some(session);
                        r.vfs = Some(vfs);
                        r.mounted_at = Some(Instant::now());
                        r.state = MountState::Mounted {
                            mountpoint: mountpoint.clone(),
                            provider: provider.to_string(),
                        };
                    }
                }
                self.spawn_watchdog(id, &mountpoint);
                tracing::info!(remote = id, mountpoint = %mountpoint, provider, "mounted");
                Ok(mountpoint)
            }
            Err(e) => {
                self.set_state(
                    id,
                    MountState::Failed {
                        reason: e.to_string(),
                    },
                );
                Err(e)
            }
        }
    }

    async fn mount_inner(
        &self,
        cfg: &RemoteConfig,
    ) -> Result<(String, &'static str, Box<dyn MountSession>, Arc<Vfs>)> {
        let backend = self.factory.build(cfg)?;
        backend.connect().await?;
        // Fail early and with a clear message rather than mounting an empty drive.
        backend.stat(&RPath::root()).await.map_err(|e| {
            Error::Backend(format!("cannot read the root of {}: {e}", backend.describe()))
        })?;

        let target = self.target_for(cfg)?;
        if let MountTarget::Path(path) = &target {
            // Crash recovery: if the agent was killed (or the machine lost power)
            // the kernel still has the old mount, and mounting over it would
            // either fail or stack a second mount on top. The only thing that can
            // be mounted at our own mount point is a previous instance of us.
            #[cfg(unix)]
            if is_mount_point(path) {
                tracing::warn!(
                    path = %path.display(),
                    "a stale mount is still here from an earlier run; unmounting it"
                );
                let _ = std::process::Command::new("umount")
                    .arg(if cfg!(target_os = "macos") { "-f" } else { "-l" })
                    .arg(path)
                    .output();
                // macOS removes the directory it made for the old volume a
                // moment *after* the unmount - which is after we would have
                // recreated it. The new mount then has no mount point and
                // fails ("exit status 66" out of mount_smbfs), so the tidying
                // up has to finish first.
                for _ in 0..20 {
                    if !is_mount_point(path) && !path.exists() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
            create_mount_point(path, self.sudo_mount)?;
        }
        let provider = mount::select(&self.providers, cfg.provider.as_deref(), &target)?;

        let vfs = Vfs::new(
            backend.clone(),
            VfsOptions {
                readonly: cfg.readonly,
                ..Default::default()
            },
        );
        let spec = MountSpec {
            id: cfg.id.clone(),
            label: cfg.display_label().to_string(),
            target: target.clone(),
            readonly: cfg.readonly,
            volume_name: cfg.display_label().to_string(),
            source: backend.describe(),
            sudo_mount: sudo_allowed(self.sudo_mount),
        };

        // Mounting blocks: FUSE waits for the kernel handshake, WinFsp waits for
        // its dispatcher thread, the NFS provider runs `mount(8)`. Doing that on a
        // runtime worker would stall other remotes - and `Handle::block_on` inside
        // a runtime thread panics outright.
        let session = {
            let provider = provider.clone();
            let spec = spec.clone();
            let vfs = vfs.clone();
            let rt = self.rt.clone();
            tokio::task::spawn_blocking(move || provider.mount(&spec, vfs, rt))
                .await
                .map_err(|e| Error::Backend(format!("the mount task failed: {e}")))??
        };
        Ok((session.mountpoint(), provider.name(), session, vfs))
    }

    pub async fn unmount(&self, id: &str) -> Result<()> {
        let (session, vfs, watchdog) = {
            let mut remotes = self.remotes.lock();
            let r = remotes
                .get_mut(id)
                .ok_or_else(|| Error::NotFound(format!("remote '{id}'")))?;
            (r.session.take(), r.vfs.take(), r.watchdog.take())
        };
        if let Some(w) = watchdog {
            w.abort();
        }
        if let Some(vfs) = &vfs {
            vfs.release_all().await;
        }
        let mut result = Ok(());
        if let Some(session) = session {
            result = session.unmount();
        }
        if let Some(vfs) = vfs {
            if let Err(e) = vfs.backend().disconnect().await {
                tracing::warn!(remote = id, error = %e, "backend disconnect failed");
            }
        }
        {
            let mut remotes = self.remotes.lock();
            if let Some(r) = remotes.get_mut(id) {
                r.state = MountState::Unmounted;
                r.mounted_at = None;
            }
        }

        // Remove the (now empty) mount point.
        //
        // Otherwise the directory stays behind as an ordinary local folder, and
        // anything written to that path after an unmount silently lands on the
        // local disk instead of the NAS - the user believes the file is on the
        // share, and it is not. Only empty directories are removed, so a path the
        // user picked and filled themselves is left alone.
        if let Some(cfg) = self.config_of(id) {
            if let Ok(MountTarget::Path(path)) = self.target_for(&cfg) {
                if let Ok(mut entries) = std::fs::read_dir(&path) {
                    if entries.next().is_none() {
                        remove_mount_point(&path, self.sudo_mount);
                    }
                }
            }
        }

        tracing::info!(remote = id, "unmounted");
        result
    }

    pub async fn mount_automatics(self: &Arc<Self>) {
        let ids: Vec<String> = {
            let remotes = self.remotes.lock();
            self.order
                .lock()
                .iter()
                .filter(|id| remotes.get(*id).map(|r| r.cfg.automount) == Some(true))
                .cloned()
                .collect()
        };
        for id in ids {
            if let Err(e) = self.mount(&id).await {
                tracing::warn!(remote = %id, error = %e, "automount failed");
            }
        }
    }

    pub async fn unmount_all(&self) {
        let ids: Vec<String> = self
            .remotes
            .lock()
            .iter()
            .filter(|(_, r)| r.state.is_live())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Err(e) = self.unmount(&id).await {
                tracing::warn!(remote = %id, error = %e, "unmount failed");
            }
        }
    }

    fn set_state(&self, id: &str, state: MountState) {
        if let Some(r) = self.remotes.lock().get_mut(id) {
            r.state = state;
        }
    }

    /// Probes the remote periodically. A network filesystem that silently hangs is
    /// worse than one that reports itself offline, so the state is published and
    /// reconnects are attempted with backoff.
    fn spawn_watchdog(self: &Arc<Self>, id: &str, mountpoint: &str) {
        let weak: Weak<MountManager> = Arc::downgrade(self);
        let owner_id = id.to_string();
        let id = owner_id.clone();
        let mountpoint = mountpoint.to_string();
        let interval = self.probe_interval;
        let handle = self.rt.spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                tokio::time::sleep(interval).await;
                let Some(mgr) = weak.upgrade() else { return };
                let Some(vfs) = mgr.vfs_of(&id) else { return };

                match vfs.backend().stat(&RPath::root()).await {
                    Ok(_) => {
                        backoff = Duration::from_secs(1);
                        let mut remotes = mgr.remotes.lock();
                        if let Some(r) = remotes.get_mut(&id) {
                            if let MountState::Offline { .. } = r.state {
                                tracing::info!(remote = %id, "remote is back");
                                r.state = MountState::Mounted {
                                    mountpoint: mountpoint.clone(),
                                    provider: r
                                        .session
                                        .as_ref()
                                        .map(|_| "reconnected".to_string())
                                        .unwrap_or_default(),
                                };
                                drop(remotes);
                                vfs.invalidate_all();
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(remote = %id, error = %e, "probe failed; reconnecting");
                        {
                            let mut remotes = mgr.remotes.lock();
                            if let Some(r) = remotes.get_mut(&id) {
                                r.state = MountState::Offline {
                                    mountpoint: mountpoint.clone(),
                                    reason: e.to_string(),
                                };
                            }
                        }
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(60));
                        if let Err(e) = vfs.backend().connect().await {
                            tracing::warn!(remote = %id, error = %e, "reconnect failed");
                        } else {
                            vfs.invalidate_all();
                        }
                    }
                }
            }
        });
        if let Some(r) = self.remotes.lock().get_mut(&owner_id) {
            r.watchdog = Some(handle);
        }
    }
}

/// May the agent reach for `sudo -n` to make a mount point?
///
/// The setting lives in the config file because the agent is started four
/// different ways - launchd, Finder, the installer, its own updater - and only
/// one of them passes an environment along. `MYDRIVES_SUDO_MOUNT` stays as an
/// override for a developer running it from a shell.
fn sudo_allowed(sudo_mount: bool) -> bool {
    match std::env::var("MYDRIVES_SUDO_MOUNT") {
        Ok(v) => v == "1",
        Err(_) => sudo_mount,
    }
}

/// Creates the mount point, escalating only where the OS requires it.
///
/// `/Volumes` on macOS is root-owned, and that is exactly where a mount has to
/// live for Finder to treat it as a **volume** (sidebar entry, eject button,
/// desktop icon) instead of a folder that happens to be mounted. Everywhere else
/// an ordinary mkdir is enough and no privileges are involved.
fn create_mount_point(path: &std::path::Path, sudo_mount: bool) -> Result<()> {
    match std::fs::create_dir_all(path) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() != std::io::ErrorKind::PermissionDenied => {
            return Err(Error::Backend(format!(
                "cannot create mount point {}: {e}",
                path.display()
            )))
        }
        Err(e) => {
            if !sudo_allowed(sudo_mount) {
                return Err(Error::PermissionDenied(format!(
                    "{} 을(를) 만들 수 없습니다: {e}. /Volumes 아래에 마운트하려면 권한이 필요합니다 \
                     (설정의 [general] sudo_mount = true 로 sudo 허용, 또는 홈 폴더 아래 경로를 쓰세요)",
                    path.display()
                )));
            }
        }
    }

    #[cfg(unix)]
    {
        let user = std::env::var("USER").unwrap_or_default();
        let made = std::process::Command::new("sudo")
            .args(["-n", "mkdir", "-p"])
            .arg(path)
            .output();
        match made {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                return Err(Error::PermissionDenied(format!(
                    "sudo mkdir {} 실패: {}",
                    path.display(),
                    String::from_utf8_lossy(&o.stderr).trim()
                )))
            }
            Err(e) => return Err(Error::Backend(format!("sudo 실행 실패: {e}"))),
        }
        // The mount itself runs as the user, so the directory has to be theirs.
        if !user.is_empty() {
            let _ = std::process::Command::new("sudo")
                .args(["-n", "chown", &user])
                .arg(path)
                .output();
        }
    }
    Ok(())
}

/// Removes an empty mount point, with the same privilege rules as creating it.
fn remove_mount_point(path: &std::path::Path, sudo_mount: bool) {
    if std::fs::remove_dir(path).is_ok() {
        return;
    }
    #[cfg(unix)]
    if sudo_allowed(sudo_mount) {
        let _ = std::process::Command::new("sudo")
            .args(["-n", "rmdir"])
            .arg(path)
            .output();
    }
}

/// True when `path` is a mount point: its device differs from its parent's.
#[cfg(unix)]
fn is_mount_point(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = path.parent() else {
        return false;
    };
    match (std::fs::metadata(path), std::fs::metadata(parent)) {
        (Ok(here), Ok(up)) => here.dev() != up.dev(),
        // A mount whose server is gone makes stat() fail; that is a stale mount
        // too, and worth trying to clear.
        (Err(e), Ok(_)) => e.kind() != std::io::ErrorKind::NotFound,
        _ => false,
    }
}

fn describe_config(cfg: &RemoteConfig) -> String {
    match &cfg.backend {
        crate::config::BackendConfig::Smb(s) => {
            format!("smb://{}:{}/{}", s.host, s.port, s.share)
        }
        crate::config::BackendConfig::Ftp(f) => {
            let scheme = if f.security.eq_ignore_ascii_case("plain") {
                "ftp"
            } else {
                "ftps"
            };
            format!(
                "{scheme}://{}:{}{}",
                f.host,
                f.port,
                f.subpath.as_deref().unwrap_or("")
            )
        }
        crate::config::BackendConfig::Mem(_) => "mem://".to_string(),
    }
}

fn expand_tilde(p: &str) -> std::path::PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(dirs) = directories::UserDirs::new() {
            return dirs.home_dir().join(rest);
        }
    }
    std::path::PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sudo_is_off_until_the_config_asks_for_it() {
        // The environment override is a developer's, and tests share one
        // process, so this only covers the configured answer.
        if std::env::var_os("MYDRIVES_SUDO_MOUNT").is_some() {
            return;
        }
        assert!(!sudo_allowed(false));
        assert!(sudo_allowed(true));
        assert!(!AppConfig::default().general.sudo_mount);
    }

    #[test]
    fn a_drive_with_no_mount_point_of_its_own_lands_under_the_root() {
        let mut config = AppConfig::default();
        config.general.mount_root = Some("/tmp/mydrives-test-root".into());
        let mgr = MountManager::new(
            tokio::runtime::Handle::try_current()
                .unwrap_or_else(|_| TEST_RT.handle().clone()),
            Vec::new(),
            Arc::new(NoFactory),
            &config,
        );
        let cfg = RemoteConfig {
            id: "nas".into(),
            label: String::new(),
            automount: false,
            readonly: false,
            provider: None,
            drive: None,
            mountpoint: None,
            backend: crate::config::BackendConfig::Mem(crate::config::MemConfig { fixture: true }),
        };
        match mgr.target_for(&cfg).unwrap() {
            MountTarget::Path(p) => assert_eq!(p, std::path::Path::new("/tmp/mydrives-test-root/nas")),
            other => panic!("expected a path, got {other:?}"),
        }
    }

    struct NoFactory;
    impl BackendFactory for NoFactory {
        fn build(&self, _cfg: &RemoteConfig) -> Result<Arc<dyn Backend>> {
            Err(Error::Unsupported("test".into()))
        }
    }

    static TEST_RT: std::sync::LazyLock<tokio::runtime::Runtime> =
        std::sync::LazyLock::new(|| tokio::runtime::Runtime::new().expect("test runtime"));
}
