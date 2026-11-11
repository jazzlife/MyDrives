//! On-disk configuration (TOML) for the agent.
//!
//! Secrets never live here - only a reference to the OS keychain entry. See
//! [`crate::secret`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub general: General,
    #[serde(default)]
    pub update: UpdateConfig,
    #[serde(default, rename = "remote")]
    pub remotes: Vec<RemoteConfig>,
}

/// Where new versions come from. See [`crate::update`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `owner/repo` on GitHub. Releases are matched by tag (`v0.2.0`).
    #[serde(default = "default_update_repo")]
    pub repo: String,
    #[serde(default = "default_check_interval")]
    pub check_interval_mins: u64,
    /// Accept pre-releases as well. Off: a tagged beta must not land on a
    /// machine that only asked for releases.
    #[serde(default)]
    pub prerelease: bool,
    /// Read a GitHub token from the OS keychain. Needed only for a private
    /// repository; a public one is read anonymously.
    #[serde(default = "default_true")]
    pub use_token: bool,
}

fn default_update_repo() -> String {
    "jazzlife/MyDrives".to_string()
}

fn default_check_interval() -> u64 {
    360
}

impl Default for UpdateConfig {
    fn default() -> Self {
        UpdateConfig {
            enabled: true,
            repo: default_update_repo(),
            check_interval_mins: default_check_interval(),
            prerelease: false,
            use_token: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct General {
    /// Where to put mount points that are not drive letters.
    #[serde(default)]
    pub mount_root: Option<String>,
    /// Seconds between liveness probes of a mounted remote.
    #[serde(default = "default_probe_secs")]
    pub probe_secs: u64,
    #[serde(default = "default_true")]
    pub show_tray: bool,
    /// Install FUSE-T when a drive asks for the `fuse` mount layer and it is
    /// missing. It is what puts a drive in Finder's sidebar; the install needs
    /// an administrator password, so it only runs for a drive that asked for
    /// it, never just because the agent started.
    #[serde(default = "default_true")]
    pub fuse_t_auto_install: bool,
    /// Let the agent use `sudo -n` to make a mount point the user cannot
    /// create, which on macOS means anything directly under `/Volumes`.
    ///
    /// Off by default, and it belongs here rather than in an environment
    /// variable: the agent is started by launchd, by Finder, by the installer
    /// and by its own updater, and only one of those passes an environment on.
    /// A machine where `sudo -n` asks for a password gets the same clear error
    /// either way.
    #[serde(default)]
    pub sudo_mount: bool,
    /// Start the agent when the user logs in. Reconciled on every start, so an
    /// agent moved or rebuilt elsewhere re-points the login item at itself
    /// instead of leaving one that launches a binary that is no longer there.
    #[serde(default = "default_true")]
    pub autostart: bool,
}

fn default_probe_secs() -> u64 {
    20
}

fn default_true() -> bool {
    true
}

impl Default for General {
    fn default() -> Self {
        General {
            mount_root: None,
            probe_secs: default_probe_secs(),
            show_tray: true,
            fuse_t_auto_install: true,
            sudo_mount: false,
            autostart: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// Stable identifier; also the keychain account name.
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default = "default_true")]
    pub automount: bool,
    #[serde(default)]
    pub readonly: bool,
    /// Mount layer to use: `fuse`, `nfs`, `winfsp`, or unset for "best available".
    #[serde(default)]
    pub provider: Option<String>,
    /// Windows drive letter, e.g. "Z:".
    #[serde(default)]
    pub drive: Option<String>,
    /// Mount directory for POSIX hosts, e.g. "/Volumes/nas" or "~/MyDrives/nas".
    #[serde(default)]
    pub mountpoint: Option<String>,
    pub backend: BackendConfig,
}

impl RemoteConfig {
    pub fn display_label(&self) -> &str {
        if self.label.is_empty() {
            &self.id
        } else {
            &self.label
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum BackendConfig {
    Smb(SmbConfig),
    Ftp(FtpConfig),
    Mybox(MyboxConfig),
    Mem(MemConfig),
    // Planned, deliberately not stubbed as half-working backends:
    //   Gdrive(OAuthConfig), Onedrive(OAuthConfig), Dropbox(OAuthConfig),
    //   Webdav(WebdavConfig)
}

impl BackendConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            BackendConfig::Smb(_) => "smb",
            BackendConfig::Ftp(_) => "ftp",
            BackendConfig::Mybox(_) => "mybox",
            BackendConfig::Mem(_) => "mem",
        }
    }
}

/// 네이버 MYBOX over its Open API. The only credential is a personal access
/// token the user makes on the MYBOX web site; it is the drive's secret and
/// lives where passwords do.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MyboxConfig {
    /// Folder inside the account to expose as the mount root; the whole
    /// account when absent.
    #[serde(default)]
    pub subpath: Option<String>,
    #[serde(default)]
    pub secret: SecretRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmbConfig {
    pub host: String,
    #[serde(default = "default_smb_port")]
    pub port: u16,
    pub share: String,
    #[serde(default)]
    pub user: String,
    /// Subdirectory of the share to expose as the mount root.
    #[serde(default)]
    pub subpath: Option<String>,
    /// Where the password comes from. Default: the OS keychain.
    #[serde(default)]
    pub secret: SecretRef,
}

fn default_smb_port() -> u16 {
    445
}

/// FTP and FTPS. `security` is spelled out rather than inferred from the port,
/// because a server may offer explicit FTPS on the plain port 21 - which is the
/// usual arrangement - and nothing in the address says so.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FtpConfig {
    pub host: String,
    #[serde(default = "default_ftp_port")]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    /// Subdirectory of the account to expose as the mount root.
    #[serde(default)]
    pub subpath: Option<String>,
    /// `plain`, `explicit` (AUTH TLS, the default) or `implicit`.
    #[serde(default = "default_ftp_security")]
    pub security: String,
    /// Accept a TLS certificate that does not verify - what a NAS with its
    /// factory self-signed certificate needs.
    #[serde(default)]
    pub accept_invalid_certs: bool,
    /// How the data connection is opened: `passive` (the default, the client
    /// dials out), `epsv` (the IPv6-capable spelling of the same thing) or
    /// `active` (the server dials back, which needs the client reachable).
    #[serde(default = "default_ftp_mode")]
    pub mode: String,
    /// Passive mode only: ignore the address `PASV` answered with and use the
    /// one the control connection is already talking to. A server behind NAT
    /// that was not told its public address answers with its private one, and
    /// the data connection then goes nowhere.
    #[serde(default)]
    pub passive_nat_workaround: bool,
    /// Active mode only: how long to wait for the server to connect back.
    #[serde(default = "default_ftp_active_timeout")]
    pub active_timeout_secs: u64,
    #[serde(default = "default_ftp_connections")]
    pub max_connections: usize,
    /// Where the password comes from. Default: the OS keychain.
    #[serde(default)]
    pub secret: SecretRef,
}

fn default_ftp_port() -> u16 {
    21
}

fn default_ftp_security() -> String {
    "explicit".to_string()
}

fn default_ftp_connections() -> usize {
    4
}

fn default_ftp_mode() -> String {
    "passive".to_string()
}

fn default_ftp_active_timeout() -> u64 {
    10
}

impl FtpConfig {
    /// The spellings `security` accepts. Checked where a drive is added, so a
    /// typo is a message at that moment instead of a mount that fails later.
    pub const SECURITY_VALUES: [&'static str; 3] = ["plain", "explicit", "implicit"];
    /// Likewise for `mode`.
    pub const MODE_VALUES: [&'static str; 3] = ["passive", "epsv", "active"];

    pub fn security_is_known(value: &str) -> bool {
        Self::SECURITY_VALUES
            .iter()
            .any(|v| value.eq_ignore_ascii_case(v))
    }

    pub fn mode_is_known(value: &str) -> bool {
        Self::MODE_VALUES.iter().any(|v| value.eq_ignore_ascii_case(v))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemConfig {
    /// Seed the in-memory tree with the standard fixture (used by smoke tests).
    #[serde(default)]
    pub fixture: bool,
}

/// How to obtain a secret. `Keychain` is the default and the only one that should
/// be used in production; the others exist for CI and for headless servers with no
/// keychain daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "lowercase")]
pub enum SecretRef {
    Keychain,
    Env { name: String },
    /// Plain file, 0600. Logged as a warning on every mount.
    File { path: String },
    /// Inline, for throwaway test configs only.
    Inline { value: String },
}

impl Default for SecretRef {
    fn default() -> Self {
        SecretRef::Keychain
    }
}

/// Config directory, overridable with `MYDRIVES_CONFIG_DIR` (tests, portable use).
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MYDRIVES_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dirs) = directories::ProjectDirs::from("app", "MyDrives", "MyDrives") {
        return dirs.config_dir().to_path_buf();
    }
    PathBuf::from(".")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Default parent directory for POSIX mount points.
pub fn default_mount_root() -> PathBuf {
    if let Ok(dir) = std::env::var("MYDRIVES_MOUNT_ROOT") {
        return PathBuf::from(dir);
    }
    match directories::UserDirs::new() {
        Some(u) => u.home_dir().join("MyDrives"),
        None => PathBuf::from("/tmp/MyDrives"),
    }
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| Error::InvalidArgument(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AppConfig::default()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| Error::Backend(format!("serialize config: {e}")))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&RemoteConfig> {
        self.remotes.iter().find(|r| r.id == id)
    }

    pub fn upsert(&mut self, remote: RemoteConfig) {
        match self.remotes.iter_mut().find(|r| r.id == remote.id) {
            Some(slot) => *slot = remote,
            None => self.remotes.push(remote),
        }
    }

    /// Renames a drive. Only the label changes - the id is the mount point and
    /// the keychain account, so renaming must not touch it.
    pub fn set_label(&mut self, id: &str, label: &str) -> bool {
        match self.remotes.iter_mut().find(|r| r.id == id) {
            Some(remote) => {
                remote.label = label.trim().to_string();
                true
            }
            None => false,
        }
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.remotes.len();
        self.remotes.retain(|r| r.id != id);
        self.remotes.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_toml() {
        let mut cfg = AppConfig::default();
        cfg.upsert(RemoteConfig {
            id: "nas".into(),
            label: "home NAS".into(),
            automount: true,
            readonly: false,
            provider: None,
            drive: Some("Z:".into()),
            mountpoint: None,
            backend: BackendConfig::Smb(SmbConfig {
                host: "192.168.0.10".into(),
                port: 445,
                share: "data".into(),
                user: "admin".into(),
                subpath: None,
                secret: SecretRef::Keychain,
            }),
        });
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.remotes.len(), 1);
        let r = &back.remotes[0];
        assert_eq!(r.id, "nas");
        assert_eq!(r.backend.kind(), "smb");
        match &r.backend {
            BackendConfig::Smb(s) => {
                assert_eq!(s.share, "data");
                assert!(matches!(s.secret, SecretRef::Keychain));
            }
            _ => panic!("wrong kind"),
        }
    }

    #[test]
    fn missing_file_is_empty_config() {
        let cfg = AppConfig::load_from(Path::new("/nonexistent/mydrives.toml")).unwrap();
        assert!(cfg.remotes.is_empty());
        assert_eq!(cfg.general.probe_secs, 20);
    }
}
