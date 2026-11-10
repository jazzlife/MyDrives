//! What a drive is called.
//!
//! Two different names, on purpose:
//!
//! * the **id** is the share name — it is the config key, the keychain account
//!   and the name of the mount point, so `~/MyDrives/Public` matches the remote
//!   `Public` folder and nothing has to be looked up to know what is what;
//! * the **label** is what the user reads, and it is named after the *product*:
//!   the NAS, or the service. "turtlelab" tells you which box it is; "Public"
//!   does not.
//!
//! Adding a backend means adding one arm to [`product_name`].

use crate::config::BackendConfig;

/// The product or service this drive comes from.
pub fn product_name(backend: &BackendConfig) -> String {
    match backend {
        BackendConfig::Smb(smb) => nas_name(&smb.host),
        // An FTP account is on a host, not on a product: the server has no name
        // of its own to offer, so the address is the best there is.
        BackendConfig::Ftp(ftp) => nas_name(&ftp.host),
        BackendConfig::Mem(_) => "테스트 볼륨".to_string(),
        // Planned backends name themselves after the service, not the folder:
        //   Mybox(_)    => "네이버 MYBOX"
        //   Gdrive(_)   => "Google Drive"
        //   Onedrive(_) => "OneDrive"
        //   Dropbox(_)  => "Dropbox"
    }
}

/// The NAS's own name, taken from the address the user typed.
///
/// SMB does not hand the client a friendly product name (the negotiate
/// response carries a GUID, and the `smb` crate does not surface the NTLM
/// computer-name attribute), but the first label of the host *is* the name
/// people give their NAS: `turtlelab.ddns.net` is "turtlelab". A bare IP has no
/// name to offer, so it stays "NAS".
fn nas_name(host: &str) -> String {
    let host = host.trim().trim_end_matches('.').trim_matches(['[', ']']);
    if host.is_empty() || host.parse::<std::net::IpAddr>().is_ok() {
        return "NAS".to_string();
    }
    match host.split('.').next() {
        Some(first) if !first.is_empty() => first.to_string(),
        _ => "NAS".to_string(),
    }
}

/// What an FTP drive is called when the user does not say.
///
/// The rule is the one the SMB drives follow - the id is the remote folder, so
/// `~/MyDrives/<id>` matches what is on the server. An FTP account mounted
/// whole has no folder to be named after, so it falls back to the host.
pub fn ftp_default_id(host: &str, subpath: Option<&str>) -> String {
    let leaf = subpath
        .unwrap_or("")
        .trim_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if !leaf.is_empty() {
        return leaf;
    }
    let host = nas_name(host);
    if host == "NAS" {
        "FTP".to_string()
    } else {
        host
    }
}

/// `base`, or something close to it that no other drive is using.
///
/// The share name is the first thing tried as a disambiguator, because two
/// drives from one NAS differ by exactly that: "turtlelab" and
/// "turtlelab (Public)" read better than "turtlelab" and "turtlelab 2".
pub fn unique_label(base: &str, hint: &str, taken: &[String]) -> String {
    let base = match base.trim() {
        "" => "드라이브",
        other => other,
    };
    let is_free = |candidate: &str| !taken.iter().any(|t| t == candidate);
    if is_free(base) {
        return base.to_string();
    }
    let hint = hint.trim();
    if !hint.is_empty() {
        let with_hint = format!("{base} ({hint})");
        if is_free(&with_hint) {
            return with_hint;
        }
    }
    (2..100)
        .map(|n| format!("{base} {n}"))
        .find(|candidate| is_free(candidate))
        .unwrap_or_else(|| base.to_string())
}

/// Label the agent generated before names were based on the product.
///
/// Used to replace `turtlelab.ddns.net/Public` with `turtlelab` on an existing
/// config without touching a name the user chose themselves.
pub fn is_generated_label(label: &str, backend: &BackendConfig) -> bool {
    match backend {
        BackendConfig::Smb(smb) => {
            label == format!("{}/{}", smb.host, smb.share) || label == smb.share
        }
        // FTP drives have had product names from the start, so the only older
        // shape to recognise is the bare host the first version wrote.
        BackendConfig::Ftp(ftp) => label == ftp.host,
        BackendConfig::Mem(_) => label == "in-memory test volume",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MemConfig, SecretRef, SmbConfig};

    fn smb(host: &str, share: &str) -> BackendConfig {
        BackendConfig::Smb(SmbConfig {
            host: host.into(),
            port: 445,
            share: share.into(),
            user: "u".into(),
            subpath: None,
            secret: SecretRef::Keychain,
        })
    }

    #[test]
    fn the_nas_is_named_after_itself_not_the_share() {
        assert_eq!(product_name(&smb("turtlelab.ddns.net", "Public")), "turtlelab");
        assert_eq!(product_name(&smb("QNAP-NAS.local", "data")), "QNAP-NAS");
        assert_eq!(product_name(&smb("nas", "data")), "nas");
    }

    #[test]
    fn an_address_without_a_name_falls_back() {
        assert_eq!(product_name(&smb("192.168.0.10", "data")), "NAS");
        assert_eq!(product_name(&smb("fe80::1", "data")), "NAS");
        assert_eq!(product_name(&smb("  ", "data")), "NAS");
        assert_eq!(
            product_name(&BackendConfig::Mem(MemConfig { fixture: true })),
            "테스트 볼륨"
        );
    }

    #[test]
    fn a_second_share_on_one_nas_is_told_apart_by_the_share() {
        let taken = vec!["turtlelab".to_string()];
        assert_eq!(unique_label("turtlelab", "Public", &taken), "turtlelab (Public)");

        let taken = vec!["turtlelab".into(), "turtlelab (Public)".into()];
        assert_eq!(unique_label("turtlelab", "Public", &taken), "turtlelab 2");
        assert_eq!(unique_label("turtlelab", "", &taken), "turtlelab 2");
    }

    #[test]
    fn only_names_we_generated_are_replaced() {
        let backend = smb("turtlelab.ddns.net", "Public");
        assert!(is_generated_label("turtlelab.ddns.net/Public", &backend));
        assert!(is_generated_label("Public", &backend));
        assert!(!is_generated_label("거북이 NAS", &backend));
    }
}
