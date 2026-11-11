//! `mydrivesd` - the resident agent.
//!
//! Runs in the **user's session**, not as a system service. That is a deliberate
//! design constraint, not laziness: on Windows a drive letter belongs to a logon
//! session, so a mount created by a LocalSystem service in session 0 is invisible
//! in the user's Explorer. On macOS the same applies to a LaunchDaemon versus a
//! LaunchAgent, and only an agent may show a menu bar item.
//!
//! The process owns: the config file, the mount manager, the control socket and
//! (with the `tray` feature) the tray/menu bar UI.

mod factory;
mod updater;
#[cfg(feature = "tray")]
mod tray;

use std::sync::Arc;

use clap::{Parser, Subcommand};
use parking_lot::Mutex;

use mydrives_core::config::{AppConfig, BackendConfig, MemConfig, RemoteConfig, SecretRef, SmbConfig};
use mydrives_core::manager::MountManager;
use mydrives_core::mount::MountProvider;
use mydrives_core::{autostart, secret, Vfs};
use mydrives_ipc::{ProviderInfo, Request, Response};

#[derive(Parser)]
#[command(
    name = "mydrivesd",
    about = "MyDrives agent: mounts remote storage as local drives",
    version
)]
struct Cli {
    /// Log filter, e.g. `info`, `mydrives_core=debug`.
    #[arg(long, global = true, default_value = "info")]
    log: String,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the agent (default).
    Run {
        /// Do not show the tray / menu bar item.
        #[arg(long)]
        no_tray: bool,
        /// Do not mount anything at start-up.
        #[arg(long)]
        no_automount: bool,
    },
    /// Mount one remote in the foreground and unmount on Ctrl-C. Used for
    /// development and for the end-to-end tests; needs no config file.
    DevMount {
        #[arg(long, default_value = "dev")]
        id: String,
        /// `smb` or `mem`.
        #[arg(long, default_value = "smb")]
        kind: String,
        #[arg(long)]
        mountpoint: Option<String>,
        #[arg(long)]
        drive: Option<String>,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        readonly: bool,

        #[arg(long)]
        host: Option<String>,
        #[arg(long, default_value_t = 445)]
        port: u16,
        #[arg(long)]
        share: Option<String>,
        #[arg(long, default_value = "")]
        user: String,
        /// Password. Prefer `--password-env` so it does not show up in `ps`.
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        password_env: Option<String>,
        #[arg(long)]
        subpath: Option<String>,
    },
    /// Print the mount providers this build has and whether they are usable.
    Providers,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // A console window on the desktop is the normal price of starting a console
    // binary from the Windows `Run` key, and it is unacceptable for a tray
    // agent: the user would log in to a stray black window that quits the agent
    // when closed. Detached only when we are the console's sole owner (so
    // running `mydrivesd providers` in a terminal still prints normally), and
    // the log then goes to a file, since stdout no longer goes anywhere.
    let detached_log = mydrives_core::console::detach_if_sole_owner("agent.log");
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| cli.log.clone().into());
    match detached_log {
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        None => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .init(),
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("mydrives")
        .build()?;

    match cli.command.unwrap_or(Command::Run {
        no_tray: false,
        no_automount: false,
    }) {
        Command::Run {
            no_tray,
            no_automount,
        } => run_agent(runtime, no_tray, no_automount),
        Command::DevMount {
            id,
            kind,
            mountpoint,
            drive,
            provider,
            readonly,
            host,
            port,
            share,
            user,
            password,
            password_env,
            subpath,
        } => {
            let backend = match kind.as_str() {
                "mem" => BackendConfig::Mem(MemConfig { fixture: true }),
                "smb" => {
                    let password = password
                        .or_else(|| password_env.and_then(|k| std::env::var(k).ok()))
                        .unwrap_or_default();
                    BackendConfig::Smb(SmbConfig {
                        host: host.ok_or_else(|| anyhow::anyhow!("--host is required"))?,
                        port,
                        share: share.ok_or_else(|| anyhow::anyhow!("--share is required"))?,
                        user,
                        subpath,
                        secret: SecretRef::Inline { value: password },
                    })
                }
                other => anyhow::bail!("unknown backend kind '{other}'"),
            };
            let cfg = RemoteConfig {
                id,
                label: "dev mount".into(),
                automount: true,
                readonly,
                provider,
                drive,
                mountpoint,
                backend,
            };
            dev_mount(runtime, cfg)
        }
        Command::Providers => {
            for p in providers() {
                match p.probe() {
                    Ok(()) => println!("{:<8} available", p.name()),
                    Err(e) => println!("{:<8} unavailable: {e}", p.name()),
                }
            }
            Ok(())
        }
    }
}

/// Replaces names the agent generated itself with the product's name.
///
/// Older versions labelled a drive `host/share`, which reads like a path and is
/// the same for every share on one NAS. Names the user typed are left alone -
/// [`naming::is_generated_label`] only recognises what we wrote.
fn rename_generated_labels(config: &Arc<Mutex<AppConfig>>) {
    use mydrives_core::naming;

    let mut cfg = config.lock();
    let mut taken: Vec<String> = cfg
        .remotes
        .iter()
        .filter(|r| !naming::is_generated_label(&r.label, &r.backend))
        .map(|r| r.label.clone())
        .collect();
    let mut changed = false;
    for remote in &mut cfg.remotes {
        if !naming::is_generated_label(&remote.label, &remote.backend) {
            continue;
        }
        let share = match &remote.backend {
            mydrives_core::config::BackendConfig::Smb(smb) => smb.share.clone(),
            _ => remote.id.clone(),
        };
        let label = naming::unique_label(&naming::product_name(&remote.backend), &share, &taken);
        tracing::info!(remote = %remote.id, from = %remote.label, to = %label, "이름을 제품 이름으로 바꿨습니다");
        remote.label = label.clone();
        taken.push(label);
        changed = true;
    }
    if changed {
        if let Err(e) = cfg.save() {
            tracing::warn!(error = %e, "바뀐 이름을 저장하지 못했습니다");
        }
    }
}

/// Mount layers this build can use, in preference order.
fn providers() -> Vec<Arc<dyn MountProvider>> {
    let mut out: Vec<Arc<dyn MountProvider>> = Vec::new();
    #[cfg(windows)]
    out.push(Arc::new(mydrives_mount_windows::WinFspProvider::new()));
    // macOS prefers FUSE-T: it needs no kernel extension either, and it is the
    // one that reaches Finder's sidebar, where a drive belongs. Its `probe`
    // fails when FUSE-T is not installed, and then the in-box NFS client takes
    // over - a working drive, just not one Finder lists. FUSE is preferred on
    // Linux too, where it is native.
    #[cfg(target_os = "macos")]
    {
        #[cfg(feature = "macfuse")]
        out.push(Arc::new(mydrives_mount_fuse::FuseProvider::new()));
        out.push(Arc::new(mydrives_mount_nfs::NfsProvider::new()));
    }
    #[cfg(target_os = "linux")]
    {
        out.push(Arc::new(mydrives_mount_fuse::FuseProvider::new()));
        out.push(Arc::new(mydrives_mount_nfs::NfsProvider::new()));
    }
    out
}

fn run_agent(
    runtime: tokio::runtime::Runtime,
    no_tray: bool,
    no_automount: bool,
) -> anyhow::Result<()> {
    // Before anything else: an update downloaded in an earlier run is applied
    // now, while no drive is mounted and nothing holds the control socket.
    // This call does not return if it succeeds - the process is replaced.
    updater::apply_pending();

    let config = Arc::new(Mutex::new(AppConfig::load()?));
    rename_generated_labels(&config);
    let manager = MountManager::new(
        runtime.handle().clone(),
        providers(),
        Arc::new(factory::Factory),
        &config.lock().clone(),
    );

    // Automount runs in the background on purpose. Doing it inline would mean a
    // slow NAS - or a macOS Keychain prompt waiting for a click - keeps the
    // control socket and the tray from ever coming up, so the agent looks dead.
    if !no_automount {
        let mgr = manager.clone();
        let cfg = config.clone();
        runtime.spawn(async move {
            ensure_mount_layer(&cfg, None).await;
            mgr.mount_automatics().await
        });
    }

    // Register (or refresh) the login item. Done on every start rather than
    // once at install time: the registration names an absolute path, and a
    // rebuilt or moved agent would otherwise leave a login item pointing at a
    // binary that is gone. Never fatal - a drive agent that will not start
    // because it could not write a registry value is worse than one that is not
    // registered.
    if config.lock().general.autostart {
        match autostart::enable() {
            Ok(autostart::Status::Enabled { detail }) => {
                tracing::info!(detail = %detail, "autostart registered")
            }
            Ok(autostart::Status::Disabled) => {}
            Err(e) => tracing::warn!(error = %e, "registering autostart failed"),
        }
    }

    // Self-update: looks for new releases on a timer, downloads them quietly and
    // applies them when no drive is in use.
    let updates = updater::Updater::new(config.lock().update.clone(), manager.clone());
    runtime.spawn(updates.clone().run());

    // Signalled by a `mydrives shutdown` over the control socket; `notify_one`
    // (not `notify_waiters`) so a shutdown that arrives before the main thread
    // starts waiting is not lost.
    let shutdown = Arc::new(tokio::sync::Notify::new());

    // Control socket: the CLI and the tray talk to the agent through it.
    {
        let manager = manager.clone();
        let config = config.clone();
        let shutdown = shutdown.clone();
        let updates = updates.clone();
        runtime.spawn(async move {
            let result = mydrives_ipc::serve(move |req| {
                let manager = manager.clone();
                let config = config.clone();
                let shutdown = shutdown.clone();
                let updates = updates.clone();
                async move { handle(req, manager, config, shutdown, updates).await }
            })
            .await;
            if let Err(e) = result {
                tracing::error!(error = %e, "control socket failed");
            }
        });
    }

    tracing::info!(
        config = %mydrives_core::config::config_path().display(),
        endpoint = %mydrives_ipc::endpoint(),
        "agent started"
    );

    #[cfg(feature = "tray")]
    if !no_tray && config.lock().general.show_tray {
        // Windows 11 hides a newly registered tray icon in the overflow by
        // default (HKCU\Control Panel\NotifyIconSettings, IsPromoted unset), so a
        // first-time user would see nothing at all. Showing the window when there
        // is nothing configured yet means the app is always visibly there.
        if config.lock().remotes.is_empty() {
            tray::open_settings_window_args(&["--add"]);
        }
        // launchd stops the agent with SIGTERM - `launchctl bootout`, logging
        // out, an update replacing it. The event loop below never hears a
        // signal, so without this the agent sat until launchd killed it, with
        // its mounts still up and its FUSE-T helpers left running; the next
        // start then found its own ghosts holding the drives' names. Measured.
        {
            let shutdown = shutdown.clone();
            runtime.spawn(async move {
                wait_for_shutdown(None).await;
                shutdown.notify_one();
            });
        }
        // The tray owns the main thread: both AppKit and the Win32 message pump
        // require their event loop to run there.
        let r = tray::run(
            manager.clone(),
            runtime.handle().clone(),
            config.clone(),
            shutdown.clone(),
            updates.clone(),
        );
        runtime.block_on(async { manager.unmount_all().await });
        return r;
    }

    let _ = no_tray;
    runtime.block_on(async {
        wait_for_shutdown(Some(shutdown)).await;
        manager.unmount_all().await;
    });
    Ok(())
}

/// Blocks until the OS asks us to stop or a `mydrives shutdown` arrives.
async fn wait_for_shutdown(ipc: Option<Arc<tokio::sync::Notify>>) {
    let ipc_shutdown = async {
        match ipc {
            Some(n) => n.notified().await,
            // Never completes: the dev-mount path has no control socket.
            None => std::future::pending().await,
        }
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut hup = signal(SignalKind::hangup()).expect("SIGHUP handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
            _ = hup.recv() => {}
            _ = ipc_shutdown => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = ipc_shutdown => {}
        }
    }
    tracing::info!("shutting down; unmounting everything");
}

/// Puts a password the user just typed into a throwaway config.
///
/// "Test" and "list shares" run against settings that have not been saved yet,
/// so there is nothing in the keychain to read: the secret has to travel with
/// the request. Every backend that takes a password needs an arm here - the
/// first version of this only had SMB, which made "연결 테스트" on a new FTP
/// drive log in with an empty password and report the server's rejection as if
/// the settings were wrong.
fn inline_secret(backend: &mut mydrives_core::config::BackendConfig, secret: String) {
    use mydrives_core::config::BackendConfig;
    match backend {
        BackendConfig::Smb(smb) => smb.secret = SecretRef::Inline { value: secret },
        BackendConfig::Ftp(ftp) => ftp.secret = SecretRef::Inline { value: secret },
        BackendConfig::Mem(_) => {}
    }
}

async fn handle(
    req: Request,
    manager: Arc<MountManager>,
    config: Arc<Mutex<AppConfig>>,
    shutdown: Arc<tokio::sync::Notify>,
    updates: Arc<updater::Updater>,
) -> (Response, bool) {
    match req {
        Request::Ping => (
            Response::Pong {
                version: mydrives_ipc::PROTOCOL_VERSION,
                agent: env!("CARGO_PKG_VERSION").to_string(),
            },
            false,
        ),
        Request::Status => (
            Response::Status {
                remotes: manager.status(),
            },
            false,
        ),
        Request::Describe { id } => match manager.config_of(&id) {
            // The config's SecretRef names the keychain, it is not the
            // password, so sending it back is safe.
            Some(remote) => (Response::Described { remote }, false),
            None => (
                Response::Error {
                    message: format!("'{id}' 드라이브가 없습니다"),
                },
                false,
            ),
        },
        Request::Providers => (
            Response::Providers {
                providers: manager
                    .providers()
                    .into_iter()
                    .map(|(name, reason)| ProviderInfo {
                        name: name.to_string(),
                        unavailable_reason: reason,
                    })
                    .collect(),
            },
            false,
        ),
        Request::Add {
            mut remote,
            secret: s,
            replace,
        } => {
            // The id is the config key, the keychain account and the mount
            // folder all at once, so adding a drive under a name another drive
            // already uses replaces it - settings, password and all. That is
            // never what "add" means: a drive pointing somewhere else is
            // refused here rather than quietly swallowing the first one. An
            // edit (`replace`) is the one case where pointing the same id
            // somewhere new is exactly what was asked.
            if replace && manager.config_of(&remote.id).is_none() {
                return (
                    Response::Error {
                        message: format!("'{}' 드라이브가 없습니다", remote.id),
                    },
                    false,
                );
            }
            if replace
                && manager
                    .status()
                    .iter()
                    .any(|r| r.id == remote.id && r.state.is_live())
            {
                return (
                    Response::Error {
                        message: "연결된 드라이브는 고칠 수 없습니다 - 먼저 연결을 해제하세요".into(),
                    },
                    false,
                );
            }
            let existing = config
                .lock()
                .remotes
                .iter()
                .find(|r| r.id == remote.id)
                .cloned()
                .filter(|_| !replace);
            if let Some(existing) = existing {
                if !mydrives_core::naming::same_target(&existing.backend, &remote.backend) {
                    return (
                        Response::Error {
                            message: format!(
                                "'{}' 이름은 이미 '{}' 드라이브가 쓰고 있습니다 — 다른 이름을 쓰거나 그 드라이브를 먼저 지우세요",
                                remote.id,
                                existing.display_label()
                            ),
                        },
                        false,
                    );
                }
            }
            // A caller that did not pick a name gets the product's: the NAS, or
            // the service. Done here rather than in the CLI or the window
            // because only the agent knows which names are already taken.
            if remote.label.trim().is_empty() {
                let taken: Vec<String> = config
                    .lock()
                    .remotes
                    .iter()
                    .filter(|r| r.id != remote.id)
                    .map(|r| r.display_label().to_string())
                    .collect();
                let share = match &remote.backend {
                    mydrives_core::config::BackendConfig::Smb(smb) => smb.share.clone(),
                    _ => remote.id.clone(),
                };
                remote.label = mydrives_core::naming::unique_label(
                    &mydrives_core::naming::product_name(&remote.backend),
                    &share,
                    &taken,
                );
            }
            let id = remote.id.clone();
            // A drive moved to another folder gets a new sidebar row when it
            // mounts; the old one would otherwise stay, pointing nowhere.
            if let Some(previous) = manager.config_of(&id) {
                let before = manager.mount_path_of(&previous);
                if before != manager.mount_path_of(&remote) {
                    if let Err(e) = mydrives_core::finder::unpin_volume(&before) {
                        tracing::warn!(remote = %id, "could not take the old folder out of Finder's sidebar: {e}");
                    }
                }
            }
            if let Some(s) = s {
                if let Err(e) = secret::store(&id, &s) {
                    return (
                        Response::Error {
                            message: format!("storing the secret failed: {e}"),
                        },
                        false,
                    );
                }
            }
            manager.put(remote.clone());
            let saved = {
                let mut cfg = config.lock();
                cfg.upsert(remote);
                cfg.save()
            };
            match saved {
                Ok(()) => (Response::Ok, false),
                Err(e) => (
                    Response::Error {
                        message: format!("saving the config failed: {e}"),
                    },
                    false,
                ),
            }
        }
        Request::Test { remote, secret } => {
            // Build a throwaway backend with the secret supplied inline, so a
            // test never touches the keychain or the config file.
            let mut probe = remote.clone();
            if let Some(s) = secret {
                inline_secret(&mut probe.backend, s);
            }
            let factory = factory::Factory;
            let result = async {
                let backend = mydrives_core::manager::BackendFactory::build(&factory, &probe)?;
                backend.connect().await?;
                let root = mydrives_core::RPath::root();
                let entries = backend.readdir(&root).await?;
                let stat = backend.statfs().await.ok();
                let _ = backend.disconnect().await;
                Ok::<_, mydrives_core::Error>((backend.describe(), entries.len(), stat))
            }
            .await;
            match result {
                Ok((source, count, stat)) => {
                    let space = stat
                        .map(|s| {
                            format!(
                                ", 여유 공간 {:.1} GiB / {:.1} GiB",
                                s.free_bytes as f64 / (1u64 << 30) as f64,
                                s.total_bytes as f64 / (1u64 << 30) as f64
                            )
                        })
                        .unwrap_or_default();
                    (
                        Response::Tested {
                            summary: format!("{source} 연결 성공 — 최상위 항목 {count}개{space}"),
                        },
                        false,
                    )
                }
                Err(e) => (
                    Response::Error {
                        message: e.to_string(),
                    },
                    false,
                ),
            }
        }
        Request::ListShares { remote, secret } => {
            let mut probe = remote.clone();
            if let Some(s) = secret {
                inline_secret(&mut probe.backend, s);
            }
            let result = (|| -> Result<Arc<mydrives_backend_smb::SmbBackend>, String> {
                match &probe.backend {
                    mydrives_core::config::BackendConfig::Smb(smb) => {
                        let password = mydrives_core::secret::load(&probe.id, &smb.secret)
                            .map_err(|e| e.to_string())?
                            .unwrap_or_default();
                        Ok(Arc::new(mydrives_backend_smb::SmbBackend::new(
                            probe.id.clone(),
                            mydrives_backend_smb::SmbTarget {
                                host: smb.host.clone(),
                                port: smb.port,
                                share: smb.share.clone(),
                                user: smb.user.clone(),
                                password,
                                subpath: smb.subpath.clone(),
                            },
                        )))
                    }
                    other => Err(format!("{} 백엔드는 공유 목록을 지원하지 않습니다", other.kind())),
                }
            })();
            match result {
                Ok(backend) => match backend.list_shares().await {
                    Ok(shares) => (Response::Shares { shares }, false),
                    Err(e) => (
                        Response::Error {
                            message: e.to_string(),
                        },
                        false,
                    ),
                },
                Err(message) => (Response::Error { message }, false),
            }
        }
        Request::Remove { id } => {
            let pinned = manager.config_of(&id).map(|cfg| manager.mount_path_of(&cfg));
            if let Err(e) = manager.forget(&id) {
                return (
                    Response::Error {
                        message: e.to_string(),
                    },
                    false,
                );
            }
            // The sidebar row was made for this drive; a drive that is gone
            // must not leave a dead row behind.
            if let Some(path) = pinned {
                if let Err(e) = mydrives_core::finder::unpin_volume(&path) {
                    tracing::warn!(remote = %id, "could not take the drive out of Finder's sidebar: {e}");
                }
            }
            let _ = secret::delete(&id);
            let saved = {
                let mut cfg = config.lock();
                cfg.remove(&id);
                cfg.save()
            };
            match saved {
                Ok(()) => (Response::Ok, false),
                Err(e) => (
                    Response::Error {
                        message: e.to_string(),
                    },
                    false,
                ),
            }
        }
        Request::Rename { id, label } => {
            let label = label.trim().to_string();
            if label.is_empty() {
                return (
                    Response::Error {
                        message: "이름을 비워 둘 수 없습니다".into(),
                    },
                    false,
                );
            }
            // The manager keeps its own copy of the config for `status`, so it
            // has to learn the new name too - otherwise the window shows the
            // old one until the agent restarts.
            let updated = {
                let mut cfg = config.lock();
                if !cfg.set_label(&id, &label) {
                    None
                } else {
                    let saved = cfg.save();
                    cfg.get(&id).cloned().map(|remote| (remote, saved))
                }
            };
            match updated {
                None => (
                    Response::Error {
                        message: format!("{id} 드라이브를 찾을 수 없습니다"),
                    },
                    false,
                ),
                Some((remote, saved)) => {
                    manager.put(remote);
                    match saved {
                        Ok(()) => (Response::Ok, false),
                        Err(e) => (
                            Response::Error {
                                message: format!("이름은 바꿨지만 저장하지 못했습니다: {e}"),
                            },
                            false,
                        ),
                    }
                }
            }
        }
        Request::Mount { id } => {
            // A drive that asked for the `fuse` layer needs FUSE-T on disk
            // before the mount is attempted.
            let wanted = config.lock().get(&id).cloned();
            if let Some(remote) = &wanted {
                ensure_mount_layer(&config, Some(remote)).await;
            }
            match manager.mount(&id).await {
                Ok(mountpoint) => (Response::Mounted { id, mountpoint }, false),
                Err(e) => (
                    Response::Error {
                        message: e.to_string(),
                    },
                    false,
                ),
            }
        }
        Request::Unmount { id } => match manager.unmount(&id).await {
            Ok(()) => (Response::Ok, false),
            Err(e) => (
                Response::Error {
                    message: e.to_string(),
                },
                false,
            ),
        },
        Request::Update { action } => {
            match action {
                mydrives_ipc::UpdateAction::Status => {}
                mydrives_ipc::UpdateAction::Check => updates.request_check(),
                mydrives_ipc::UpdateAction::Apply => updates.request_apply(),
            }
            (
                Response::Update {
                    status: updates.status(),
                },
                false,
            )
        }
        Request::SetUpdateToken { token } => {
            match secret::store(mydrives_core::update::TOKEN_ACCOUNT, token.trim()) {
                Ok(()) => {
                    // A token is only ever stored because a check failed, so try
                    // again straight away rather than waiting for the timer.
                    updates.request_check();
                    (Response::Ok, false)
                }
                Err(e) => (
                    Response::Error {
                        message: format!("토큰 저장 실패: {e}"),
                    },
                    false,
                ),
            }
        }
        Request::FuseT { install } => {
            let mut detail = String::new();
            if install && !mydrives_core::fuse_t::is_installed() {
                match mydrives_core::fuse_t::ensure_installed().await {
                    Ok(true) => detail = "설치했습니다".into(),
                    Ok(false) => detail = "이 플랫폼에서는 설치할 수 없습니다".into(),
                    Err(e) => detail = format!("설치 실패: {e}"),
                }
            }
            let installed = mydrives_core::fuse_t::is_installed();
            if detail.is_empty() {
                detail = if installed {
                    "Finder 사이드바에 드라이브를 올릴 수 있습니다".into()
                } else {
                    "없어도 동작합니다 — 대신 NFS 계층을 쓰고 사이드바에는 안 뜹니다".into()
                };
            }
            (
                Response::FuseT {
                    installed,
                    version: mydrives_core::fuse_t::installed_version(),
                    detail,
                },
                false,
            )
        }
        Request::Autostart { enable } => {
            let result = match enable {
                None => autostart::status(),
                Some(true) => autostart::enable(),
                Some(false) => autostart::disable().and_then(|()| autostart::status()),
            };
            // Remember the choice: the agent reconciles it on every start, and
            // without this a `disable` would be undone by the next launch.
            if let Some(want) = enable {
                let mut cfg = config.lock();
                if cfg.general.autostart != want {
                    cfg.general.autostart = want;
                    if let Err(e) = cfg.save() {
                        tracing::warn!(error = %e, "saving the autostart setting failed");
                    }
                }
            }
            match result {
                Ok(autostart::Status::Enabled { detail }) => (
                    Response::Autostart {
                        enabled: true,
                        detail,
                    },
                    false,
                ),
                Ok(autostart::Status::Disabled) => (
                    Response::Autostart {
                        enabled: false,
                        detail: String::new(),
                    },
                    false,
                ),
                Err(e) => (
                    Response::Error {
                        message: e.to_string(),
                    },
                    false,
                ),
            }
        }
        Request::Shutdown => {
            manager.unmount_all().await;
            // Stop the accept loop *and* release the main thread, otherwise the
            // process keeps running with no way to talk to it.
            shutdown.notify_one();
            (Response::Ok, true)
        }
    }
}

/// One-shot foreground mount. Keeps the process in the foreground so that Ctrl-C
/// (or the test harness killing it) unmounts cleanly.
fn dev_mount(runtime: tokio::runtime::Runtime, cfg: RemoteConfig) -> anyhow::Result<()> {
    let app = AppConfig::default();
    let manager = MountManager::new(
        runtime.handle().clone(),
        providers(),
        Arc::new(factory::Factory),
        &app,
    );
    let id = cfg.id.clone();
    manager.put(cfg);

    let mountpoint = runtime.block_on(manager.mount(&id))?;
    println!("mounted {id} at {mountpoint}");
    if let Some(vfs) = manager.vfs_of(&id) {
        print_source(&vfs);
    }
    println!("press Ctrl-C to unmount");

    runtime.block_on(async {
        wait_for_shutdown(None).await;
        manager.unmount_all().await;
    });
    println!("unmounted {id}");
    Ok(())
}

fn print_source(vfs: &Arc<Vfs>) {
    println!("source: {}", vfs.backend().describe());
}

/// Makes sure the mount layer a drive asked for is actually installed.
///
/// Only FUSE-T needs this: it is a system-wide package, and it is what gets a
/// drive into Finder's sidebar. The install asks for an administrator
/// password, so it runs for a drive that asked for the `fuse` provider and
/// never just because the agent started - and at most once per run, so a
/// cancelled prompt does not come back on every mount.
#[cfg(target_os = "macos")]
async fn ensure_mount_layer(config: &Arc<Mutex<AppConfig>>, remote: Option<&RemoteConfig>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static TRIED: AtomicBool = AtomicBool::new(false);

    // A drive that does not name a provider gets the default one, which on
    // macOS is FUSE - so it wants FUSE-T too. Only a drive pinned to another
    // provider does not.
    let wants_fuse = |r: &RemoteConfig| r.provider.as_deref().unwrap_or("fuse") == "fuse";
    let wants_fuse = match remote {
        Some(r) => wants_fuse(r),
        None => config.lock().remotes.iter().any(wants_fuse),
    };
    let auto_install = config.lock().general.fuse_t_auto_install;
    if !mydrives_core::fuse_t::should_install(
        auto_install,
        wants_fuse,
        mydrives_core::fuse_t::is_installed(),
    ) {
        return;
    }
    if TRIED.swap(true, Ordering::SeqCst) {
        return;
    }
    tracing::info!("FUSE-T가 없어 설치를 시작합니다 (관리자 암호를 묻습니다)");
    match mydrives_core::fuse_t::ensure_installed().await {
        Ok(true) => tracing::info!("FUSE-T 설치 완료"),
        Ok(false) => tracing::warn!("FUSE-T를 설치하지 못했습니다"),
        Err(e) => tracing::warn!(error = %e, "FUSE-T 설치 실패 — NFS 계층으로 계속합니다"),
    }
}

#[cfg(not(target_os = "macos"))]
async fn ensure_mount_layer(_config: &Arc<Mutex<AppConfig>>, _remote: Option<&RemoteConfig>) {}
