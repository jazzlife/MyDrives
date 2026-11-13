//! Rewrite each drive's stored secret so the reader is also its creator.
//!
//! A keychain item's **partition list** - the code identities that may open it
//! without asking - starts out holding whoever created the item. Items an older
//! build wrote from inside the agent therefore name our cdhash and leave out
//! `apple-tool:`, the identity of the `/usr/bin/security` that reads them, so
//! every read raises the dialog that asks for the login keychain password (see
//! `docs/DECISIONS.md` §17).
//!
//! This reads each secret while it is still readable and stores it again, now
//! through `security`, so the conversion costs no authorisation dialog. Run it
//! once, with the agent stopped:
//!
//!   cargo run -p mydrives-core --example reseal-keychain
use mydrives_core::config::SecretRef;
use mydrives_core::secret;
use std::process::Command;

fn description(id: &str) -> String {
    let out = Command::new("/usr/bin/security")
        .arg("find-generic-password")
        .args(["-s", secret::SERVICE])
        .args(["-a", id])
        .output()
        .expect("security");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().strip_prefix("\"desc\"<blob>=").map(str::to_string))
        .unwrap_or_else(|| "<none>".into())
}

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .init();
    let config = mydrives_core::config::AppConfig::load().expect("config");
    let mut failed = 0;
    for remote in &config.remotes {
        let id = &remote.id;
        let before = description(id);
        let value = match secret::load(id, &SecretRef::Keychain) {
            Ok(Some(v)) => v,
            Ok(None) => {
                println!("{id:14} no secret stored - skipped");
                continue;
            }
            // An item written with an access list that trusts nobody cannot be
            // read at all, by anyone. Say so and move on: the others are still
            // worth converting, and this one needs its password typed again.
            Err(e) => {
                println!("{id:14} UNREADABLE ({e}) - needs the password entered again");
                failed += 1;
                continue;
            }
        };
        if let Err(e) = secret::store(id, &value) {
            println!("{id:14} FAILED to store: {e}");
            failed += 1;
            continue;
        }
        let after = description(id);
        // The read straight after a write is the very thing that fails here,
        // so give it the same few tries the agent now does.
        let mut back = None;
        for _ in 0..5 {
            if let Ok(Some(s)) = secret::load(id, &SecretRef::Keychain) {
                back = Some(s);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        // A read right after the write is the thing that used to ask, so a
        // value that comes back whole is the proof the conversion worked.
        let intact = back.as_deref() == Some(value.as_str());
        let len = value.len();
        println!("{id:14} {len} bytes  desc {before} -> {after}  value intact={intact}");
        if !intact {
            failed += 1;
        }
    }
    println!("\n{failed} drive(s) whose secret could not be rewritten");
    std::process::exit(if failed == 0 { 0 } else { 1 });
}
