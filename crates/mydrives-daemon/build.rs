//! Link arguments that only apply to the executable.
//!
//! Both of these have to live in the package that produces the binary: cargo
//! only applies `rustc-link-arg` to targets of the package whose build script
//! emitted it, so putting them in a mount-provider rlib has no effect on the
//! linked program.

fn main() {
    // Delay-load WinFsp. Without it `mydrivesd.exe` fails to start with
    // STATUS_DLL_NOT_FOUND (0xC0000135) on a machine where WinFsp is not
    // installed - before `main` runs, so the user gets no message at all.
    #[cfg(windows)]
    winfsp::build::winfsp_link_delayload();

    // FUSE-T installs `libfuse-t.dylib` with an `@rpath` install name, so the
    // program has to carry the search path itself; pkg-config's `-Wl,-rpath`
    // is dropped by the `pkg-config` crate, which forwards only `-L`/`-l`.
    // Weakly linked: a Mac without FUSE-T must still run the agent (the in-box
    // NFS provider needs nothing installed), and `mydrives_mount_fuse` checks
    // the library is really there before mounting.
    #[cfg(all(target_os = "macos", feature = "macfuse"))]
    {
        println!("cargo::rustc-link-arg-bins=-Wl,-rpath,/usr/local/lib");
        println!("cargo::rustc-link-arg-bins=-Wl,-weak-lfuse-t");
    }
}
