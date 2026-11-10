//! Turns configuration into live backends.
//!
//! The only place that knows which backend crates are linked into this binary,
//! which is what lets the core stay free of protocol dependencies.

use std::sync::Arc;
use std::time::Duration;

use mydrives_core::backend::Backend;
use mydrives_core::backends::mem::{fixture, MemBackend};
use mydrives_core::config::{BackendConfig, RemoteConfig};
use mydrives_core::error::{Error, Result};
use mydrives_core::manager::BackendFactory;
use mydrives_core::secret;

use mydrives_backend_ftp::FtpTarget;
use mydrives_backend_smb::{SmbBackend, SmbTarget};

/// How long to wait for the OS credential store.
///
/// Reading the Keychain can block indefinitely: macOS asks the user to allow
/// access whenever the calling binary's signature changes (every rebuild during
/// development, and every update in production), and the call does not return
/// until that dialog is answered. Blocking a mount forever on it - with no
/// message anywhere - is worse than failing with an explanation.
const SECRET_TIMEOUT: Duration = Duration::from_secs(20);

fn load_secret(
    id: &str,
    how: &mydrives_core::config::SecretRef,
) -> Result<Option<String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let id_owned = id.to_string();
    let how_owned = how.clone();
    std::thread::spawn(move || {
        let _ = tx.send(secret::load(&id_owned, &how_owned));
    });
    match rx.recv_timeout(SECRET_TIMEOUT) {
        Ok(result) => result,
        Err(_) => Err(Error::Timeout(format!(
            "'{id}'의 비밀번호를 키체인에서 읽지 못했습니다 — 키체인 접근 허용 대화상자가 떠 있는지 확인하세요"
        ))),
    }
}

pub struct Factory;

impl BackendFactory for Factory {
    fn build(&self, cfg: &RemoteConfig) -> Result<Arc<dyn Backend>> {
        match &cfg.backend {
            BackendConfig::Smb(smb) => {
                let password = load_secret(&cfg.id, &smb.secret)?.unwrap_or_default();
                if password.is_empty() && !smb.user.is_empty() {
                    // Guest access with a user name set is almost always a missing
                    // keychain entry, and the resulting STATUS_LOGON_FAILURE is
                    // much harder to read than this.
                    return Err(Error::AuthFailed(format!(
                        "no password stored for '{}' (user '{}'); add it with `mydrives add`",
                        cfg.id, smb.user
                    )));
                }
                Ok(Arc::new(SmbBackend::new(
                    cfg.id.clone(),
                    SmbTarget {
                        host: smb.host.clone(),
                        port: smb.port,
                        share: smb.share.clone(),
                        user: smb.user.clone(),
                        password,
                        subpath: smb.subpath.clone(),
                    },
                )))
            }
            BackendConfig::Ftp(ftp) => {
                let password = load_secret(&cfg.id, &ftp.secret)?.unwrap_or_default();
                let security = mydrives_backend_ftp::FtpSecurity::parse(&ftp.security)
                    .ok_or_else(|| {
                        Error::InvalidArgument(format!(
                            "'{}'의 security 값 '{}'을 모르겠습니다 (plain, explicit, implicit)",
                            cfg.id, ftp.security
                        ))
                    })?;
                let mode = mydrives_backend_ftp::FtpMode::parse(&ftp.mode).ok_or_else(|| {
                    Error::InvalidArgument(format!(
                        "'{}'의 mode 값 '{}'을 모르겠습니다 (passive, epsv, active)",
                        cfg.id, ftp.mode
                    ))
                })?;
                Ok(mydrives_backend_ftp::backend(
                    cfg.id.clone(),
                    FtpTarget {
                        host: ftp.host.clone(),
                        port: ftp.port,
                        user: ftp.user.clone(),
                        password,
                        subpath: ftp.subpath.clone(),
                        security,
                        mode,
                        passive_nat_workaround: ftp.passive_nat_workaround,
                        active_timeout: Duration::from_secs(ftp.active_timeout_secs.max(1)),
                        accept_invalid_certs: ftp.accept_invalid_certs,
                        max_connections: ftp.max_connections,
                    },
                ))
            }
            BackendConfig::Mem(mem) => Ok(Arc::new(if mem.fixture {
                fixture(&cfg.id)
            } else {
                MemBackend::new(cfg.id.clone())
            })),
        }
    }
}
