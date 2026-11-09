//! Giving up a console window that nobody asked for (Windows).
//!
//! A console binary started from Explorer, the `Run` key or another GUI process
//! gets a console window of its own. For the agent and for the settings window
//! that window is pure noise: it sits on the desktop next to the real UI and
//! quits the program when closed. A console *inherited from a shell* is a
//! different matter - the user is watching it - so it is left alone.
//!
//! Returns the log file to write to when the console was given up, because
//! stdout no longer goes anywhere after that.

use std::path::PathBuf;

/// Maximum size of a log file before it is started over, so a login loop
/// cannot fill the disk.
const MAX_LOG_BYTES: u64 = 4 << 20;

#[cfg(windows)]
pub fn detach_if_sole_owner(log_name: &str) -> Option<std::fs::File> {
    // kernel32 is already linked by std, so this needs no new dependency.
    extern "system" {
        fn GetConsoleProcessList(lpdwProcessList: *mut u32, dwProcessCount: u32) -> u32;
        fn FreeConsole() -> i32;
    }
    let mut pids = [0u32; 4];
    // 0 - no console at all (started detached, which is how the updater
    //     restarts the agent); 1 - a console made for us alone, by Explorer or
    //     the `Run` key; >1 - a shell is sharing it, so the user is watching.
    let count = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) };
    if count > 1 {
        return None;
    }
    let file = open_log(log_name)?;
    if count == 1 {
        unsafe { FreeConsole() };
    }
    Some(file)
}

#[cfg(not(windows))]
pub fn detach_if_sole_owner(log_name: &str) -> Option<std::fs::File> {
    // launchd redirects the agent's output to a file of its own (see
    // `crate::autostart`), and a terminal needs no special handling.
    let _ = log_name;
    None
}

#[cfg(windows)]
fn open_log(log_name: &str) -> Option<std::fs::File> {
    let path = log_path(log_name);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::metadata(&path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(&path);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
}

pub fn log_path(log_name: &str) -> PathBuf {
    crate::config::config_dir().join(log_name)
}
