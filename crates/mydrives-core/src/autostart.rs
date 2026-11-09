//! Starting the agent when the user logs in.
//!
//! Deliberately a **per-user** registration on every platform, never a system
//! service. On Windows a drive letter belongs to a logon session, so a mount
//! made by a service in session 0 is invisible in the user's Explorer; on macOS
//! only a LaunchAgent (not a LaunchDaemon) gets a GUI session, which the menu
//! bar item and any keychain prompt need.
//!
//! * macOS - `~/Library/LaunchAgents/app.MyDrives.agent.plist` + `launchctl`
//! * Windows - `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//! * Linux - `~/.config/autostart/mydrives.desktop` (development convenience)

use std::path::PathBuf;

use crate::error::{Error, Result};

pub const LABEL: &str = "app.MyDrives.agent";
#[cfg(windows)]
const RUN_VALUE: &str = "MyDrives";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Enabled { detail: String },
    Disabled,
}

/// Path of the agent binary to launch at login.
///
/// Resolved next to whatever is running now, so the CLI can register the agent
/// without being told where it is.
pub fn agent_path() -> Result<PathBuf> {
    let exe_name = if cfg!(windows) {
        "mydrivesd.exe"
    } else {
        "mydrivesd"
    };
    let here = std::env::current_exe()
        .map_err(|e| Error::Backend(format!("cannot locate the running binary: {e}")))?;
    if here.file_stem().map(|s| s == "mydrivesd").unwrap_or(false) {
        return Ok(here);
    }
    let beside = here
        .parent()
        .map(|d| d.join(exe_name))
        .ok_or_else(|| Error::Backend("cannot locate the agent binary".into()))?;
    if beside.exists() {
        Ok(beside)
    } else {
        Err(Error::NotFound(format!(
            "{} (build it with `cargo build -p mydrives-daemon`)",
            beside.display()
        )))
    }
}

// ----------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
fn plist_path() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|_| Error::Backend("HOME is not set".into()))?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

#[cfg(target_os = "macos")]
pub fn enable() -> Result<Status> {
    let agent = agent_path()?;
    let path = plist_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // launchd starts the agent with a near-empty environment, so anything the
    // current process needed has to be written into the job description or the
    // agent that starts at login behaves differently from the one the user set
    // up. `MYDRIVES_SUDO_MOUNT` is the one that matters: without it a drive
    // whose mount point lives in `/Volumes` cannot create its directory.
    let environment = match std::env::var("MYDRIVES_SUDO_MOUNT") {
        Ok(v) if !v.is_empty() => format!(
            "    <key>EnvironmentVariables</key>\n    \
             <dict><key>MYDRIVES_SUDO_MOUNT</key><string>{v}</string></dict>\n"
        ),
        _ => String::new(),
    };

    // `KeepAlive` with `SuccessfulExit = false` restarts the agent if it dies,
    // but not when the user quits it from the tray.
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{program}</string>
        <string>run</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key>
    <dict><key>SuccessfulExit</key><false/></dict>
{environment}    <key>ProcessType</key><string>Interactive</string>
    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        program = agent.display(),
        log = log_path().display(),
    );
    if std::fs::read_to_string(&path).map(|old| old != plist).unwrap_or(true) {
        std::fs::write(&path, &plist)?;
    }

    // Deliberately **no** `launchctl bootstrap` here.
    //
    // Writing the plist into `~/Library/LaunchAgents` *is* the registration:
    // launchd loads that directory at login, and `RunAtLoad` starts the agent
    // then. Bootstrapping it now would ask launchd to start a second agent
    // immediately - and `enable()` is only ever reached from inside a running
    // agent, so that is a guaranteed duplicate fighting over the same control
    // socket and mount points. What does need clearing is a disabled-override
    // left by `disable()`, which would otherwise make the plist a no-op.
    let uid = unsafe { getuid() };
    let _ = std::process::Command::new("launchctl")
        .args(["enable", &format!("gui/{uid}/{LABEL}")])
        .output();

    Ok(Status::Enabled {
        detail: path.display().to_string(),
    })
}

#[cfg(target_os = "macos")]
pub fn disable() -> Result<()> {
    let uid = unsafe { getuid() };
    // `disable` (an override), not `bootout`: booting out the label would
    // SIGTERM the agent asking for it, yanking every mounted drive out from
    // under the user who only wanted to change a start-up setting. The running
    // agent stays; nothing starts at the next login.
    let _ = std::process::Command::new("launchctl")
        .args(["disable", &format!("gui/{uid}/{LABEL}")])
        .output();
    let path = plist_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Asks launchd to run the agent again, for an update that has just replaced
/// the binary underneath it.
///
/// `None` means this process is not a launchd job, so the caller has to start
/// the replacement itself. `Some(false)` means launchd was asked and refused.
/// `Some(true)` is mostly theoretical: `kickstart -k` kills this process to
/// start the new one, so the call usually does not return at all.
///
/// This exists because a process spawned from inside a launchd job belongs to
/// that job and is killed when the job exits - an update that spawned its own
/// replacement left the Mac with a new binary and nothing running.
#[cfg(target_os = "macos")]
pub fn restart_job() -> Option<bool> {
    if !std::env::var_os("XPC_SERVICE_NAME").is_some_and(|name| name != "0") {
        return None;
    }
    let uid = unsafe { getuid() };
    Some(
        std::process::Command::new("/bin/launchctl")
            .args(["kickstart", "-k", &format!("gui/{uid}/{LABEL}")])
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
    )
}

#[cfg(not(target_os = "macos"))]
pub fn restart_job() -> Option<bool> {
    None
}

#[cfg(target_os = "macos")]
pub fn status() -> Result<Status> {
    let path = plist_path()?;
    if path.exists() {
        Ok(Status::Enabled {
            detail: path.display().to_string(),
        })
    } else {
        Ok(Status::Disabled)
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    fn getuid() -> u32;
}

#[cfg(target_os = "macos")]
fn log_path() -> PathBuf {
    crate::config::config_dir().join("agent.log")
}

// --------------------------------------------------------------- Windows

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// Runs `reg.exe` without letting a console window appear.
///
/// The agent gives up its console at start-up (see `mydrivesd`), so spawning a
/// console program would make Windows create a *new* console for it - a black
/// window blinking on the desktop. `CREATE_NO_WINDOW` is what keeps it hidden.
#[cfg(windows)]
fn reg(args: &[&str]) -> Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("reg")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| Error::Backend(format!("running reg: {e}")))
}

#[cfg(windows)]
pub fn enable() -> Result<Status> {
    let agent = agent_path()?;
    let command = format!("\"{}\" run", agent.display());
    let out = reg(&[
        "add",
        RUN_KEY,
        "/v",
        RUN_VALUE,
        "/t",
        "REG_SZ",
        "/d",
        &command,
        "/f",
    ])?;
    if !out.status.success() {
        return Err(Error::Backend(format!(
            "registering the startup entry failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(Status::Enabled { detail: command })
}

#[cfg(windows)]
pub fn disable() -> Result<()> {
    let _ = reg(&["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]);
    Ok(())
}

#[cfg(windows)]
pub fn status() -> Result<Status> {
    let out = reg(&["query", RUN_KEY, "/v", RUN_VALUE])?;
    if out.status.success() {
        let text = String::from_utf8_lossy(&out.stdout);
        let detail = text
            .lines()
            .find(|l| l.contains(RUN_VALUE))
            .unwrap_or("")
            .trim()
            .to_string();
        Ok(Status::Enabled { detail })
    } else {
        Ok(Status::Disabled)
    }
}

// ----------------------------------------------------------------- Linux

#[cfg(all(unix, not(target_os = "macos")))]
fn desktop_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| Error::Backend("HOME is not set".into()))?;
    Ok(PathBuf::from(home).join(".config/autostart/mydrives.desktop"))
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn enable() -> Result<Status> {
    let agent = agent_path()?;
    let path = desktop_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &path,
        format!(
            "[Desktop Entry]\nType=Application\nName=MyDrives\nExec={} run\nX-GNOME-Autostart-enabled=true\n",
            agent.display()
        ),
    )?;
    Ok(Status::Enabled {
        detail: path.display().to_string(),
    })
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn disable() -> Result<()> {
    match std::fs::remove_file(desktop_path()?) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn status() -> Result<Status> {
    let path = desktop_path()?;
    Ok(if path.exists() {
        Status::Enabled {
            detail: path.display().to_string(),
        }
    } else {
        Status::Disabled
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_path_points_at_the_daemon() {
        // In tests `current_exe` is the test binary, so this resolves to a
        // sibling that does not exist - the error must say which path it tried.
        match agent_path() {
            Ok(p) => assert!(p.file_stem().unwrap().to_string_lossy().contains("mydrives")),
            Err(e) => assert!(e.to_string().contains("mydrivesd"), "{e}"),
        }
    }

    #[test]
    fn status_does_not_fail_when_nothing_is_registered() {
        // Must not panic or error just because autostart was never enabled.
        let _ = status().expect("status is readable");
    }
}
