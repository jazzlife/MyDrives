//! The agent's half of self-update: when to look, and when it is safe to apply.
//!
//! Downloading is cheap and invisible, so it happens on a timer. *Applying* is
//! not: it replaces the running executables and restarts the agent, which tears
//! every mounted drive down for a second or two. So a staged update waits for a
//! moment when no file is open on any drive - and if that moment never comes,
//! the next start picks it up (see `apply_pending` in `main.rs`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use mydrives_core::config::UpdateConfig;
use mydrives_core::manager::MountManager;
use mydrives_core::update::{self, UpdateState, UpdateStatus};

/// How often the loop wakes up to re-evaluate (not how often it checks online).
const TICK: Duration = Duration::from_secs(60);

pub struct Updater {
    cfg: UpdateConfig,
    manager: Arc<MountManager>,
    status: Mutex<UpdateStatus>,
    wake: tokio::sync::Notify,
    /// The user pressed "지금 적용": apply even if a drive is busy.
    force_apply: AtomicBool,
}

impl Updater {
    pub fn new(cfg: UpdateConfig, manager: Arc<MountManager>) -> Arc<Self> {
        let state = if cfg.enabled {
            UpdateState::Idle
        } else {
            UpdateState::Disabled
        };
        let updater = Arc::new(Updater {
            cfg,
            manager,
            status: Mutex::new(UpdateStatus {
                state,
                ..UpdateStatus::default()
            }),
            wake: tokio::sync::Notify::new(),
            force_apply: AtomicBool::new(false),
        });
        // An update downloaded in an earlier run but never applied is still
        // waiting; say so instead of reporting "up to date".
        if let Some((version, _)) = update::staged() {
            if update::is_newer(&version, update::current_version()) {
                updater.set_state(UpdateState::Staged { version });
            }
        }
        updater
    }

    pub fn status(&self) -> UpdateStatus {
        self.status.lock().clone()
    }

    fn set_state(&self, state: UpdateState) {
        let mut status = self.status.lock();
        if let UpdateState::Staged { version } | UpdateState::Downloading { version } = &state {
            status.latest = Some(version.clone());
        }
        status.state = state;
    }

    /// Check now rather than at the next interval.
    pub fn request_check(&self) {
        self.wake.notify_one();
    }

    /// Apply a staged update at the next tick, busy drives or not.
    pub fn request_apply(&self) {
        self.force_apply.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    /// Background loop. Never returns; cancelled by the process exiting.
    pub async fn run(self: Arc<Self>) {
        if !self.cfg.enabled {
            tracing::info!("자동 업데이트가 꺼져 있습니다");
            return;
        }
        let interval = Duration::from_secs(self.cfg.check_interval_mins.max(5) * 60);
        // Not immediately at start-up: mounting the drives matters more, and a
        // release published seconds ago can wait a minute.
        let mut next_check = Instant::now() + Duration::from_secs(90);

        loop {
            tokio::select! {
                _ = tokio::time::sleep(TICK) => {}
                _ = self.wake.notified() => {
                    next_check = Instant::now();
                }
            }

            if Instant::now() >= next_check && !self.has_staged() {
                self.check_once().await;
                next_check = Instant::now() + interval;
            }
            self.maybe_apply().await;
        }
    }

    fn has_staged(&self) -> bool {
        matches!(self.status.lock().state, UpdateState::Staged { .. })
    }

    /// One round trip to GitHub, and the download if there is something new.
    async fn check_once(&self) {
        self.set_state(UpdateState::Checking);
        let release = match update::fetch_latest(&self.cfg).await {
            Ok(Some(release)) => release,
            Ok(None) => {
                self.set_state(UpdateState::UpToDate);
                return;
            }
            Err(e) => {
                // Offline laptops are the normal case, not an incident.
                tracing::debug!(error = %e, "업데이트 확인 실패");
                self.set_state(UpdateState::Failed {
                    reason: e.to_string(),
                });
                return;
            }
        };

        let version = release.version().to_string();
        self.status.lock().latest = Some(version.clone());
        if !update::is_newer(&version, update::current_version()) {
            self.set_state(UpdateState::UpToDate);
            return;
        }

        tracing::info!(version = %version, "새 버전을 내려받습니다");
        self.set_state(UpdateState::Downloading {
            version: version.clone(),
        });
        match update::stage(&self.cfg, &release, |name| {
            tracing::debug!(file = name, "내려받는 중");
        })
        .await
        {
            Ok(dir) => {
                tracing::info!(version = %version, path = %dir.display(), "업데이트 준비 완료");
                self.set_state(UpdateState::Staged { version });
            }
            Err(e) => {
                tracing::warn!(error = %e, "업데이트 내려받기 실패");
                self.set_state(UpdateState::Failed {
                    reason: e.to_string(),
                });
            }
        }
    }

    /// Applies a staged update when nothing is using the drives.
    async fn maybe_apply(&self) {
        let UpdateState::Staged { version } = self.status.lock().state.clone() else {
            return;
        };
        let forced = self.force_apply.swap(false, Ordering::SeqCst);
        if !forced && !self.drives_are_idle() {
            tracing::debug!("드라이브 사용 중 — 업데이트 적용을 미룹니다");
            return;
        }
        let Some((staged_version, dir)) = update::staged() else {
            // Someone removed the staging directory from under us.
            self.set_state(UpdateState::Idle);
            return;
        };
        if staged_version != version {
            self.set_state(UpdateState::Staged {
                version: staged_version,
            });
            return;
        }

        self.set_state(UpdateState::Applying {
            version: version.clone(),
        });
        let install = match update::install_dir() {
            Ok(dir) => dir,
            Err(e) => {
                self.set_state(UpdateState::Failed {
                    reason: e.to_string(),
                });
                return;
            }
        };

        // Unmount first: the drives go away with the restart either way, and
        // doing it in order leaves no stale mount points behind.
        self.manager.unmount_all().await;
        match update::apply(&dir, &install) {
            Ok(()) => {
                tracing::info!(version = %version, "업데이트 적용 — 에이전트를 다시 시작합니다");
                restart();
            }
            Err(e) => {
                tracing::error!(error = %e, "업데이트 적용 실패");
                self.set_state(UpdateState::Failed {
                    reason: e.to_string(),
                });
                // The drives were unmounted for an update that did not happen.
                self.manager.mount_automatics().await;
            }
        }
    }

    /// "Idle" means no process is holding a file open on any drive. Byte
    /// counters are not consulted: a drive that was read a second ago is fine
    /// to remount, an open file handle is not.
    fn drives_are_idle(&self) -> bool {
        self.manager.status().iter().all(|r| {
            !matches!(r.state, mydrives_core::manager::MountState::Mounting)
                && r.stats.map(|s| s.open_handles == 0).unwrap_or(true)
        })
    }
}

/// Starts the freshly installed agent and leaves.
///
/// Deliberately a plain spawn on both platforms rather than asking launchd (or
/// the `Run` key) to do it: those only act at login, and the handover has to be
/// deterministic - the new process must be up before this one lets go of the
/// control socket.
fn restart() -> ! {
    match std::env::current_exe() {
        Ok(exe) => {
            let mut command = std::process::Command::new(&exe);
            command.arg("run");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const DETACHED_PROCESS: u32 = 0x0000_0008;
                const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
                command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
            }
            if let Err(e) = command.spawn() {
                tracing::error!(error = %e, path = %exe.display(), "새 에이전트를 시작할 수 없습니다");
            }
        }
        Err(e) => tracing::error!(error = %e, "실행 파일 경로를 알 수 없습니다"),
    }
    // Exit code 0: launchd's KeepAlive is set to restart only on failure, so a
    // clean exit here must not race with the process we just started.
    std::process::exit(0);
}

/// Applies an update staged by an earlier run, before anything is mounted.
///
/// Returns `true` when the process has been replaced and the caller should do
/// nothing further (it is about to exit).
pub fn apply_pending() -> bool {
    let Ok(install) = update::install_dir() else {
        return false;
    };
    update::cleanup(&install);
    let Some((version, dir)) = update::staged() else {
        return false;
    };
    if !update::is_newer(&version, update::current_version()) {
        let _ = std::fs::remove_dir_all(&dir);
        return false;
    }
    match update::apply(&dir, &install) {
        Ok(()) => {
            tracing::info!(version = %version, "시작 전에 대기 중인 업데이트를 적용했습니다");
            restart();
        }
        Err(e) => {
            tracing::error!(error = %e, "대기 중인 업데이트 적용 실패");
            false
        }
    }
}
