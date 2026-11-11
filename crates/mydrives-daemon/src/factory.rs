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
            BackendConfig::Mybox(mybox) => {
                let token = load_secret(&cfg.id, &mybox.secret)?.unwrap_or_default();
                if token.trim().is_empty() {
                    return Err(Error::AuthFailed(format!(
                        "'{}'에 저장된 MYBOX 액세스 토큰이 없습니다 - 드라이브 편집에서 토큰을 넣으세요",
                        cfg.id
                    )));
                }
                Ok(mydrives_backend_cloud::mybox::backend(
                    cfg.id.clone(),
                    token.trim().to_string(),
                    mybox.subpath.clone(),
                ))
            }
            BackendConfig::Gdrive(oauth) => {
                let stored = load_secret(&cfg.id, &oauth.secret)?.unwrap_or_default();
                if stored.trim().is_empty() {
                    return Err(Error::AuthFailed(format!(
                        "'{}'은 아직 Google에 로그인하지 않았습니다 - 드라이브 편집에서 로그인하세요",
                        cfg.id
                    )));
                }
                let tokens = mydrives_core::oauth::TokenSet::from_json(&stored)?;
                let provider = mydrives_backend_cloud::gdrive::provider(
                    &oauth.client_id,
                    oauth.client_secret.as_deref(),
                );
                // A refreshed token set goes back where the old one was, so
                // the next start does not have to refresh again - and so a
                // Google refresh token rotation is not lost.
                let id = cfg.id.clone();
                let session = mydrives_core::oauth::Session::new(provider, tokens, move |t| {
                    if let Err(e) = secret::store(&id, &t.to_json()) {
                        tracing::warn!(remote = %id, error = %e, "갱신된 로그인 정보를 저장하지 못했습니다");
                    }
                });
                Ok(mydrives_backend_cloud::gdrive::backend(
                    cfg.id.clone(),
                    session,
                    oauth.subpath.clone(),
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
