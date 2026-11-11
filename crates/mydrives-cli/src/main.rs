//! `mydrives` - control the running agent.
//!
//! Everything goes through the agent's local socket, so the CLI never touches a
//! mount or the keychain itself. That keeps one owner for the config file and
//! means the CLI works the same whether the user is driving the tray or a script.

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};

use mydrives_core::config::{
    BackendConfig, FtpConfig, MemConfig, MyboxConfig, RemoteConfig, SecretRef, SmbConfig,
};
use mydrives_core::manager::MountState;
use mydrives_ipc::{Request, Response};

#[derive(Parser)]
#[command(name = "mydrives", about = "Mount remote storage as local drives", version)]
struct Cli {
    /// Machine-readable output.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Is the agent up?
    Ping,
    /// Show every configured remote and its mount state.
    Status,
    /// Mount layers available in this build.
    Providers,
    /// Add (or update) a remote.
    Add {
        #[command(subcommand)]
        kind: AddKind,
    },
    /// Try settings without saving them: connect, read the root, report.
    Test {
        #[command(subcommand)]
        kind: AddKind,
    },
    /// List the shares a server offers (no need to know the name in advance).
    Shares {
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 445)]
        port: u16,
        #[arg(long, default_value = "")]
        user: String,
        #[arg(long)]
        password_stdin: bool,
    },
    /// Remove a remote (must be unmounted).
    Rm { id: String },
    /// Change the name shown for a drive (the mount path keeps the share name).
    Rename {
        id: String,
        name: String,
    },
    Mount { id: String },
    Unmount { id: String },
    /// Updates: show what the agent knows, ask it to look now, or apply.
    Update {
        /// Look for a new release right away instead of waiting for the timer.
        #[arg(long)]
        check: bool,
        /// Apply a downloaded update now (restarts the agent, drives drop).
        #[arg(long)]
        now: bool,
        /// Read a GitHub token from stdin and keep it in the OS keychain.
        /// Only needed when the release repository is private.
        #[arg(long)]
        token_stdin: bool,
    },
    /// macOS mount layer (FUSE-T): show it, or install it now.
    FuseT {
        /// Install FUSE-T if it is missing (asks for an administrator password).
        #[arg(long)]
        install: bool,
    },
    /// Start MyDrives when you log in: `on`, `off`, or nothing to just show it.
    Autostart {
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
    },
    /// Where the config file lives.
    ConfigPath,
    /// Stop the agent (unmounts everything first).
    Shutdown,
}

#[derive(Subcommand)]
enum AddKind {
    /// A NAS or Windows share.
    Smb {
        /// Drive name. Defaults to the share name, which keeps the local folder
        /// matching the remote one (`~/MyDrives/<share>`).
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 445)]
        port: u16,
        #[arg(long)]
        share: String,
        #[arg(long, default_value = "")]
        user: String,
        /// Read the password from stdin (recommended: it stays out of `ps` and
        /// shell history). It is stored in the OS keychain, never in the config.
        #[arg(long)]
        password_stdin: bool,
        /// Only part of the share.
        #[arg(long)]
        subpath: Option<String>,
        /// Windows drive letter, e.g. `Z:`.
        #[arg(long)]
        drive: Option<String>,
        /// Directory to mount on (macOS/Linux).
        #[arg(long)]
        mountpoint: Option<String>,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        readonly: bool,
        #[arg(long)]
        no_automount: bool,
        /// Mount it right away.
        #[arg(long)]
        mount: bool,
    },
    /// An FTP or FTPS account (web host, router, older NAS).
    Ftp {
        /// Drive name. Defaults to the last part of the remote path, or to the
        /// host when the whole account is mounted.
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        host: String,
        /// 21 for plain and explicit FTPS, 990 for implicit.
        #[arg(long)]
        port: Option<u16>,
        #[arg(long, default_value = "")]
        user: String,
        /// Read the password from stdin (recommended: it stays out of `ps` and
        /// shell history). It is stored in the OS keychain, never in the config.
        #[arg(long)]
        password_stdin: bool,
        /// `plain`, `explicit` (AUTH TLS, the default) or `implicit`.
        #[arg(long, default_value = "explicit")]
        security: String,
        /// Data connection: `passive` (default), `epsv`, or `active`.
        #[arg(long, default_value = "passive")]
        mode: String,
        /// Passive mode: ignore the address PASV answered with and keep talking
        /// to the one that is already connected. For a server behind NAT that
        /// reports its private address.
        #[arg(long)]
        nat_workaround: bool,
        /// Active mode: seconds to wait for the server to connect back.
        #[arg(long, default_value_t = 10)]
        active_timeout: u64,
        /// Accept a TLS certificate that does not verify - what a NAS with its
        /// factory self-signed certificate needs.
        #[arg(long)]
        insecure_tls: bool,
        /// Only part of the account, e.g. `/public_html`.
        #[arg(long)]
        subpath: Option<String>,
        /// Windows drive letter, e.g. `Z:`.
        #[arg(long)]
        drive: Option<String>,
        /// Directory to mount on (macOS/Linux).
        #[arg(long)]
        mountpoint: Option<String>,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        readonly: bool,
        #[arg(long)]
        no_automount: bool,
        /// Mount it right away.
        #[arg(long)]
        mount: bool,
    },
    /// 네이버 MYBOX, with a personal access token from the MYBOX web site
    /// (설정 → 계정 및 개인 액세스 토큰 관리).
    Mybox {
        /// Drive name. Defaults to the folder mounted, or `MYBOX` for the
        /// whole account.
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        label: Option<String>,
        /// Read the access token from stdin (it is stored in the OS keychain,
        /// never in the config).
        #[arg(long)]
        token_stdin: bool,
        /// Only one folder of the account, e.g. `/Work/2026`.
        #[arg(long)]
        subpath: Option<String>,
        /// Windows drive letter, e.g. `Z:`.
        #[arg(long)]
        drive: Option<String>,
        /// Directory to mount on (macOS/Linux).
        #[arg(long)]
        mountpoint: Option<String>,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        readonly: bool,
        #[arg(long)]
        no_automount: bool,
        /// Mount it right away.
        #[arg(long)]
        mount: bool,
    },
    /// An in-memory test volume, for checking that mounting works at all.
    Mem {
        #[arg(long)]
        id: String,
        #[arg(long)]
        mountpoint: Option<String>,
        #[arg(long)]
        drive: Option<String>,
        #[arg(long)]
        mount: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(cli))
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::ConfigPath => {
            println!("{}", mydrives_core::config::config_path().display());
            return Ok(());
        }
        Command::Ping => {
            let res = send(&Request::Ping, cli.json).await?;
            if let Response::Pong { version, agent } = res {
                println!("agent {agent} speaking protocol v{version}");
            }
        }
        Command::Status => {
            let res = send(&Request::Status, cli.json).await?;
            if let Response::Status { remotes } = res {
                if remotes.is_empty() {
                    println!("no remotes configured - add one with `mydrives add smb --help`");
                }
                for r in remotes {
                    let state = match &r.state {
                        MountState::Mounted {
                            mountpoint,
                            provider,
                        } => format!("mounted at {mountpoint} ({provider})"),
                        MountState::Offline { mountpoint, reason } => {
                            format!("OFFLINE at {mountpoint}: {reason}")
                        }
                        MountState::Failed { reason } => format!("failed: {reason}"),
                        MountState::Mounting => "mounting...".to_string(),
                        MountState::Unmounted => "not mounted".to_string(),
                    };
                    println!(
                        "{:<12} {:<6} {:<34} {}",
                        r.id, r.kind, r.source, state
                    );
                    if let Some(s) = r.stats {
                        println!(
                            "{:<12} {:<6} {:<34} {} read / {} written, {} calls, {} cache hits, {} errors, {} open",
                            "", "", "",
                            human(s.bytes_read),
                            human(s.bytes_written),
                            s.backend_calls,
                            s.cache_hits,
                            s.errors,
                            s.open_handles
                        );
                    }
                }
            }
        }
        Command::Providers => {
            let res = send(&Request::Providers, cli.json).await?;
            if let Response::Providers { providers } = res {
                for p in providers {
                    match p.unavailable_reason {
                        None => println!("{:<8} available", p.name),
                        Some(r) => println!("{:<8} unavailable: {r}", p.name),
                    }
                }
            }
        }
        Command::Add { kind } => {
            let (remote, secret, mount_now) = build_remote(kind)?;
            let id = remote.id.clone();
            let res = send(
                &Request::Add {
                    remote,
                    secret,
                    replace: false,
                },
                cli.json,
            )
            .await?;
            if let Response::Ok = res {
                if !cli.json {
                    println!("added {id}");
                }
            }
            if mount_now {
                let res = send(&Request::Mount { id }, cli.json).await?;
                if let Response::Mounted { id, mountpoint } = res {
                    println!("mounted {id} at {mountpoint}");
                }
            }
        }
        Command::Test { kind } => {
            let (remote, secret, _) = build_remote(kind)?;
            let res = send(&Request::Test { remote, secret }, cli.json).await?;
            if let Response::Tested { summary } = res {
                println!("{summary}");
            }
        }
        Command::Shares {
            host,
            port,
            user,
            password_stdin,
        } => {
            let secret = if password_stdin {
                let mut buf = String::new();
                std::io::stdin().read_line(&mut buf)?;
                Some(buf.trim_end_matches(['\n', '\r']).to_string())
            } else {
                None
            };
            let remote = RemoteConfig {
                id: "probe".into(),
                label: String::new(),
                automount: false,
                readonly: true,
                provider: None,
                drive: None,
                mountpoint: None,
                backend: BackendConfig::Smb(SmbConfig {
                    host,
                    port,
                    share: String::new(),
                    user,
                    subpath: None,
                    secret: SecretRef::Keychain,
                }),
            };
            let res = send(&Request::ListShares { remote, secret }, cli.json).await?;
            if let Response::Shares { shares } = res {
                if shares.is_empty() {
                    println!("공유 폴더가 없습니다 (또는 이 계정에 보이는 것이 없습니다)");
                }
                for s in shares {
                    println!("{s}");
                }
            }
        }
        Command::Rm { id } => {
            send(&Request::Remove { id: id.clone() }, cli.json).await?;
            if !cli.json {
                println!("removed {id}");
            }
        }
        Command::Rename { id, name } => {
            send(
                &Request::Rename {
                    id: id.clone(),
                    label: name.clone(),
                },
                cli.json,
            )
            .await?;
            if !cli.json {
                println!("{id} 이름을 \"{name}\" 으로 바꿨습니다");
            }
        }
        Command::Mount { id } => {
            let res = send(&Request::Mount { id }, cli.json).await?;
            if let Response::Mounted { id, mountpoint } = res {
                println!("mounted {id} at {mountpoint}");
            }
        }
        Command::Unmount { id } => {
            send(&Request::Unmount { id: id.clone() }, cli.json).await?;
            if !cli.json {
                println!("unmounted {id}");
            }
        }
        Command::Update {
            check,
            now,
            token_stdin,
        } => {
            if token_stdin {
                let mut buf = String::new();
                std::io::stdin()
                    .read_line(&mut buf)
                    .context("reading the token from stdin")?;
                send(
                    &Request::SetUpdateToken {
                        token: buf.trim().to_string(),
                    },
                    cli.json,
                )
                .await?;
                if !cli.json {
                    println!("토큰을 OS 키체인에 저장했습니다");
                }
            }
            let action = if now {
                mydrives_ipc::UpdateAction::Apply
            } else if check || token_stdin {
                mydrives_ipc::UpdateAction::Check
            } else {
                mydrives_ipc::UpdateAction::Status
            };
            let mut res = send(&Request::Update { action }, cli.json).await?;
            // A check happens on the agent's own task, so the first answer is
            // still "checking". Wait for it rather than printing a state that
            // is already stale by the time it reaches the terminal.
            if action == mydrives_ipc::UpdateAction::Check {
                for _ in 0..40 {
                    // `Idle` counts as "not finished": the agent answers before
                    // its background task has even started looking, and a
                    // completed check always lands on some other state.
                    let waiting = matches!(
                        &res,
                        Response::Update { status } if matches!(
                            status.state,
                            mydrives_core::update::UpdateState::Checking
                                | mydrives_core::update::UpdateState::Idle
                        )
                    );
                    if !waiting {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    res = send(
                        &Request::Update {
                            action: mydrives_ipc::UpdateAction::Status,
                        },
                        false,
                    )
                    .await?;
                }
            }
            if let Response::Update { status } = res {
                println!("현재 버전: {}", status.current);
                if let Some(latest) = &status.latest {
                    println!("최신 릴리즈: {latest}");
                }
                println!("{}", status.summary());
            }
        }
        Command::FuseT { install } => {
            let res = send(&Request::FuseT { install }, cli.json).await?;
            if let Response::FuseT {
                installed,
                version,
                detail,
            } = res
            {
                match (installed, version) {
                    (true, Some(v)) => println!("FUSE-T {v} 설치됨"),
                    (true, None) => println!("FUSE-T 설치됨"),
                    (false, _) => println!("FUSE-T 없음"),
                }
                println!("  {detail}");
            }
        }
        Command::Autostart { state } => {
            let enable = match state.as_deref() {
                Some("on") => Some(true),
                Some("off") => Some(false),
                _ => None,
            };
            let res = send(&Request::Autostart { enable }, cli.json).await?;
            if let Response::Autostart { enabled, detail } = res {
                if enabled {
                    println!("로그인 시 자동 시작: 켜짐\n  등록 위치: {detail}");
                } else {
                    println!("로그인 시 자동 시작: 꺼짐");
                }
            }
        }
        Command::Shutdown => {
            send(&Request::Shutdown, cli.json).await?;
            if !cli.json {
                println!("agent stopped");
            }
        }
    }
    Ok(())
}

fn build_remote(kind: AddKind) -> anyhow::Result<(RemoteConfig, Option<String>, bool)> {
    match kind {
        AddKind::Smb {
            id,
            label,
            host,
            port,
            share,
            user,
            password_stdin,
            subpath,
            drive,
            mountpoint,
            provider,
            readonly,
            no_automount,
            mount,
        } => {
            let secret = if password_stdin {
                let mut buf = String::new();
                std::io::stdin()
                    .read_line(&mut buf)
                    .context("reading the password from stdin")?;
                Some(buf.trim_end_matches(['\n', '\r']).to_string())
            } else {
                None
            };
            // Name the drive after the share unless told otherwise.
            let id = id.unwrap_or_else(|| share.clone());
            Ok((
                RemoteConfig {
                    // 비우면 에이전트가 제품 이름(NAS 이름·서비스 이름)으로 채웁니다.
                    label: label.unwrap_or_default(),
                    automount: !no_automount,
                    readonly,
                    provider,
                    drive,
                    mountpoint,
                    backend: BackendConfig::Smb(SmbConfig {
                        host,
                        port,
                        share,
                        user,
                        subpath,
                        secret: SecretRef::Keychain,
                    }),
                    id,
                },
                secret,
                mount,
            ))
        }
        AddKind::Ftp {
            id,
            label,
            host,
            port,
            user,
            password_stdin,
            security,
            mode,
            nat_workaround,
            active_timeout,
            insecure_tls,
            subpath,
            drive,
            mountpoint,
            provider,
            readonly,
            no_automount,
            mount,
        } => {
            let secret = if password_stdin {
                let mut buf = String::new();
                std::io::stdin()
                    .read_line(&mut buf)
                    .context("reading the password from stdin")?;
                Some(buf.trim_end_matches(['\n', '\r']).to_string())
            } else {
                None
            };
            // Checked here so a typo is a message now rather than a failed mount
            // later, when the agent builds the backend.
            if !FtpConfig::security_is_known(&security) {
                anyhow::bail!("--security 는 plain, explicit, implicit 중 하나여야 합니다");
            }
            if !FtpConfig::mode_is_known(&mode) {
                anyhow::bail!("--mode 는 passive, epsv, active 중 하나여야 합니다");
            }
            // Implicit FTPS has its own well-known port; everything else is 21.
            let port = port.unwrap_or(if security.eq_ignore_ascii_case("implicit") {
                990
            } else {
                21
            });
            // The drive is named after the folder it exposes, like a share.
            let id = id.unwrap_or_else(|| mydrives_core::naming::ftp_default_id(&host, subpath.as_deref()));
            Ok((
                RemoteConfig {
                    // 비우면 에이전트가 제품 이름(호스트 이름)으로 채웁니다.
                    label: label.unwrap_or_default(),
                    automount: !no_automount,
                    readonly,
                    provider,
                    drive,
                    mountpoint,
                    backend: BackendConfig::Ftp(FtpConfig {
                        host,
                        port,
                        user,
                        subpath,
                        security,
                        mode,
                        passive_nat_workaround: nat_workaround,
                        active_timeout_secs: active_timeout,
                        accept_invalid_certs: insecure_tls,
                        max_connections: 4,
                        secret: SecretRef::Keychain,
                    }),
                    id,
                },
                secret,
                mount,
            ))
        }
        AddKind::Mybox {
            id,
            label,
            token_stdin,
            subpath,
            drive,
            mountpoint,
            provider,
            readonly,
            no_automount,
            mount,
        } => {
            let secret = if token_stdin {
                let mut buf = String::new();
                std::io::stdin()
                    .read_line(&mut buf)
                    .context("reading the access token from stdin")?;
                Some(buf.trim().to_string())
            } else {
                None
            };
            if secret.as_deref().map_or(true, str::is_empty) {
                anyhow::bail!("MYBOX 개인 액세스 토큰이 필요합니다: `--token-stdin` 으로 넘기세요");
            }
            let id = id.unwrap_or_else(|| {
                mydrives_core::naming::cloud_default_id("MYBOX", subpath.as_deref())
            });
            Ok((
                RemoteConfig {
                    label: label.unwrap_or_default(),
                    automount: !no_automount,
                    readonly,
                    provider,
                    drive,
                    mountpoint,
                    backend: BackendConfig::Mybox(MyboxConfig {
                        subpath,
                        secret: SecretRef::Keychain,
                    }),
                    id,
                },
                secret,
                mount,
            ))
        }
        AddKind::Mem {
            id,
            mountpoint,
            drive,
            mount,
        } => Ok((
            RemoteConfig {
                // 에이전트가 제품 이름으로 채웁니다.
                label: String::new(),
                automount: false,
                readonly: false,
                provider: None,
                drive,
                mountpoint,
                backend: BackendConfig::Mem(MemConfig { fixture: true }),
                id,
            },
            None,
            mount,
        )),
    }
}

async fn send(req: &Request, json: bool) -> anyhow::Result<Response> {
    let res = mydrives_ipc::call(req).await.map_err(|e| match e {
        mydrives_ipc::IpcError::NotRunning(detail) => anyhow::anyhow!(
            "the MyDrives agent is not running ({detail}).\n\
             start it with `mydrivesd run`"
        ),
        other => anyhow::Error::new(other),
    })?;
    if json {
        println!("{}", serde_json::to_string_pretty(&res)?);
    }
    if let Response::Error { message } = &res {
        bail!("{message}");
    }
    Ok(res)
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
