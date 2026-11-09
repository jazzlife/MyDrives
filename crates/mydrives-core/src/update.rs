//! Updating the agent from GitHub Releases.
//!
//! The release workflow publishes one plain binary per executable and target
//! (`mydrivesd-x86_64-pc-windows-msvc.exe`, …) plus a `SHA256SUMS` file. No
//! archives: unpacking a tarball or a zip would mean another dependency and
//! another format to get wrong, while three downloads and a checksum are the
//! whole of it.
//!
//! The flow is deliberately split in two halves that can fail independently:
//!
//! 1. **stage** - download into `config_dir/updates/<version>/`, verify every
//!    checksum, and stop. Nothing the user can see has changed yet, so a failed
//!    or half-finished download costs nothing.
//! 2. **apply** - swap the staged files into the install directory and restart.
//!    This unmounts every drive, so the caller decides *when*: the agent waits
//!    for a moment with no open files, and otherwise applies at the next start.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::UpdateConfig;
use crate::error::{Error, Result};

/// Where `security`/the keychain keeps a token for private repositories.
pub const TOKEN_ACCOUNT: &str = "github-update-token";
const CHECKSUMS: &str = "SHA256SUMS";

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// What the agent is doing about updates, as shown in the tray and the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum UpdateState {
    /// Turned off in the config.
    Disabled,
    /// Nothing checked yet.
    Idle,
    Checking,
    UpToDate,
    Downloading { version: String },
    /// Downloaded and verified; waiting for a quiet moment to restart.
    Staged { version: String },
    Applying { version: String },
    Failed { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateStatus {
    pub current: String,
    pub latest: Option<String>,
    pub state: UpdateState,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        UpdateStatus {
            current: current_version().to_string(),
            latest: None,
            state: UpdateState::Idle,
        }
    }
}

impl UpdateStatus {
    /// One line for the tray menu and `mydrives update`.
    pub fn summary(&self) -> String {
        match &self.state {
            UpdateState::Disabled => "자동 업데이트 꺼짐".into(),
            UpdateState::Idle => format!("버전 {} (확인 전)", self.current),
            UpdateState::Checking => "업데이트 확인 중...".into(),
            UpdateState::UpToDate => format!("최신 버전입니다 ({})", self.current),
            UpdateState::Downloading { version } => format!("{version} 내려받는 중..."),
            UpdateState::Staged { version } => {
                format!("{version} 준비됨 — 드라이브가 한가할 때 적용합니다")
            }
            UpdateState::Applying { version } => format!("{version} 적용 중..."),
            UpdateState::Failed { reason } => format!("업데이트 실패: {reason}"),
        }
    }
}

// ------------------------------------------------------------- GitHub API

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    /// API URL. Used instead of `browser_download_url` because it is the only
    /// one that works for a private repository (with a token).
    pub url: String,
}

impl Release {
    /// `v0.2.0` and `0.2.0` both mean the same thing.
    pub fn version(&self) -> &str {
        self.tag_name.trim_start_matches(['v', 'V'])
    }

    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// `true` when `candidate` is a strictly newer release than what is running.
///
/// Anything unparseable is treated as "not newer": a malformed tag must never
/// be able to talk the agent into replacing its own binaries.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (
        semver::Version::parse(candidate.trim_start_matches(['v', 'V'])),
        semver::Version::parse(current),
    ) {
        (Ok(new), Ok(now)) => new > now,
        _ => false,
    }
}

/// The triple the release assets are named after.
pub fn target_triple() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", _) => "x86_64-apple-darwin",
        // Windows ships as x64 even on ARM64, where it runs emulated.
        ("windows", _) => "x86_64-pc-windows-msvc",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        _ => "x86_64-unknown-linux-gnu",
    }
}

/// Executables that make up an installation, in the order they are replaced.
/// The agent itself goes last so a half-applied update still has a working one.
pub fn binary_names() -> Vec<String> {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    ["mydrives", "mydrives-ui", "mydrivesd"]
        .iter()
        .map(|n| format!("{n}{suffix}"))
        .collect()
}

/// Asset name for one executable, e.g. `mydrivesd-aarch64-apple-darwin`.
fn asset_name(binary: &str) -> String {
    let stem = binary.strip_suffix(".exe").unwrap_or(binary);
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    format!("{stem}-{}{suffix}", target_triple())
}

fn token(cfg: &UpdateConfig) -> Option<String> {
    if let Ok(t) = std::env::var("MYDRIVES_UPDATE_TOKEN") {
        if !t.trim().is_empty() {
            return Some(t.trim().to_string());
        }
    }
    if !cfg.use_token {
        return None;
    }
    // A private repository needs one; a public one never asks.
    match crate::secret::load(TOKEN_ACCOUNT, &crate::config::SecretRef::Keychain) {
        Ok(Some(t)) if !t.trim().is_empty() => Some(t.trim().to_string()),
        _ => None,
    }
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("MyDrives/{}", current_version()))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| Error::Backend(format!("HTTP 클라이언트를 만들 수 없습니다: {e}")))
}

fn authorized(req: reqwest::RequestBuilder, token: Option<&String>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => req.bearer_auth(t),
        None => req,
    }
}

/// The newest published release, or `None` when the repository has none.
pub async fn fetch_latest(cfg: &UpdateConfig) -> Result<Option<Release>> {
    if cfg.repo.trim().is_empty() {
        return Err(Error::InvalidArgument(
            "업데이트 저장소가 설정되지 않았습니다 ([update] repo)".into(),
        ));
    }
    let token = token(cfg);
    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        cfg.repo.trim()
    );
    let response = authorized(client()?.get(&url), token.as_ref())
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|e| Error::Backend(format!("릴리즈 조회 실패: {e}")))?;

    match response.status().as_u16() {
        200 => {}
        404 => {
            // Also what a private repository looks like without a token, so say
            // both out loud rather than guessing which one it is.
            return if token.is_some() {
                Ok(None)
            } else {
                Err(Error::NotFound(format!(
                    "{} 에 릴리즈가 없거나 비공개 저장소입니다 \
                     (비공개라면 `mydrives update --token-stdin` 으로 토큰을 저장하세요)",
                    cfg.repo
                )))
            };
        }
        401 | 403 => {
            return Err(Error::PermissionDenied(format!(
                "{} 접근이 거부되었습니다 (토큰을 확인하세요)",
                cfg.repo
            )))
        }
        other => {
            return Err(Error::Backend(format!(
                "릴리즈 조회 실패: HTTP {other}"
            )))
        }
    }

    let release: Release = response
        .json()
        .await
        .map_err(|e| Error::Backend(format!("릴리즈 응답을 읽을 수 없습니다: {e}")))?;
    if release.draft || (release.prerelease && !cfg.prerelease) {
        return Ok(None);
    }
    Ok(Some(release))
}

// --------------------------------------------------------------- staging

pub fn staging_root() -> PathBuf {
    crate::config::config_dir().join("updates")
}

/// Version of an update that is downloaded and waiting, if any.
pub fn staged() -> Option<(String, PathBuf)> {
    let root = staging_root();
    let mut best: Option<(semver::Version, PathBuf)> = None;
    for entry in std::fs::read_dir(&root).ok()? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(version) = semver::Version::parse(&name) else {
            continue;
        };
        // Only complete stagings count; `stage()` writes the marker last.
        if !entry.path().join(".complete").exists() {
            continue;
        }
        if best.as_ref().map(|(v, _)| version > *v).unwrap_or(true) {
            best = Some((version, entry.path()));
        }
    }
    best.map(|(v, p)| (v.to_string(), p))
}

/// Downloads every executable of `release` and verifies it against `SHA256SUMS`.
///
/// Returns the staging directory. Reports progress through `on_progress` so the
/// tray can say something more useful than "please wait".
pub async fn stage<F>(cfg: &UpdateConfig, release: &Release, mut on_progress: F) -> Result<PathBuf>
where
    F: FnMut(&str),
{
    let version = release.version().to_string();
    let dir = staging_root().join(&version);
    // A previous attempt may have left a partial directory behind.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let token = token(cfg);
    let http = client()?;

    let sums_asset = release.asset(CHECKSUMS).ok_or_else(|| {
        Error::NotFound(format!("릴리즈 {version} 에 {CHECKSUMS} 파일이 없습니다"))
    })?;
    let sums = download(&http, sums_asset, token.as_ref()).await?;
    let sums = String::from_utf8_lossy(&sums).into_owned();

    // A release that carries nothing for this platform - a build that failed
    // for macOS, say - must fail here. Staging an empty update would "apply"
    // nothing, leave the version unchanged, and have the agent download and
    // restart for the same release again at every check.
    let agent = binary_names()
        .last()
        .cloned()
        .expect("binary_names is never empty");
    if release.asset(&asset_name(&agent)).is_none() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(Error::NotFound(format!(
            "릴리즈 {version} 에 {} 용 파일이 없습니다",
            target_triple()
        )));
    }

    for binary in binary_names() {
        let wanted = asset_name(&binary);
        let Some(asset) = release.asset(&wanted) else {
            // Beyond the agent itself a missing file is tolerated: an older
            // release may not have shipped every executable.
            tracing::warn!(asset = %wanted, "릴리즈에 없는 실행 파일 — 건너뜁니다");
            continue;
        };
        on_progress(&binary);
        let bytes = download(&http, asset, token.as_ref()).await?;

        let expected = checksum_for(&sums, &wanted).ok_or_else(|| {
            Error::Backend(format!("{CHECKSUMS} 에 {wanted} 항목이 없습니다"))
        })?;
        let actual = sha256_hex(&bytes);
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(Error::Backend(format!(
                "{wanted} 체크섬이 다릅니다 (기대 {expected}, 실제 {actual})"
            )));
        }

        let path = dir.join(&binary);
        let mut file = std::fs::File::create(&path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        make_executable(&path)?;
    }

    // Written last: `staged()` treats a directory without it as incomplete.
    std::fs::write(dir.join(".complete"), release.tag_name.as_bytes())?;
    Ok(dir)
}

async fn download(
    http: &reqwest::Client,
    asset: &Asset,
    token: Option<&String>,
) -> Result<Vec<u8>> {
    let response = authorized(http.get(&asset.url), token)
        .header("Accept", "application/octet-stream")
        .send()
        .await
        .map_err(|e| Error::Backend(format!("{} 내려받기 실패: {e}", asset.name)))?;
    if !response.status().is_success() {
        return Err(Error::Backend(format!(
            "{} 내려받기 실패: HTTP {}",
            asset.name,
            response.status().as_u16()
        )));
    }
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| Error::Backend(format!("{} 내려받기 실패: {e}", asset.name)))
}

/// Looks up one entry in a `sha256sum`-style file.
fn checksum_for<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let sum = parts.next()?;
        // `sha256sum` writes "* " for binary mode; both spellings appear.
        let file = parts.next()?.trim_start_matches('*');
        (file == name).then_some(sum)
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

// ----------------------------------------------------------------- apply

/// Directory the running executables live in.
pub fn install_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe()
        .map_err(|e| Error::Backend(format!("실행 파일 경로를 알 수 없습니다: {e}")))?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::Backend("실행 파일의 상위 폴더가 없습니다".into()))
}

/// Swaps the staged executables in.
///
/// Windows refuses to delete a running `.exe` but is perfectly happy to rename
/// it, which is what makes an in-place update possible at all: the old file is
/// moved aside as `<name>.old` and removed on the next start.
pub fn apply(staged_dir: &Path, install_dir: &Path) -> Result<()> {
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    for binary in binary_names() {
        let src = staged_dir.join(&binary);
        if !src.exists() {
            continue;
        }
        let dst = install_dir.join(&binary);
        let old = install_dir.join(format!("{binary}.old"));
        let _ = std::fs::remove_file(&old);
        if dst.exists() {
            if let Err(e) = std::fs::rename(&dst, &old) {
                rollback(&moved);
                return Err(Error::Backend(format!(
                    "{} 를 교체할 수 없습니다: {e}",
                    dst.display()
                )));
            }
            moved.push((old.clone(), dst.clone()));
        }
        // Copy rather than rename: the staging directory is usually on another
        // volume (user config dir vs. install dir), where rename fails.
        if let Err(e) = std::fs::copy(&src, &dst) {
            rollback(&moved);
            return Err(Error::Backend(format!(
                "{} 를 쓸 수 없습니다: {e}",
                dst.display()
            )));
        }
        make_executable(&dst)?;
    }
    let _ = std::fs::remove_dir_all(staged_dir);
    cleanup(install_dir);
    Ok(())
}

fn rollback(moved: &[(PathBuf, PathBuf)]) {
    for (old, dst) in moved.iter().rev() {
        let _ = std::fs::remove_file(dst);
        let _ = std::fs::rename(old, dst);
    }
}

/// Removes the `<name>.old` files left by the previous update. The one
/// belonging to a still-running process cannot be deleted on Windows; it goes
/// away at the next start, which is why this runs on every start.
pub fn cleanup(install_dir: &Path) {
    for binary in binary_names() {
        let _ = std::fs::remove_file(install_dir.join(format!("{binary}.old")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_strictly_newer_versions_update() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("v0.0.9", "0.1.0"));
        // Garbage must never trigger a replacement of our own binaries.
        assert!(!is_newer("latest", "0.1.0"));
        assert!(!is_newer("", "0.1.0"));
    }

    #[test]
    fn asset_names_follow_the_target() {
        let name = asset_name("mydrivesd");
        assert!(name.starts_with("mydrivesd-"), "{name}");
        assert!(name.contains(target_triple()), "{name}");
        assert_eq!(name.ends_with(".exe"), cfg!(windows), "{name}");
    }

    #[test]
    fn checksums_are_matched_by_file_name() {
        let sums = "\
aaaa  mydrivesd-aarch64-apple-darwin
bbbb *mydrives-ui-aarch64-apple-darwin
";
        assert_eq!(
            checksum_for(sums, "mydrivesd-aarch64-apple-darwin"),
            Some("aaaa")
        );
        assert_eq!(
            checksum_for(sums, "mydrives-ui-aarch64-apple-darwin"),
            Some("bbbb")
        );
        assert_eq!(checksum_for(sums, "nope"), None);
    }

    #[test]
    fn sha256_matches_the_reference_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn the_agent_is_replaced_last() {
        // A half-applied update must still leave a runnable agent behind.
        let names = binary_names();
        assert!(names.last().unwrap().starts_with("mydrivesd"));
    }
}

#[cfg(test)]
mod apply_tests {
    use super::*;

    /// The swap must leave the install directory with the new files and the old
    /// ones moved aside - that is the step that can brick an installation, and
    /// it is the one part that runs with no network and no GitHub involved.
    #[test]
    fn staged_files_replace_the_installed_ones() {
        let root = std::env::temp_dir().join(format!("mydrives-apply-{}", std::process::id()));
        let staged = root.join("staged");
        let install = root.join("install");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::create_dir_all(&install).unwrap();

        let names = binary_names();
        for name in &names {
            std::fs::write(install.join(name), b"old").unwrap();
            std::fs::write(staged.join(name), b"new").unwrap();
        }

        apply(&staged, &install).expect("apply");

        for name in &names {
            assert_eq!(std::fs::read(install.join(name)).unwrap(), b"new");
            // `cleanup` ran, so nothing is left lying around.
            assert!(!install.join(format!("{name}.old")).exists(), "{name}.old");
        }
        // The staging directory is consumed, so it cannot be applied twice.
        assert!(!staged.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A release that ships only some of the executables must not wipe the rest.
    #[test]
    fn missing_files_leave_the_installed_ones_alone() {
        let root = std::env::temp_dir().join(format!("mydrives-partial-{}", std::process::id()));
        let staged = root.join("staged");
        let install = root.join("install");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::create_dir_all(&install).unwrap();

        let names = binary_names();
        for name in &names {
            std::fs::write(install.join(name), b"old").unwrap();
        }
        std::fs::write(staged.join(&names[0]), b"new").unwrap();

        apply(&staged, &install).expect("apply");

        assert_eq!(std::fs::read(install.join(&names[0])).unwrap(), b"new");
        for name in &names[1..] {
            assert_eq!(std::fs::read(install.join(name)).unwrap(), b"old");
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod release_tests {
    use super::*;

    fn release_with(assets: &[&str]) -> Release {
        Release {
            tag_name: "v9.9.9".into(),
            name: String::new(),
            prerelease: false,
            draft: false,
            assets: assets
                .iter()
                .map(|name| Asset {
                    name: (*name).to_string(),
                    url: format!("https://example.invalid/{name}"),
                })
                .collect(),
        }
    }

    #[test]
    fn a_release_without_this_platform_is_not_a_usable_update() {
        // The agent's own binary is what makes an update worth applying: a
        // release built for another OS must be reported as missing instead of
        // being staged empty and "applied" on every check, forever.
        let ours = asset_name(binary_names().last().unwrap());
        assert!(release_with(&[&ours]).asset(&ours).is_some());
        assert!(release_with(&["mydrivesd-some-other-target"])
            .asset(&ours)
            .is_none());
    }
}
