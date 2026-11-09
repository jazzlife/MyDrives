//! Delay-load the WinFsp DLL.
//!
//! Without this the agent's import table references `winfsp-x64.dll` at load
//! time, so on a machine where WinFsp is not installed the process dies with
//! STATUS_DLL_NOT_FOUND (0xC0000135) before `main` runs - the user sees nothing
//! at all. With delay loading, start-up succeeds and the tray can tell them to
//! install WinFsp.

fn main() {
    #[cfg(windows)]
    winfsp::build::winfsp_link_delayload();
}
