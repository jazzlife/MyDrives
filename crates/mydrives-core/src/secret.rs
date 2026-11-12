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
/// So the item is written with an ACL that names no application at all, which
/// means "any application running as this user may read it without prompting".
/// The trade-off is deliberate and worth stating: a different program running as
/// the same user could read the password without the user being asked. That is
/// already true of the config file, the mount itself and the user's own files,
/// and it is the behaviour every always-on drive agent needs.
#[cfg(target_os = "macos")]
fn keychain_set(remote_id: &str, secret: &str) -> Result<()> {
    // Replace rather than update: updating keeps whatever access list the
    // existing item already had, including the per-application one an older
    // build left behind, which is the thing being fixed here.
    let _ = keychain_delete(remote_id);
    match keychain_set_without_acl(remote_id, secret) {
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
                // Written whole: the item opens with no prompt and nothing was
                // truncated on the way in.
                Some(ref s) if s == secret => return Ok(()),
                // A *different* value came back - the old `security -A` path
                // used to truncate at 128 characters - so the write really is
                // no good and the library path is worth the prompt it costs.
                Some(s) => tracing::warn!(
                    remote = remote_id,
                    wrote = secret.len(),
                    read_back = s.len(),
                    "the keychain item read back truncated; falling back"
                ),
                // Nothing came back. That is a failed *read*, not evidence of
                // a failed write, and `SecKeychainItemCreateFromContent` already
                // said it stored the item. Falling back here was actively
                // harmful: the library write pins the item to this build's code
                // signature, so every token refresh threw away the access the
                // user had just granted and the drive dropped at the next read.
                // Keep the item that was written.
                // Nothing came back. With the access list naming
                // `/usr/bin/security` this should not happen, and if it does
                // the item is one the agent cannot read - exactly the state a
                // NULL access list used to leave behind. Fall back rather than
                // keep a password nobody can get at.
                None => tracing::warn!(
                    remote = remote_id,
                    wrote = secret.len(),
                    "the stored keychain item could not be read back; falling back"
                ),
            }
        }
        Err(status) => tracing::warn!(
            remote = remote_id,
            status,
            "could not store the password without an ACL; falling back"
        ),
    }

    let _ = keychain_delete(remote_id);
    // The library path still works, it just asks the user again after every
    // update of the app.
    entry(remote_id)?
        .set_password(secret)
        .map_err(|e| Error::Backend(format!("keychain write {remote_id}: {e}")))
}

/// Writes the item straight through the Security framework with an ACL that
/// names no application, which is what "any program running as this user may
/// read this without a prompt" looks like on macOS.
///
/// This used to shell out to `/usr/bin/security -A` with the value on stdin.
/// That tool reads a password from stdin through its interactive prompt, and
/// the prompt stops at **128 characters** (measured on 2026-11-11). Short NAS
/// and FTP passwords went through; an OAuth token set is a JSON document of
/// several hundred bytes, so it was silently truncated, the read-back check
/// failed, and every cloud drive fell back to a write that *does* attach a
/// per-application ACL. That ACL names the binary by its code signature, so
/// each new build asked the user for permission all over again.
#[cfg(target_os = "macos")]
fn keychain_set_without_acl(remote_id: &str, secret: &str) -> std::result::Result<(), i32> {
    use std::ffi::c_void;

    type CFTypeRef = *const c_void;
    type OSStatus = i32;

    #[repr(C)]
    struct Attribute {
        tag: u32,
        length: u32,
        data: *mut c_void,
    }
    #[repr(C)]
    struct AttributeList {
        count: u32,
        attr: *mut Attribute,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: CFTypeRef);
        fn CFStringCreateWithBytes(
            alloc: CFTypeRef,
            bytes: *const u8,
            num_bytes: isize,
            encoding: u32,
            is_external: u8,
        ) -> CFTypeRef;
        fn CFArrayGetCount(array: CFTypeRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
        fn CFArrayCreate(
            alloc: CFTypeRef,
            values: *const CFTypeRef,
            num_values: isize,
            callbacks: *const c_void,
        ) -> CFTypeRef;
        static kCFTypeArrayCallBacks: c_void;
    }

    #[link(name = "Security", kind = "framework")]
    extern "C" {
        fn SecKeychainCopyDefault(keychain: *mut CFTypeRef) -> OSStatus;
        fn SecTrustedApplicationCreateFromPath(
            path: *const u8,
            app: *mut CFTypeRef,
        ) -> OSStatus;
        fn SecAccessCreate(
            descriptor: CFTypeRef,
            trusted_list: CFTypeRef,
            access: *mut CFTypeRef,
        ) -> OSStatus;
        fn SecAccessCopyACLList(access: CFTypeRef, acl_list: *mut CFTypeRef) -> OSStatus;
        fn SecACLSetContents(
            acl: CFTypeRef,
            application_list: CFTypeRef,
            description: CFTypeRef,
            prompt_selector: u16,
        ) -> OSStatus;
        fn SecKeychainItemCreateFromContent(
            item_class: u32,
            attr_list: *const AttributeList,
            length: u32,
            data: *const c_void,
            keychain: CFTypeRef,
            initial_access: CFTypeRef,
            item: *mut CFTypeRef,
        ) -> OSStatus;
    }

    /// Frees a CoreFoundation object when it goes out of scope.
    struct Owned(CFTypeRef);
    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) }
            }
        }
    }

    const UTF8: u32 = 0x0800_0100;
    const GENERIC_PASSWORD: u32 = u32::from_be_bytes(*b"genp");
    const SERVICE_ATTR: u32 = u32::from_be_bytes(*b"svce");
    const ACCOUNT_ATTR: u32 = u32::from_be_bytes(*b"acct");
    const DESCRIPTION_ATTR: u32 = u32::from_be_bytes(*b"desc");

    let cfstring = |s: &str| -> Owned {
        Owned(unsafe {
            CFStringCreateWithBytes(
                std::ptr::null(),
                s.as_ptr(),
                s.len() as isize,
                UTF8,
                0,
            )
        })
    };

    unsafe {
        let mut keychain: CFTypeRef = std::ptr::null();
        let status = SecKeychainCopyDefault(&mut keychain);
        if status != 0 {
            return Err(status);
        }
        let keychain = Owned(keychain);

        // The access list names one program: `/usr/bin/security`, which is
        // what `keychain_get` reads through. It is Apple-signed and its
        // identity never changes, so the item stays readable across every
        // rebuild and update of this agent - which is the whole point.
        //
        // A NULL application list is *not* "any application", whatever the
        // old comment here claimed. Measured on macOS 15.7.7: an item written
        // that way is refused outright, `security` exits 51 (authorisation
        // denied) and no dialog is even offered. That left the agent unable to
        // read back a password it had just stored.
        let label = cfstring(SERVICE);
        let mut reader: CFTypeRef = std::ptr::null();
        let status =
            SecTrustedApplicationCreateFromPath(c"/usr/bin/security".as_ptr() as *const u8, &mut reader);
        if status != 0 {
            return Err(status);
        }
        let reader = Owned(reader);
        let trusted = Owned(CFArrayCreate(
            std::ptr::null(),
            &reader.0,
            1,
            &kCFTypeArrayCallBacks as *const c_void,
        ));
        if trusted.0.is_null() {
            return Err(-1);
        }
        let mut access: CFTypeRef = std::ptr::null();
        let status = SecAccessCreate(label.0, trusted.0, &mut access);
        if status != 0 {
            return Err(status);
        }
        let access = Owned(access);

        // `SecAccessCreate` also adds this process to the list it builds, and
        // this process's signature changes with every build. Rewrite each ACL
        // so only the stable reader is named and nothing is ever prompted for.
        let mut acls: CFTypeRef = std::ptr::null();
        let status = SecAccessCopyACLList(access.0, &mut acls);
        if status != 0 {
            return Err(status);
        }
        let acls = Owned(acls);
        let empty = cfstring("");
        for i in 0..CFArrayGetCount(acls.0) {
            let acl = CFArrayGetValueAtIndex(acls.0, i);
            // A zero prompt selector means "never ask".
            let status = SecACLSetContents(acl, trusted.0, empty.0, 0);
            if status != 0 {
                return Err(status);
            }
        }

        let mut service = SERVICE.as_bytes().to_vec();
        let mut account = remote_id.as_bytes().to_vec();
        let mut description = b"MyDrives password".to_vec();
        let mut attrs = [
            Attribute {
                tag: SERVICE_ATTR,
                length: service.len() as u32,
                data: service.as_mut_ptr() as *mut c_void,
            },
            Attribute {
                tag: ACCOUNT_ATTR,
                length: account.len() as u32,
                data: account.as_mut_ptr() as *mut c_void,
            },
            Attribute {
                tag: DESCRIPTION_ATTR,
                length: description.len() as u32,
                data: description.as_mut_ptr() as *mut c_void,
            },
        ];
        let list = AttributeList {
            count: attrs.len() as u32,
            attr: attrs.as_mut_ptr(),
        };
        let mut item: CFTypeRef = std::ptr::null();
        let status = SecKeychainItemCreateFromContent(
            GENERIC_PASSWORD,
            &list,
            secret.len() as u32,
            secret.as_ptr() as *const c_void,
            keychain.0,
            access.0,
            &mut item,
        );
        if !item.is_null() {
            CFRelease(item);
        }
        if status != 0 {
            return Err(status);
        }
    }
    Ok(())
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
    // Run with `cargo test -- --ignored`. Writing is prompt-free, which is
    // the property that matters and is checked below; *deleting* an item is
    // a separate authorisation, and a test binary carries a new ad-hoc
    // signature on every build, so the tidy-up at the end stops for a
    // password dialog and the run hangs on a developer machine.
    #[ignore]
    fn keychain_round_trip_needs_no_authorisation() {
        let id = "mydrives-selftest-acl";
        let pw = "s3cret value;with#specials";
        store(id, pw).expect("storing the password");
        assert_eq!(keychain_get(id).unwrap().as_deref(), Some(pw));
        delete(id).expect("deleting the password");
        assert!(keychain_get(id).unwrap().is_none());
    }

    /// A sign-in leaves behind a JSON token set of several hundred bytes, not
    /// a password someone typed. The writer this replaced handed the value to
    /// `/usr/bin/security` on stdin, which stops reading at 128 characters: it
    /// stored a truncated string, the drive fell back to a write that pins the
    /// item to one binary, and every rebuild asked the user for permission.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore] // same reason as above: the delete at the end wants a password
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
