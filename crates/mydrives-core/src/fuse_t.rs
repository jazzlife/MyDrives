//! FUSE-T: the file system layer macOS drives mount through.
//!
//! macOS has no supported way for a user-space network mount to appear in
//! Finder's sidebar: that list is built from DiskArbitration volumes and from
//! servers mounted through NetFS, and a mount made with `mount_nfs(8)` is
//! neither (measured on macOS 26 - the volume is usable everywhere else, it
//! just is not listed). FUSE-T mounts the way Finder expects, so the drive
//! shows up next to the external disks.
//!
//! It is a user-space implementation - no kernel extension, no reboot - but it
//! is still a system-wide install, so the agent installs it the way a person
//! would: download the official package and hand it to `installer`, which asks
//! for an administrator password.

use std::path::Path;

use crate::error::{Error, Result};

/// The dynamic library the mount provider links against, weakly, so that a Mac
/// without FUSE-T still runs the agent.
pub const LIBRARY: &str = "/usr/local/lib/libfuse-t.dylib";

/// Where the official packages are published.
const RELEASES: &str = "https://api.github.com/repos/macos-fuse-t/fuse-t/releases/latest";

pub fn is_installed() -> bool {
    Path::new(LIBRARY).exists()
}

/// Version string taken from the versioned file the symlink points at
/// (`libfuse-t-1.2.9.dylib`), or `None` when FUSE-T is not installed.
pub fn installed_version() -> Option<String> {
    let target = std::fs::read_link(LIBRARY).ok()?;
    let name = target.file_name()?.to_string_lossy().into_owned();
    let version = name
        .strip_prefix("libfuse-t-")?
        .strip_suffix(".dylib")?
        .to_string();
    Some(version)
}

/// Installs FUSE-T if it is missing. Returns whether it is available now.
///
/// The `installer` run needs administrator rights, which means a password
/// dialog; that dialog *is* the consent, so there is no second prompt of our
/// own. A user who cancels it gets `Ok(false)` and keeps the NFS provider.
#[cfg(target_os = "macos")]
pub async fn ensure_installed() -> Result<bool> {
    if is_installed() {
        return Ok(true);
    }
    let (version, url) = latest_package().await?;
    tracing::info!(version = %version, "FUSE-T를 내려받습니다");

    let bytes = reqwest::Client::builder()
        .user_agent(format!("MyDrives/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| Error::Backend(format!("HTTP 클라이언트를 만들 수 없습니다: {e}")))?
        .get(&url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| Error::Backend(format!("FUSE-T 내려받기 실패: {e}")))?
        .bytes()
        .await
        .map_err(|e| Error::Backend(format!("FUSE-T 내려받기 실패: {e}")))?;

    // Into the agent's own directory, not /tmp: an installer package handed to
    // a privileged process must not sit somewhere any user can swap it out
    // between the write and the install.
    let dir = crate::config::config_dir().join("downloads");
    std::fs::create_dir_all(&dir)?;
    let pkg = dir.join(format!("fuse-t-{version}.pkg"));
    std::fs::write(&pkg, &bytes)?;

    let outcome = install_package(&pkg);
    let _ = std::fs::remove_file(&pkg);
    outcome?;

    if is_installed() {
        tracing::info!(version = %version, "FUSE-T 설치 완료");
        Ok(true)
    } else {
        Err(Error::Backend(
            "설치는 끝났는데 libfuse-t.dylib이 보이지 않습니다".into(),
        ))
    }
}

#[cfg(not(target_os = "macos"))]
pub async fn ensure_installed() -> Result<bool> {
    Ok(false)
}

/// Newest published package: `(version, download url)`.
#[cfg(target_os = "macos")]
async fn latest_package() -> Result<(String, String)> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
        #[serde(default)]
        assets: Vec<Asset>,
    }
    #[derive(serde::Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
    }

    let release: Release = reqwest::Client::builder()
        .user_agent(format!("MyDrives/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| Error::Backend(format!("HTTP 클라이언트를 만들 수 없습니다: {e}")))?
        .get(RELEASES)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| Error::Backend(format!("FUSE-T 릴리즈 조회 실패: {e}")))?
        .json()
        .await
        .map_err(|e| Error::Backend(format!("FUSE-T 릴리즈 응답을 읽을 수 없습니다: {e}")))?;

    let asset = release
        .assets
        .iter()
        .find(|a| a.name.ends_with(".pkg"))
        .ok_or_else(|| {
            Error::NotFound(format!(
                "FUSE-T {} 릴리즈에 .pkg 파일이 없습니다",
                release.tag_name
            ))
        })?;
    Ok((
        release.tag_name.trim_start_matches(['v', 'V']).to_string(),
        asset.browser_download_url.clone(),
    ))
}

/// Runs `installer(8)` with administrator rights.
#[cfg(target_os = "macos")]
fn install_package(pkg: &Path) -> Result<()> {
    // The path goes through AppleScript, so a quote in it would end the string
    // early. Our own directory never contains one, but refusing is cheaper
    // than reasoning about what the shell would do with it.
    let path = pkg.display().to_string();
    if path.contains('"') || path.contains('\\') {
        return Err(Error::InvalidArgument(format!(
            "설치 파일 경로에 따옴표가 있습니다: {path}"
        )));
    }
    let script = format!(
        r#"do shell script "/usr/sbin/installer -pkg \"{path}\" -target /" with administrator privileges"#
    );
    let output = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .map_err(|e| Error::Backend(format!("osascript 실행 실패: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    // -128 is what AppleScript reports when the user clicks Cancel.
    if stderr.contains("-128") {
        return Err(Error::PermissionDenied(
            "FUSE-T 설치를 취소했습니다".into(),
        ));
    }
    Err(Error::Backend(format!(
        "FUSE-T 설치 실패: {}",
        stderr.trim()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comes_from_the_symlink_target() {
        // Only meaningful where FUSE-T is installed; everywhere else the
        // important part is that it answers instead of panicking.
        match (is_installed(), installed_version()) {
            (true, Some(v)) => assert!(v.chars().next().unwrap().is_ascii_digit(), "{v}"),
            (true, None) => {} // installed as a real file rather than a symlink
            (false, v) => assert!(v.is_none()),
        }
    }
}
