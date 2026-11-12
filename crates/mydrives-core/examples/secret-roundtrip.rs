//! Does a password survive the keychain, whatever it is made of?
//!
//!   cargo run -p mydrives-core --example secret-roundtrip
//!
//! Scratch ids only; it removes them afterwards.
use mydrives_core::config::SecretRef;
use mydrives_core::secret;

fn main() {
    let cases = [
        ("ascii", "plain-pass"),
        ("hexish", "deadbeef"),
        ("korean", "비밀번호-12"),
        ("accents", "paßwörd-ünïcøde"),
        ("newline", "two\nlines"),
        ("tokenset", r#"{"access_token":"a","refresh_token":"b","expires_at":1}"#),
    ];
    let mut bad = 0;
    for (name, value) in cases {
        let id = format!("mydrives-roundtrip-{name}");
        if let Err(e) = secret::store(&id, value) {
            println!("{name:10} store failed: {e}");
            bad += 1;
            continue;
        }
        let back = secret::load(&id, &SecretRef::Keychain);
        let same = matches!(&back, Ok(Some(s)) if s == value);
        println!(
            "{name:10} wrote {:>3} bytes, read {:>3}  identical={same}",
            value.len(),
            match &back {
                Ok(Some(s)) => s.len().to_string(),
                Ok(None) => "none".into(),
                Err(_) => "err".into(),
            }
        );
        if !same {
            bad += 1;
        }
        let _ = secret::delete(&id);
    }
    println!("\n{bad} case(s) did not survive");
    std::process::exit(if bad == 0 { 0 } else { 1 });
}
