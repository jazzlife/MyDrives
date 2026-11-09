//! Tray icon (Windows) / menu bar item (macOS).
//!
//! This is the only part of the agent that must own the **main thread**: AppKit
//! requires its run loop there, and the Win32 message pump for a tray icon has
//! the same constraint. So the tokio runtime lives on worker threads and the
//! menu dispatches work onto it; nothing in here blocks on the network.
//!
//! On macOS the activation policy is set to `Accessory`, which is the
//! programmatic equivalent of `LSUIElement`: a menu bar item and no Dock icon.
//!
//! The menu is rebuilt from the mount manager's state (every few seconds, and
//! after every action), so what the user sees is the real state - including
//! "offline", which a network drive agent must surface rather than hide.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
// The menu types must come from `tray_icon::menu` (tray-icon's own re-export of
// muda). Depending on `muda` directly gives a *different* crate instance, and
// `Menu` then does not implement tray-icon's `ContextMenu` trait.
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

use mydrives_core::manager::{MountManager, MountState};

const REFRESH: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
enum Action {
    Mount(String),
    Unmount(String),
    Reveal(String),
    /// Open the settings window (the separate `mydrives-ui` process).
    OpenWindow,
    OpenConfig,
    /// Start at login: on or off.
    SetAutostart(bool),
    CheckUpdate,
    ApplyUpdate,
    Quit,
}

enum UserEvent {
    Menu(MenuEvent),
    /// `mydrives shutdown` arrived on the control socket.
    Quit,
}

pub fn run(
    manager: Arc<MountManager>,
    rt: Handle,
    config: Arc<parking_lot::Mutex<mydrives_core::config::AppConfig>>,
    shutdown: Arc<tokio::sync::Notify>,
    updates: Arc<crate::updater::Updater>,
) -> anyhow::Result<()> {
    let event_loop = {
        let mut builder = EventLoop::<UserEvent>::with_user_event();
        #[cfg(target_os = "macos")]
        {
            use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
            // Menu bar only: no Dock icon, no window.
            builder.with_activation_policy(ActivationPolicy::Accessory);
        }
        builder.build()?
    };

    // Menu clicks arrive on an OS callback; forward them into the event loop so
    // all state lives on one thread.
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = proxy.send_event(UserEvent::Menu(event));
    }));

    // With the tray running, this thread *is* the event loop, so a `mydrives
    // shutdown` has no other way to end the process: the IPC handler can
    // unmount, but the agent would stay resident with its menu bar item.
    {
        let proxy = event_loop.create_proxy();
        rt.spawn(async move {
            shutdown.notified().await;
            let _ = proxy.send_event(UserEvent::Quit);
        });
    }

    let mut app = Tray {
        manager,
        rt,
        config,
        updates,
        icon: None,
        actions: HashMap::new(),
        next_refresh: Instant::now(),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct Tray {
    manager: Arc<MountManager>,
    rt: Handle,
    config: Arc<parking_lot::Mutex<mydrives_core::config::AppConfig>>,
    updates: Arc<crate::updater::Updater>,
    icon: Option<TrayIcon>,
    actions: HashMap<MenuId, Action>,
    next_refresh: Instant,
}

impl Tray {
    /// Builds the menu from the current mount states.
    fn build_menu(&mut self) -> anyhow::Result<Menu> {
        let menu = Menu::new();
        self.actions.clear();

        let remotes = self.manager.status();

        // One entry, not two: "add" and "manage" open the same window, and a
        // menu that offers both makes the user choose between synonyms. The
        // wording follows what the user actually has.
        let open = MenuItem::new(
            if remotes.is_empty() {
                "드라이브 추가..."
            } else {
                "드라이브 관리..."
            },
            true,
            None,
        );
        self.actions.insert(open.id().clone(), Action::OpenWindow);
        menu.append(&open)?;
        menu.append(&PredefinedMenuItem::separator())?;
        if remotes.is_empty() {
            menu.append(&MenuItem::new("등록된 드라이브가 없습니다", false, None))?;
        }
        for r in &remotes {
            let (label, mounted, at) = match &r.state {
                MountState::Mounted { mountpoint, .. } => (
                    format!("{}  -  {}", r.label, mountpoint),
                    true,
                    Some(mountpoint.clone()),
                ),
                MountState::Offline { mountpoint, .. } => (
                    format!("{}  -  연결 끊김 (재시도 중)", r.label),
                    true,
                    Some(mountpoint.clone()),
                ),
                MountState::Mounting => (format!("{}  -  마운트 중...", r.label), false, None),
                MountState::Failed { reason } => (
                    format!("{}  -  실패: {}", r.label, short(reason)),
                    false,
                    None,
                ),
                MountState::Unmounted => (format!("{}  -  마운트 안 됨", r.label), false, None),
            };

            let submenu = Submenu::new(label, true);
            if mounted {
                if let Some(path) = at {
                    let reveal = MenuItem::new(
                        if cfg!(target_os = "macos") {
                            "Finder에서 열기"
                        } else {
                            "탐색기에서 열기"
                        },
                        true,
                        None,
                    );
                    self.actions
                        .insert(reveal.id().clone(), Action::Reveal(path));
                    submenu.append(&reveal)?;
                }
                let unmount = MenuItem::new("연결 해제", true, None);
                self.actions
                    .insert(unmount.id().clone(), Action::Unmount(r.id.clone()));
                submenu.append(&unmount)?;
            } else {
                let mount = MenuItem::new("연결", true, None);
                self.actions
                    .insert(mount.id().clone(), Action::Mount(r.id.clone()));
                submenu.append(&mount)?;
            }
            if let Some(stats) = r.stats {
                submenu.append(&PredefinedMenuItem::separator())?;
                submenu.append(&MenuItem::new(
                    format!(
                        "읽기 {} / 쓰기 {}",
                        human(stats.bytes_read),
                        human(stats.bytes_written)
                    ),
                    false,
                    None,
                ))?;
                if stats.errors > 0 {
                    submenu.append(&MenuItem::new(
                        format!("오류 {}건", stats.errors),
                        false,
                        None,
                    ))?;
                }
            }
            submenu.append(&PredefinedMenuItem::separator())?;
            submenu.append(&MenuItem::new(r.source.clone(), false, None))?;
            menu.append(&submenu)?;
        }

        menu.append(&PredefinedMenuItem::separator())?;

        // Updates: the state is always visible, and the one useful action for
        // that state sits under it.
        let status = self.updates.status();
        menu.append(&MenuItem::new(status.summary(), false, None))?;
        match &status.state {
            mydrives_core::update::UpdateState::Staged { version } => {
                let apply = MenuItem::new(format!("{version} 지금 적용 (재시작)"), true, None);
                self.actions
                    .insert(apply.id().clone(), Action::ApplyUpdate);
                menu.append(&apply)?;
            }
            mydrives_core::update::UpdateState::Disabled => {}
            mydrives_core::update::UpdateState::Checking
            | mydrives_core::update::UpdateState::Downloading { .. }
            | mydrives_core::update::UpdateState::Applying { .. } => {}
            _ => {
                let check = MenuItem::new("업데이트 확인", true, None);
                self.actions
                    .insert(check.id().clone(), Action::CheckUpdate);
                menu.append(&check)?;
            }
        }

        menu.append(&PredefinedMenuItem::separator())?;
        let config = MenuItem::new("설정 파일 열기", true, None);
        self.actions.insert(config.id().clone(), Action::OpenConfig);
        menu.append(&config)?;
        // Shown from the saved setting, not by querying the OS: on Windows that
        // query means running `reg.exe`, and this menu is rebuilt every few
        // seconds. The agent reconciles the real registration at start-up.
        let on = self.config.lock().general.autostart;
        let login = CheckMenuItem::new("로그인 시 자동 시작", true, on, None);
        self.actions
            .insert(login.id().clone(), Action::SetAutostart(!on));
        menu.append(&login)?;
        let quit = MenuItem::new("MyDrives 종료", true, None);
        self.actions.insert(quit.id().clone(), Action::Quit);
        menu.append(&quit)?;

        Ok(menu)
    }

    fn refresh(&mut self) {
        match self.build_menu() {
            Ok(menu) => {
                if let Some(icon) = &self.icon {
                    icon.set_menu(Some(Box::new(menu)));
                    let tip = self
                        .manager
                        .status()
                        .iter()
                        .filter(|r| r.state.is_live())
                        .count();
                    let _ = icon.set_tooltip(Some(format!("MyDrives - {tip} mounted")));
                }
            }
            Err(e) => tracing::warn!(error = %e, "rebuilding the tray menu failed"),
        }
        self.next_refresh = Instant::now() + REFRESH;
    }

    fn perform(&mut self, action: Action, event_loop: &ActiveEventLoop) {
        match action {
            Action::Mount(id) => {
                let manager = self.manager.clone();
                self.rt.spawn(async move {
                    if let Err(e) = manager.mount(&id).await {
                        tracing::error!(remote = %id, error = %e, "mount from the tray failed");
                    }
                });
            }
            Action::Unmount(id) => {
                let manager = self.manager.clone();
                self.rt.spawn(async move {
                    if let Err(e) = manager.unmount(&id).await {
                        tracing::error!(remote = %id, error = %e, "unmount from the tray failed");
                    }
                });
            }
            Action::Reveal(path) => open_in_file_manager(&path),
            Action::OpenWindow => open_settings_window(),
            Action::OpenConfig => {
                let path = mydrives_core::config::config_path();
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if !path.exists() {
                    let _ = std::fs::write(&path, "# MyDrives configuration\n");
                }
                open_in_file_manager(&path.display().to_string());
            }
            Action::SetAutostart(on) => {
                let applied = if on {
                    mydrives_core::autostart::enable().map(|_| ())
                } else {
                    mydrives_core::autostart::disable()
                };
                match applied {
                    Ok(()) => {
                        let mut cfg = self.config.lock();
                        cfg.general.autostart = on;
                        if let Err(e) = cfg.save() {
                            tracing::warn!(error = %e, "saving the autostart setting failed");
                        }
                    }
                    Err(e) => tracing::error!(error = %e, enable = on, "changing autostart failed"),
                }
            }
            Action::CheckUpdate => self.updates.request_check(),
            Action::ApplyUpdate => self.updates.request_apply(),
            Action::Quit => {
                let manager = self.manager.clone();
                // Unmount before leaving, or the OS is left with dead mount points.
                self.rt.block_on(async move { manager.unmount_all().await });
                event_loop.exit();
            }
        }
        // State changed (or will shortly); show it.
        self.next_refresh = Instant::now() + Duration::from_millis(300);
    }
}

impl ApplicationHandler<UserEvent> for Tray {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        if self.icon.is_some() {
            return;
        }
        // The tray icon must be created after the event loop is running, which on
        // macOS means after the first `resumed`.
        let menu = match self.build_menu() {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(error = %e, "building the tray menu failed");
                return;
            }
        };
        let builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("MyDrives");
        // The macOS menu bar wants a template image so the icon follows
        // light/dark mode. `with_icon_templated` only exists there.
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(drive_icon());
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(drive_icon());

        match builder.build() {
            Ok(icon) => {
                self.icon = Some(icon);
                tracing::info!("tray icon ready");
            }
            Err(e) => tracing::error!(error = %e, "creating the tray icon failed"),
        }
        self.refresh();
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _id: WindowId,
        _event: WindowEvent,
    ) {
        // The agent has no windows.
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Menu(menu_event) => {
                if let Some(action) = self.actions.get(menu_event.id()).cloned() {
                    self.perform(action, event_loop);
                }
            }
            UserEvent::Quit => event_loop.exit(),
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if Instant::now() >= self.next_refresh {
            self.refresh();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_refresh));
    }
}

fn short(text: &str) -> String {
    let one_line: String = text.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if one_line.chars().count() > 48 {
        format!("{}...", one_line.chars().take(45).collect::<String>())
    } else {
        one_line
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Launches the settings window as its own process.
///
/// Deliberately a separate binary: the tray owns this process's event loop (and
/// on macOS the main thread), while a GUI framework wants to own one too. Two
/// processes talking over the existing control socket is simpler and more robust
/// than making them share a loop - and if the window crashes, the mounts do not.
pub fn open_settings_window() {
    open_settings_window_args(&[])
}

/// `--add` 처럼 창에 전달할 인자와 함께 엽니다.
pub fn open_settings_window_args(args: &[&str]) {
    let exe_name = if cfg!(windows) {
        "mydrives-ui.exe"
    } else {
        "mydrives-ui"
    };
    let beside_us = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(exe_name)));

    let candidates: Vec<std::path::PathBuf> = beside_us
        .into_iter()
        .chain(std::iter::once(std::path::PathBuf::from(exe_name)))
        .collect();

    for candidate in candidates {
        match std::process::Command::new(&candidate).args(args).spawn() {
            Ok(_) => return,
            Err(e) => tracing::debug!(path = %candidate.display(), error = %e, "settings window not started"),
        }
    }
    tracing::error!(
        "could not start {exe_name}; build it with `cargo build -p mydrives-ui`"
    );
}

fn open_in_file_manager(path: &str) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(path).spawn()
    } else if cfg!(windows) {
        std::process::Command::new("explorer").arg(path).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(path).spawn()
    };
    if let Err(e) = result {
        tracing::warn!(path, error = %e, "could not open the file manager");
    }
}

/// A small drive glyph, drawn in code so the agent needs no asset files (and no
/// image decoder dependency).
///
/// Drawn as an **outline** on purpose: macOS renders menu bar icons as template
/// images, where only the alpha channel survives and every opaque pixel becomes
/// one flat colour. A filled rectangle therefore shows up as a featureless white
/// blob; the shape has to be carried by the transparent parts.
fn drive_icon() -> Icon {
    const SIZE: i32 = 32;
    const BORDER: i32 = 3;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];

    // Body: 24 x 16, centred.
    let (left, top) = (4i32, 8i32);
    let (w, h) = (24i32, 16i32);
    let radius = 4i32;

    let in_rounded = |px: i32, py: i32, l: i32, t: i32, w: i32, h: i32, r: i32| -> bool {
        if px < l || py < t || px >= l + w || py >= t + h {
            return false;
        }
        let cx = px.clamp(l + r, l + w - 1 - r);
        let cy = py.clamp(t + r, t + h - 1 - r);
        let (dx, dy) = (px - cx, py - cy);
        dx * dx + dy * dy <= r * r
    };

    for y in 0..SIZE {
        for x in 0..SIZE {
            let outer = in_rounded(x, y, left, top, w, h, radius);
            let inner = in_rounded(
                x,
                y,
                left + BORDER,
                top + BORDER,
                w - 2 * BORDER,
                h - 2 * BORDER,
                (radius - BORDER).max(1),
            );
            // Activity light: a filled dot inside, on the right.
            let (ldx, ldy) = (x - (left + w - 7), y - (top + h / 2));
            let led = ldx * ldx + ldy * ldy <= 4;

            let opaque = (outer && !inner) || led;
            let i = ((y * SIZE + x) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&if opaque {
                [0xEA, 0xEA, 0xEA, 0xFF]
            } else {
                [0x00, 0x00, 0x00, 0x00]
            });
        }
    }
    Icon::from_rgba(rgba, SIZE as u32, SIZE as u32).expect("the generated icon is valid RGBA")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_valid() {
        // from_rgba would have panicked on a wrong buffer length.
        let _ = drive_icon();
    }

    #[test]
    fn long_errors_are_shortened_for_the_menu() {
        let long = "a".repeat(100);
        assert!(short(&long).chars().count() <= 48);
        assert_eq!(short("line\nbreak"), "line break");
    }

    #[test]
    fn byte_counts_are_readable() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(2048), "2.0 KiB");
        assert_eq!(human(5 * 1024 * 1024), "5.0 MiB");
    }
}
