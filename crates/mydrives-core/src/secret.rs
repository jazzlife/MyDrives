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
/// Measured on macOS 26: a read from our own binary raises "<binary> wants to
/// access key app.MyDrives in your keychain", and our identity changes with
/// every rebuild and every update, so the dialog comes back forever. An
/// Apple-signed tool has one identity that never changes, so the agent talks
/// to the keychain the way `security` does and keeps its own identity out of
/// the decision. [`keychain_set`] creates the item through the same tool, which
/// is what keeps this read from asking.
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
        return Ok(Some(decode_if_hex_dump(remote_id, password)));
    }
    if out.status.code() == Some(44) {
        return Ok(None);
    }
    Err(Error::Backend(format!(
        "keychain read {remote_id}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}


/// Turns `security`'s hex dump back into the password it stands for.
///
/// `security find-generic-password -w` prints the value as text only while it
/// is printable; anything else - a Korean or accented password, one with a
/// newline in it - comes out as a bare hex dump. The agent used to hand that
/// dump to the server as the password, so a non-ASCII password could never
/// authenticate, and `keychain_set`'s read-back saw 24 characters where it
/// wrote 12 and fell back to a write that re-prompts on every rebuild.
///
/// A password may legitimately look like hex ("deadbeef"), so the shape alone
/// cannot decide it. `-g` can: it quotes a text value and prefixes a dumped
/// one with `0x`. That second call only happens for a value that *could* be a
/// dump, which is rare, so the ordinary read stays one call.
#[cfg(target_os = "macos")]
fn decode_if_hex_dump(remote_id: &str, value: String) -> String {
    let could_be = !value.is_empty()
        && value.len() % 2 == 0
        && value.bytes().all(|b| b.is_ascii_hexdigit());
    if !could_be {
        return value;
    }
    let Ok(out) = std::process::Command::new("/usr/bin/security")
        .arg("find-generic-password")
        .args(["-s", SERVICE])
        .args(["-a", remote_id])
        .arg("-g")
        .output()
    else {
        return value;
    };
    // `-g` writes the attributes to stderr.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    let dumped = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("password: "))
        .is_some_and(|v| v.starts_with("0x"));
    if !dumped {
        return value;
    }
    match hex_to_string(&value) {
        Some(decoded) => decoded,
        // Bytes that are not text after all. Nothing sensible to return but
        // what we were given; the caller will fail to authenticate and say so,
        // which beats pretending.
        None => {
            tracing::warn!(
                remote = remote_id,
                "the stored password is not text; using it as it came back"
            );
            value
        }
    }
}

#[cfg(target_os = "macos")]
fn hex_to_string(hex: &str) -> Option<String> {
    let bytes: Option<Vec<u8>> = hex
        .as_bytes()
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).ok()?, 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}
#[cfg(target_os = "windows")]
fn keychain_set(remote_id: &str, secret: &str) -> Result<()> {
    entry(remote_id)?
        .set_password(secret)
        .map_err(|e| Error::Backend(format!("keychain write {remote_id}: {e}")))
}

/// Stores the password by letting `/usr/bin/security` create the item.
///
/// Every keychain item carries, besides its access list, a **partition list**:
/// the set of code identities allowed to open it without asking. The list is
/// fixed when the item is created and starts out holding the identity of
/// whoever created it - `teamid:…` for a signed app, `cdhash:…` for an ad-hoc
/// build, `apple-tool:` for one of Apple's command line tools. A reader whose
/// identity is not on the list gets the dialog that asks for the login
/// keychain password, and the read blocks until it is answered.
///
/// The agent reads through `/usr/bin/security` (see [`keychain_get`]), whose
/// identity is `apple-tool:`. Creating the item from *our* process put our own
/// cdhash on the list and left `apple-tool:` off it, so the very next read
/// asked. Measured on macOS 26 (2026-11-13), straight from the system log:
///
/// ```text
/// mydrivesd  SecKeychainItemCreateFromContent
/// securityd  ACL partition mismatch: client apple-tool:
/// securityd  asking user about XARA partition for 'apple-tool:'
/// securityd  displaying keychain prompt for /usr/bin/security(1661)
/// ```
///
/// Answering "항상 허용" adds `apple-tool:` to *that one item*, and a token
/// refresh replaces the item, so the dialog came back at the next start of the
/// agent - three cloud drives, three dialogs, every login. No access list can
/// fix that: the partition list is a separate check, which is why writing with
/// no ACL and writing with an ACL that names `security` both still asked.
///
/// So the item is created by the same program that reads it. `security` is
/// Apple-signed and its identity never changes, so the partition list says
/// `apple-tool:` for good and no build of ours ever has to appear on it.
///
/// The trade-off is the one already written down for reading: any program
/// running as this user can get the password out through `security`. The value
/// also passes through the command line of a short-lived child, where `ps`
/// shows it to that same user - the same set of programs that could simply ask
/// `security` for it. It goes as hex (`-X`) so that a NUL byte or a leading `-`
/// in a password cannot change what the command means.
#[cfg(target_os = "macos")]
fn keychain_set(remote_id: &str, secret: &str) -> Result<()> {
    // Delete rather than update: `-U` keeps the partition list of whatever item
    // is already there, including the one an older build created, which is the
    // thing being fixed here. A new item starts with a new list.
    let _ = keychain_delete(remote_id);
    match security_add(remote_id, secret) {
        Ok(()) => {
            // Read it back to catch a short write. `security` is a separate
            // process and does not always see a just-created item on the
            // first try, so give it a few attempts.
            let mut answer = None;
            for attempt in 0..5 {
                match keychain_get(remote_id) {
                    Ok(Some(s)) => {
                        answer = Some(s);
                        break;
                    }
                    // No item yet, or the read itself failed: try again.
                    Ok(None) | Err(_) if attempt < 4 => {
                        std::thread::sleep(std::time::Duration::from_millis(150))
                    }
                    other => {
                        tracing::debug!(
                            remote = remote_id,
                            result = ?other.as_ref().map(|o| o.as_ref().map(|s| s.len())),
                            "the written keychain item could not be read back"
                        );
                    }
                }
            }
            match answer {
                // Written whole, and openable without a dialog - the two
                // things that have gone wrong here before.
                Some(ref s) if s == secret => return Ok(()),
                // A *different* value came back. An older writer handed the
                // value to `security` on stdin, where it was cut off at 128
                // characters; nothing should truncate it now, so if it happens
                // the write is no good and the library path is worth the
                // prompt it costs.
                Some(s) => tracing::warn!(
                    remote = remote_id,
                    wrote = secret.len(),
                    read_back = s.len(),
                    "the keychain item read back truncated; falling back"
                ),
                // Nothing came back at all: the item exists but this reader
                // cannot open it, which is the state the fix is meant to
                // prevent. Fall back rather than keep a password nobody can
                // get at.
                None => tracing::warn!(
                    remote = remote_id,
                    wrote = secret.len(),
                    "the stored keychain item could not be read back; falling back"
                ),
            }
        }
        Err(e) => tracing::warn!(
            remote = remote_id,
            error = %e,
            "could not store the password through /usr/bin/security; falling back"
        ),
    }

    let _ = keychain_delete(remote_id);
    // The library path still works, it just asks the user again whenever the
    // signature of the build that reads it differs from the one that wrote it.
    entry(remote_id)?
        .set_password(secret)
        .map_err(|e| Error::Backend(format!("keychain write {remote_id}: {e}")))
}

/// Creates the item with `security add-generic-password`, value as hex.
///
/// There is no length limit on the way in: an OAuth token set of a couple of
/// thousand characters arrives whole, which the writer this replaced - the same
/// tool with the value on stdin - did not manage.
#[cfg(target_os = "macos")]
fn security_add(remote_id: &str, secret: &str) -> std::result::Result<(), String> {
    use std::fmt::Write as _;

    let mut hex = String::with_capacity(secret.len() * 2);
    for byte in secret.as_bytes() {
        let _ = write!(hex, "{byte:02x}");
    }
    let out = std::process::Command::new("/usr/bin/security")
        .arg("add-generic-password")
        .args(["-s", SERVICE])
        .args(["-a", remote_id])
        .args(["-D", "MyDrives password"])
        .args(["-X", &hex])
        // The delete above should have left nothing behind, but a write that
        // refused because the item is still there would lose a token.
        .arg("-U")
        .output()
        .map_err(|e| format!("running /usr/bin/security: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "{} ({})",
        String::from_utf8_lossy(&out.stderr).trim(),
        out.status
    ))
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

    /// `security` prints a non-printable value as a hex dump, so the decoder
    /// has to turn it back. A password that merely looks like hex must be
    /// left alone, which is why the shape alone never decides it.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_hex_dump_becomes_the_bytes_it_stands_for() {
        // "비밀번호" and "two\nlines", as `security -w` prints them.
        assert_eq!(hex_to_string("ebb984ebb080ebb288ed98b8").as_deref(), Some("비밀번호"));
        assert_eq!(hex_to_string("74776f0a6c696e6573").as_deref(), Some("two\nlines"));
        assert_eq!(hex_to_string("EBB984EBB080EBB288ED98B8").as_deref(), Some("비밀번호"));
        // Not a dump at all.
        assert_eq!(hex_to_string("zz").as_deref(), None);
        assert_eq!(hex_to_string("abc").as_deref(), None);
        // Valid hex whose bytes are not text: the caller keeps the raw form.
        assert_eq!(hex_to_string("ff").as_deref(), None);
    }
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

    /// Writes a throwaway item into the real login keychain, reads it back and
    /// removes it again - the whole cycle the agent runs on every token
    /// refresh, which is where the keychain dialogs came from.
    ///
    /// Each step goes through `/usr/bin/security`, so the item's partition
    /// list names the one identity that reads it and nothing asks the user.
    /// A step that starts prompting shows up here as a test that hangs, which
    /// is the warning worth having.
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

    /// A sign-in leaves behind a JSON token set of several hundred bytes, not
    /// a password someone typed. An older writer handed the value to
    /// `/usr/bin/security` on stdin, which stops reading at 128 characters: it
    /// stored a truncated string, and the fallback it triggered pinned the item
    /// to one build, so every rebuild asked the user for permission.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_token_set_is_stored_whole_however_long_it_is() {
        let id = "mydrives-selftest-long";
        let pw = format!(
            r#"{{"access_token":"{}","refresh_token":"{}","expires_at":1794000000}}"#,
            "y29.".to_string() + &"A1b2C3d4".repeat(240),
            "1//0e".to_string() + &"Z9y8X7w6".repeat(12),
        );
        assert!(pw.len() > 1900, "the sample is longer than any prompt buffer");
        store(id, &pw).expect("storing the token set");
        assert_eq!(keychain_get(id).unwrap().as_deref(), Some(pw.as_str()));
        delete(id).expect("deleting the token set");
        assert!(keychain_get(id).unwrap().is_none());
    }
}
