//! Makes the compiled-in OAuth client identifiers follow the environment.
//!
//! `option_env!` is read while compiling, so without this cargo would keep
//! a build made before the identifiers were set and the window would still
//! ask for a client id.

fn main() {
    for key in [
        "MYDRIVES_GOOGLE_CLIENT_ID",
        "MYDRIVES_GOOGLE_CLIENT_SECRET",
        "MYDRIVES_MICROSOFT_CLIENT_ID",
        "MYDRIVES_DROPBOX_CLIENT_ID",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
}
