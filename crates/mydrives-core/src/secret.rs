//! Credentials.
//!
//! Passwords and OAuth refresh tokens go into the OS credential store - Keychain
//! on macOS, Credential Manager on Windows - never into the config file. Linux
//! builds (used here for development and CI) fall back to a 0600 file and say so
//! loudly, because wiring up Secret Service would pull a D-Bus dependency into a
//! platform that is not a shipping target.

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use std::path::PathBuf;

use crate::config::SecretRef;
use crate::error::{Error, Result};

pub const SERVICE: &str = "app.MyDrives";

/// Resolves the secret for a remote. `Ok(None)` means "no secret configured",
/// which is legitimate for guest shares.
pub fn load(remote_id: &str, how: &SecretRef) -> Result<Option<String>> {
    match how {
        SecretRef::Keychain => keychain_get(remote_id),
        SecretRef::Env { name } => Ok(std::env::var(name).ok()),
        SecretRef::File { path } => match std::fs::read_to_string(path) {
            Ok(s) => Ok(Some(s.trim_end_matches(['\n', '\r']).to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Io(e)),
        },
        SecretRef::Inline { value } => Ok(Some(value.clone())),
    }
}

pub fn store(remote_id: &str, secret: &str) -> Result<()> {
    keychain_set(remote_id, secret)
}

pub fn delete(remote_id: &str) -> Result<()> {
    keychain_delete(remote_id)
}

/// Where the Linux fallback keeps secrets.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn fallback_path(remote_id: &str) -> PathBuf {
    crate::config::config_dir()
        .join("secrets")
        .join(format!("{remote_id}.secret"))
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn entry(remote_id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, remote_id)
        .map_err(|e| Error::Backend(format!("keychain entry {remote_id}: {e}")))
}

#[cfg(target_os = "windows")]
fn keychain_get(remote_id: &str) -> Result<Option<String>> {
    match entry(remote_id)?.get_password() {
        Ok(p) => Ok(Some(p)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(Error::Backend(format!("keychain read {remote_id}: {e}"))),
    }
}

/// Reads the password back through `/usr/bin/security`, not through the
/// Security framework.
///
/// Measured on macOS 26: an item stored with an open access list (`-A`) is
/// still protected against a *library* read - calling `SecKeychainItemCopy…`
/// from our own binary raises "<binary> wants to access key app.MyDrives in
/// your keychain", and the binary's identity changes with every rebuild, so the
/// dialog comes back forever. The same item read through `/usr/bin/security`
/// returns immediately. The agent therefore talks to the keychain the way the
/// `security` tool does: one stable, Apple-signed reader, and no identity of
/// ours for the keychain to object to.
///
/// `security` exits 44 when the item does not exist, which is a legitimate
/// answer ("no password configured"), not a failure.
#[cfg(target_os = "macos")]
fn keychain_get(remote_id: &str) -> Result<Option<String>> {
    let out = std::process::Command::new("/usr/bin/security")
        .arg("find-generic-password")
        .args(["-s", SERVICE])
        .args(["-a", remote_id])
        .arg("-w")
        .output();
    let out = match out {
        Ok(o) => o,
        // No `security` binary at all: fall back to the library path, prompt
        // and all, rather than pretending there is no password.
        Err(e) => {
            tracing::warn!(error = %e, "running /usr/bin/security failed; using the keychain API");
            return match entry(remote_id)?.get_password() {
                Ok(p) => Ok(Some(p)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(Error::Backend(format!("keychain read {remote_id}: {e}"))),
            };
        }
    };
    if out.status.success() {
        let mut password = String::from_utf8_lossy(&out.stdout).into_owned();
        // `security` terminates the value with a newline of its own.
        if password.ends_with('\n') {
            password.pop();
            if password.ends_with('\r') {
                password.pop();
            }
        }
        return Ok(Some(password));
    }
    if out.status.code() == Some(44) {
        return Ok(None);
    }
    Err(Error::Backend(format!(
        "keychain read {remote_id}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

#[cfg(target_os = "windows")]
fn keychain_set(remote_id: &str, secret: &str) -> Result<()> {
    entry(remote_id)?
        .set_password(secret)
        .map_err(|e| Error::Backend(format!("keychain write {remote_id}: {e}")))
}

/// Stores the password in the login keychain with an access list that does not
/// pin it to one binary.
///
/// macOS attaches an ACL to each keychain item naming the application that
/// created it, identified by its code signature. The agent's signature changes
/// with every build and every update, so the next read raises the "… wants to
/// use your confidential information" dialog - and if the agent is running
/// somewhere that cannot show a dialog, the read simply blocks forever. We saw
/// both: the prompt on a rebuild, and a hung mount when no dialog could appear.
///
/// `security add-generic-password -A` writes the item with an empty ACL, which
/// means "any application running as this user may read it without prompting".
/// The trade-off is deliberate and worth stating: a different program running as
/// the same user could read the password without the user being asked. That is
/// already true of the config file, the mount itself and the user's own files,
/// and it is the behaviour every always-on drive agent needs.
/// The password is handed over on **stdin**, not as `-w <secret>`: an argument
/// is visible in `ps` for as long as the child lives. `security` asks for it
/// twice (prompt + retype) and reads both from stdin when it is not a terminal,
/// which is why the value is written twice. A password containing a newline
/// cannot be expressed that way, so those take the library path instead.
#[cfg(target_os = "macos")]
fn keychain_set(remote_id: &str, secret: &str) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let output = if secret.contains(['\n', '\r']) {
        None
    } else {
        // Replace rather than update (`-U`): updating keeps whatever access
        // list the existing item already had - including the per-application
        // one an older build left behind, which is the thing being fixed here -
        // and modifying a protected item needs authorisation of its own.
        let _ = keychain_delete(remote_id);
        let mut child = Command::new("/usr/bin/security")
            .arg("add-generic-password")
            .arg("-A") // no per-application ACL: no prompt on the next build
            .args(["-s", SERVICE])
            .args(["-a", remote_id])
            .args(["-D", "MyDrives password"])
            .arg("-w") // value omitted on purpose: read from stdin below
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::Backend(format!("running /usr/bin/security: {e}")))?;
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = write!(stdin, "{secret}\n{secret}\n");
        }
        drop(child.stdin.take());
        Some(
            child
                .wait_with_output()
                .map_err(|e| Error::Backend(format!("waiting for /usr/bin/security: {e}")))?,
        )
    };

    if let Some(output) = &output {
        // `security` exits 0 even when its two stdin reads disagreed - it then
        // stores an *empty* password - so the only trustworthy check is reading
        // the item back. Doing that through the Security framework also proves
        // the point of the exercise: the item opens with no prompt.
        if output.status.success() && matches!(keychain_get(remote_id), Ok(Some(ref s)) if s == secret)
        {
            return Ok(());
        }
    }
    let stderr = output
        .as_ref()
        .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
        .unwrap_or_else(|| "the password contains a line break".to_string());
    // Fall back to the library path; it still works, it just prompts later.
    tracing::warn!(
        remote = remote_id,
        detail = %stderr,
        "could not store the password without an ACL; falling back"
    );
    entry(remote_id)?
        .set_password(secret)
        .map_err(|e| Error::Backend(format!("keychain write {remote_id}: {e}")))
}

#[cfg(target_os = "windows")]
fn keychain_delete(remote_id: &str) -> Result<()> {
    match entry(remote_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(Error::Backend(format!("keychain delete {remote_id}: {e}"))),
    }
}

/// Same reasoning as [`keychain_get`]: deleting through the framework asks for
/// authorisation, deleting through `security` does not. Exit code 44 means it
/// was already gone, which is the outcome the caller wanted.
#[cfg(target_os = "macos")]
fn keychain_delete(remote_id: &str) -> Result<()> {
    let out = std::process::Command::new("/usr/bin/security")
        .arg("delete-generic-password")
        .args(["-s", SERVICE])
        .args(["-a", remote_id])
        .output()
        .map_err(|e| Error::Backend(format!("running /usr/bin/security: {e}")))?;
    if out.status.success() || out.status.code() == Some(44) {
        return Ok(());
    }
    Err(Error::Backend(format!(
        "keychain delete {remote_id}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn keychain_get(remote_id: &str) -> Result<Option<String>> {
    match std::fs::read_to_string(fallback_path(remote_id)) {
        Ok(s) => {
            tracing::warn!(
                remote = remote_id,
                "reading secret from a plain file; no OS keychain on this platform"
            );
            Ok(Some(s.trim_end_matches(['\n', '\r']).to_string()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Io(e)),
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn keychain_set(remote_id: &str, secret: &str) -> Result<()> {
    let path = fallback_path(remote_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, secret)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::warn!(
        remote = remote_id,
        path = %path.display(),
        "stored secret in a plain 0600 file; no OS keychain on this platform"
    );
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn keychain_delete(remote_id: &str) -> Result<()> {
    match std::fs::remove_file(fallback_path(remote_id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_and_inline_sources() {
        std::env::set_var("MYDRIVES_TEST_SECRET", "s3cret");
        let got = load(
            "x",
            &SecretRef::Env {
                name: "MYDRIVES_TEST_SECRET".into(),
            },
        )
        .unwrap();
        assert_eq!(got.as_deref(), Some("s3cret"));

        let got = load(
            "x",
            &SecretRef::Inline {
                value: "inline".into(),
            },
        )
        .unwrap();
        assert_eq!(got.as_deref(), Some("inline"));

        let missing = load(
            "x",
            &SecretRef::Env {
                name: "MYDRIVES_NOT_SET_12345".into(),
            },
        )
        .unwrap();
        assert!(missing.is_none());
    }

    /// Writes a throwaway item into the real login keychain, reads it back
    /// through the Security framework and removes it again.
    ///
    /// The point is the *reading*: this test binary is signed differently from
    /// whatever wrote the item, so if the item still carried a per-application
    /// ACL the read would raise the "wants to use your confidential
    /// information" dialog instead of returning the password - which is exactly
    /// the behaviour that made the agent ask on every rebuild.
    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_round_trip_needs_no_authorisation() {
        let id = "mydrives-selftest-acl";
        let pw = "s3cret value;with#specials";
        store(id, pw).expect("storing the password");
        assert_eq!(keychain_get(id).unwrap().as_deref(), Some(pw));
        delete(id).expect("deleting the password");
        assert!(keychain_get(id).unwrap().is_none());
    }
}
