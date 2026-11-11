//! MyDrives 설정 창.
//!
//! 트레이/메뉴바의 "드라이브 관리..."가 이 프로세스를 띄웁니다. 에이전트와는 제어
//! 소켓(IPC)으로만 대화하므로, 이 창이 죽어도 마운트는 그대로 유지됩니다.
//!
//! 화면은 macOS 시스템 설정과 같은 두 칸 구조입니다 — 왼쪽은 드라이브 목록(소스
//! 리스트), 오른쪽은 선택한 드라이브의 상세. 추가도 같은 자리에서 하므로 "추가"와
//! "관리"가 따로 있지 않습니다.

// 릴리즈 빌드는 콘솔 서브시스템 자체를 쓰지 않습니다. 디버그 빌드는 콘솔
// 앱이라 Windows가 창을 하나 더 띄우는데, 그건 `main`에서 내려놓습니다.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod theme;

use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, CornerRadius, FontId, Margin, RichText, Sense, Stroke, Vec2};

use mydrives_core::config::{BackendConfig, FtpConfig, RemoteConfig, SecretRef, SmbConfig};
use mydrives_core::manager::{MountState, RemoteStatus};
use mydrives_core::update::{UpdateState, UpdateStatus};
use mydrives_ipc::{Request, Response, UpdateAction};

use theme::Palette;

const REFRESH: Duration = Duration::from_millis(1500);
const SIDEBAR_WIDTH: f32 = 236.0;
const ROW_HEIGHT: f32 = 48.0;

/// 이미 떠 있는 설정 창이 자기 존재를 알리는 파일. 1초마다 갱신되므로
/// 창이 죽으면 금방 낡은 것으로 판정됩니다.
const LOCK_NAME: &str = "ui.lock";
/// 새로 실행된 설정 창이 "네가 앞으로 나와라"라고 남기는 표시.
const RAISE_NAME: &str = "ui.raise";
const HEARTBEAT: Duration = Duration::from_secs(1);
const STALE_AFTER: Duration = Duration::from_secs(5);

fn lock_path() -> std::path::PathBuf {
    mydrives_core::config::config_dir().join(LOCK_NAME)
}

fn raise_path() -> std::path::PathBuf {
    mydrives_core::config::config_dir().join(RAISE_NAME)
}

/// 설정 창의 주인이 되거나, 이미 있는 창을 앞으로 불러내고 물러납니다.
///
/// 트레이의 "드라이브 관리..."도, 드라이브가 하나도 없을 때 에이전트가 여는
/// 첫 화면도, 매번 이 프로그램을 새로 실행합니다. 그대로 두면 창이 열릴 때마다
/// **쓰고 있던 창이 포커스를 빼앗기고** 입력하던 내용은 뒤에 남은 창에 갇힙니다.
/// 그래서 창은 언제나 하나만 두고, 두 번째 실행은 그 창을 앞으로 올린 뒤 끝냅니다.
fn claim_the_window() -> bool {
    let claimed = claim_the_window_in(&mydrives_core::config::config_dir());
    if !claimed {
        tracing::info!("설정 창이 이미 열려 있어 그 창을 앞으로 올립니다");
    }
    claimed
}

/// 살아 있는 창이 있으면 `ui.raise`를 남기고 `false`, 없으면 자리를 차지하고 `true`.
///
/// 살아 있는지는 `ui.lock`이 최근에 갱신되었는지로 봅니다 - 창은 1초마다 그
/// 파일을 다시 씁니다. 창이 죽으면 갱신이 멈추므로 다음 실행이 자리를 넘겨받고,
/// 지우지 못한 잠금 파일 때문에 창이 영영 안 열리는 일이 없습니다.
fn claim_the_window_in(dir: &std::path::Path) -> bool {
    let lock = dir.join(LOCK_NAME);
    if let Ok(modified) = std::fs::metadata(&lock).and_then(|m| m.modified()) {
        if modified.elapsed().map(|e| e < STALE_AFTER).unwrap_or(false) {
            let _ = std::fs::write(dir.join(RAISE_NAME), std::process::id().to_string());
            return false;
        }
    }
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(&lock, std::process::id().to_string());
    true
}

fn main() -> eframe::Result {
    // 트레이에서 띄우면(또는 디버그 빌드면) Windows가 이 프로세스에 콘솔 창을
    // 하나 붙여 줍니다. 창 옆에 검은 콘솔이 같이 뜨는 게 그것이라, 우리가
    // 그 콘솔의 유일한 주인일 때만 내려놓고 로그는 파일로 돌립니다.
    let detached_log = mydrives_core::console::detach_if_sole_owner("ui.log");
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info".into());
    match detached_log {
        Some(file) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        None => tracing_subscriber::fmt().with_env_filter(filter).init(),
    }

    if !claim_the_window() {
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([880.0, 580.0])
            .with_min_inner_size([720.0, 460.0])
            // Windows otherwise inherits the launching process's "show maximized"
            // hint and the window opens full screen.
            .with_maximized(false)
            .with_resizable(true)
            .with_title("MyDrives"),
        ..Default::default()
    };
    eframe::run_native(
        "MyDrives",
        options,
        Box::new(|cc| {
            install_korean_font(&cc.egui_ctx);
            // `--add` opens straight on the form. The tray uses it when there
            // is nothing to manage yet, so a first-time user lands on the one
            // thing they came to do.
            let add = std::env::args().any(|a| a == "--add");
            Ok(Box::new(App::new(add)))
        }),
    )
}

/// egui의 기본 폰트에는 한글 글리프가 없어 전부 □ 로 보입니다. OS에 이미 있는
/// 한글 폰트를 찾아 넣습니다(별도 폰트 파일을 배포하지 않기 위함).
fn install_korean_font(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        // macOS — 시스템 UI 폰트와 가장 가까운 것부터.
        "/System/Library/Fonts/AppleSDGothicNeo.ttc",
        "/System/Library/Fonts/Supplemental/AppleGothic.ttf",
        // Windows
        "C:\\Windows\\Fonts\\malgun.ttf",
        "C:\\Windows\\Fonts\\gulim.ttc",
        // Linux (개발용)
        "/usr/share/fonts/truetype/nanum/NanumGothic.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    ];
    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "system-korean".to_owned(),
            std::sync::Arc::new(egui::FontData::from_owned(bytes)),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .insert(0, "system-korean".to_owned());
        }
        ctx.set_fonts(fonts);
        tracing::info!(path, "한글 폰트 적용");
        return;
    }
    tracing::warn!("한글 폰트를 찾지 못했습니다; 한글이 깨져 보일 수 있습니다");
}

/// 에이전트에 보낸 요청의 결과를 기다리는 자리.
struct Pending<T> {
    label: String,
    rx: Receiver<Result<T, String>>,
}

/// 오른쪽 칸에 무엇을 그릴지.
enum View {
    /// 선택된 드라이브(없으면 안내).
    Drive,
    /// 새 드라이브 추가 폼.
    Add(Box<Form>),
}

struct App {
    rt: tokio::runtime::Runtime,
    remotes: Vec<RemoteStatus>,
    agent_error: Option<String>,

    status_call: Option<Pending<Vec<RemoteStatus>>>,
    action_call: Option<Pending<String>>,
    update_call: Option<Pending<UpdateStatus>>,
    /// "편집"을 누르면 그 드라이브의 저장된 설정을 받아 폼을 채웁니다.
    edit_call: Option<Pending<RemoteConfig>>,
    next_refresh: Instant,

    update: Option<UpdateStatus>,
    message: Option<(String, bool)>,
    view: View,
    selected: Option<String>,
    /// "저장하고 연결"에서 저장이 끝난 뒤 이어서 연결할 드라이브.
    pending_mount: Option<String>,
    /// 이름 칸에 입력 중인 값과, 그게 어느 드라이브의 것인지.
    name_edit: String,
    name_edit_for: Option<String>,
    sized: bool,
    dark: Option<bool>,
    /// 창이 살아 있다는 표시를 마지막으로 갱신한 시각.
    last_beat: Instant,
}

/// 폼이 만들 드라이브의 종류.
///
/// 한 폼에 둘을 담는 것은, 사용자가 채우는 칸이 거의 같기 때문입니다 - 주소,
/// 계정, 비밀번호, 어디에 붙일지. 다른 것은 "무엇을 고르느냐"(SMB는 공유 폴더,
/// FTP는 계정 안의 경로)와 FTP의 TLS 선택뿐입니다.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DriveKind {
    Smb,
    Ftp,
}

impl DriveKind {
    fn label(self) -> &'static str {
        match self {
            DriveKind::Smb => "NAS (SMB)",
            DriveKind::Ftp => "FTP",
        }
    }

    fn blurb(self) -> &'static str {
        match self {
            DriveKind::Smb => "QNAP·Synology 등 NAS의 공유 폴더를 드라이브로 연결합니다",
            DriveKind::Ftp => "웹호스팅·공유기·구형 NAS의 FTP 계정을 드라이브로 연결합니다",
        }
    }
}

/// 추가 폼. SMB(NAS)와 FTP를 받습니다.
struct Form {
    kind: DriveKind,
    /// 드라이브 이름. 기본값은 **공유 폴더 이름**입니다 — 원격 폴더와 로컬 폴더
    /// 이름이 어긋나면 공유가 여러 개일 때 어느 것이 어느 것인지 알 수 없습니다.
    id: String,
    /// 사용자가 직접 고쳤는지(그렇다면 공유 이름을 따라가지 않습니다).
    id_edited: bool,
    mountpoint_edited: bool,
    name_edited: bool,
    home: String,
    /// 화면에 보이는 이름. 기본값은 **제품 이름**(NAS 이름·서비스 이름)입니다.
    name: String,
    host: String,
    port: String,
    share: String,
    user: String,
    password: String,
    subpath: String,
    drive: String,
    mountpoint: String,
    readonly: bool,
    automount: bool,
    advanced: bool,
    /// FTP: `plain`, `explicit` 또는 `implicit`.
    security: String,
    /// FTP: `passive`, `epsv` 또는 `active`.
    mode: String,
    /// FTP 패시브: PASV가 알려준 주소 대신 접속한 주소를 씁니다(NAT 뒤 서버).
    nat_workaround: bool,
    /// FTP 액티브: 서버가 되돌아 연결할 때까지 기다리는 시간(초).
    active_timeout: String,
    /// FTP: 검증되지 않는 인증서를 받아들입니다(NAS 자체 서명).
    insecure_tls: bool,
    /// 포트를 직접 고쳤는지. 그렇지 않으면 종류와 보안 설정을 따라갑니다.
    port_edited: bool,
    /// 이미 있는 드라이브를 고치는 중. 드라이브 이름(id)은 설정 키·키체인
    /// 계정이라 잠그고, 비밀번호를 비워 두면 저장된 것을 그대로 씁니다.
    editing: bool,
}

impl Form {
    /// 저장된 드라이브를 고치려고 폼에 그대로 펼쳐 놓습니다.
    ///
    /// 잘못 넣은 주소·공유·계정 하나 때문에 드라이브를 지우고 처음부터 다시
    /// 넣는 일이 없도록. 모든 칸은 "사용자가 손댄 것"으로 표시해 두어, 호스트를
    /// 고쳐도 이름과 폴더가 멋대로 따라 바뀌지 않습니다.
    fn from_remote(cfg: &RemoteConfig) -> Form {
        let mut form = Form {
            id: cfg.id.clone(),
            name: cfg.label.clone(),
            automount: cfg.automount,
            readonly: cfg.readonly,
            drive: cfg.drive.clone().unwrap_or_default(),
            mountpoint: cfg.mountpoint.clone().unwrap_or_default(),
            id_edited: true,
            name_edited: true,
            mountpoint_edited: true,
            port_edited: true,
            editing: true,
            ..Form::default()
        };
        match &cfg.backend {
            BackendConfig::Smb(smb) => {
                form.kind = DriveKind::Smb;
                form.host = smb.host.clone();
                form.port = smb.port.to_string();
                form.share = smb.share.clone();
                form.user = smb.user.clone();
                form.subpath = smb.subpath.clone().unwrap_or_default();
            }
            BackendConfig::Ftp(ftp) => {
                form.kind = DriveKind::Ftp;
                form.host = ftp.host.clone();
                form.port = ftp.port.to_string();
                form.user = ftp.user.clone();
                form.subpath = ftp.subpath.clone().unwrap_or_default();
                form.security = ftp.security.clone();
                form.mode = ftp.mode.clone();
                form.nat_workaround = ftp.passive_nat_workaround;
                form.active_timeout = ftp.active_timeout_secs.to_string();
                form.insecure_tls = ftp.accept_invalid_certs;
            }
            BackendConfig::Mem(_) => {}
        }
        // 고급 칸에 값이 있으면 펼쳐서 보여 줍니다 - 숨긴 채로는 고칠 수 없습니다.
        form.advanced = !form.subpath.is_empty() || form.readonly || !form.automount;
        form
    }
}

impl Default for Form {
    fn default() -> Self {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default();
        Form {
            kind: DriveKind::Smb,
            id: String::new(),
            id_edited: false,
            mountpoint_edited: false,
            name_edited: false,
            home,
            name: String::new(),
            host: String::new(),
            port: "445".into(),
            share: String::new(),
            user: String::new(),
            password: String::new(),
            subpath: String::new(),
            drive: if cfg!(windows) { "Z:".into() } else { String::new() },
            mountpoint: String::new(),
            readonly: false,
            automount: true,
            advanced: false,
            security: "explicit".into(),
            mode: "passive".into(),
            nat_workaround: false,
            active_timeout: "10".into(),
            insecure_tls: false,
            port_edited: false,
            editing: false,
        }
    }
}

impl Form {
    /// 입력한 값에서 나머지를 채웁니다(사용자가 직접 고친 칸은 건드리지 않음).
    ///
    /// 보이는 **이름**은 제품(NAS·서비스) 이름을 따르고, **id와 마운트 폴더**는
    /// 공유 폴더 이름을 따릅니다 — 이름은 "어느 장비인가"를, 경로는 "어느
    /// 폴더인가"를 말해야 하기 때문입니다.
    fn follow_sources(&mut self, taken: &[String], taken_ids: &[String]) {
        // 주소 칸에 포트까지 붙여 적는 것이 보통입니다 - 서버가 알려주는 모양이
        // "nas.example.com:5445"이기 때문입니다. 받아서 갈라 놓습니다.
        if let Some((host, port)) = split_host_port(&self.host) {
            self.host = host;
            self.port = port.to_string();
            self.port_edited = true;
        }
        // "어느 폴더인가": SMB는 공유 폴더 칸(공유 안의 폴더까지 적었으면 그
        // 마지막 폴더), FTP는 계정 안의 경로입니다.
        let share = match self.kind {
            DriveKind::Smb => {
                let (share, inside) = split_share_path(&self.share);
                inside
                    .and_then(|p| p.rsplit('/').next().map(str::to_string))
                    .filter(|leaf| !leaf.is_empty())
                    .unwrap_or(share)
            }
            DriveKind::Ftp => self.subpath.trim().to_string(),
        };
        if !self.port_edited {
            self.port = self.default_port().to_string();
        }
        if !self.id_edited {
            let base = match self.kind {
                DriveKind::Smb => share.clone(),
                DriveKind::Ftp => {
                    mydrives_core::naming::ftp_default_id(self.host.trim(), opt(&self.subpath).as_deref())
                }
            };
            // 이름이 겹치면 **기존 드라이브를 덮어씁니다** — id가 설정 키이자
            // 키체인 계정이자 마운트 폴더이기 때문입니다. 겹치지 않는 이름을
            // 먼저 제안합니다.
            self.id = if base.trim().is_empty() {
                base
            } else {
                mydrives_core::naming::unique_id(&base, taken_ids)
            };
        }
        // 마운트 폴더는 드라이브 이름(id)을 그대로 씁니다. 공유 이름을 받기
        // 전에는 비워 둡니다 - ".../MyDrives/drive" 같은 자리표시 경로가
        // 진짜 값처럼 보입니다.
        let _ = taken;
        if !self.mountpoint_edited && !cfg!(windows) {
            self.mountpoint = match self.id.trim() {
                "" => String::new(),
                name => format!("{}/MyDrives/{name}", self.home),
            };
        }
        if !self.name_edited {
            // 화면에 보이는 이름은 **마운트 폴더 이름**을 기본값으로 합니다 -
            // Finder 사이드바와 "컴퓨터" 보기가 그 폴더 이름을 보여 주므로,
            // 둘이 같으면 어느 것이 어느 드라이브인지 헷갈리지 않습니다.
            // 사용자가 다른 이름으로 고치면 그때부터 따라가지 않습니다.
            self.name = self.id.trim().to_string();
        }
    }

    /// 종류와 보안 설정에 맞는 포트. 사용자가 포트를 고치면 쓰이지 않습니다.
    fn default_port(&self) -> u16 {
        match self.kind {
            DriveKind::Smb => 445,
            // 명시적 FTPS는 평문과 같은 21번에서 AUTH TLS로 올라갑니다.
            DriveKind::Ftp if self.security == "implicit" => 990,
            DriveKind::Ftp => 21,
        }
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self.kind == DriveKind::Smb && self.share.trim().is_empty() {
            return Err("공유 폴더 이름을 입력하세요");
        }
        if self.id.trim().is_empty() {
            return Err("드라이브 이름을 입력하세요");
        }
        if self.host.trim().is_empty() {
            return Err("호스트(IP 또는 주소)를 입력하세요");
        }
        if self.port.trim().parse::<u16>().is_err() {
            return Err("포트는 1~65535 사이의 숫자여야 합니다");
        }
        if cfg!(windows) {
            if self.drive.trim().is_empty() {
                return Err("드라이브 문자를 입력하세요 (예: Z:)");
            }
        } else if self.mountpoint.trim().is_empty() {
            return Err("마운트할 폴더 경로를 입력하세요");
        }
        Ok(())
    }

    /// 폼의 값을 프로토콜이 받아들이는 모양으로 다듬어 돌려줍니다.
    ///
    /// 사람은 주소 칸에 포트를 붙여 적고, 공유 폴더를 `/jazzlife` 처럼 경로로
    /// 적습니다. 그대로 넘기면 SMB가 `bad SMB target: Share name...` 으로
    /// 거절하는데, 그건 사용자가 틀린 게 아니라 받아 적는 쪽이 할 일을 안 한
    /// 것입니다.
    fn to_remote(&self) -> RemoteConfig {
        let (host, split_port) = match split_host_port(&self.host) {
            Some((host, port)) => (host, Some(port)),
            None => (self.host.trim().to_string(), None),
        };
        let typed_port = self.port.trim().parse::<u16>().ok();
        let port = split_port.or(typed_port);
        RemoteConfig {
            id: self.id.trim().to_string(),
            // 비워 두면 에이전트가 제품 이름으로 채웁니다.
            label: self.name.trim().to_string(),
            automount: self.automount,
            readonly: self.readonly,
            provider: None,
            drive: opt(&self.drive),
            mountpoint: opt(&self.mountpoint),
            backend: match self.kind {
                DriveKind::Smb => {
                    let (share, inside) = split_share_path(&self.share);
                    BackendConfig::Smb(SmbConfig {
                        host: host.clone(),
                        port: port.unwrap_or(445),
                        share,
                        user: self.user.trim().to_string(),
                        // 공유 폴더 칸에 폴더까지 적었으면 그것이, 아니면 고급
                        // 설정의 하위 경로가 마운트의 맨 위가 됩니다.
                        subpath: inside.or_else(|| clean_subpath(&self.subpath)),
                        // 저장은 에이전트가 OS 키체인에 합니다.
                        secret: SecretRef::Keychain,
                    })
                }
                DriveKind::Ftp => BackendConfig::Ftp(FtpConfig {
                    host: host.clone(),
                    port: port.unwrap_or(21),
                    user: self.user.trim().to_string(),
                    subpath: clean_subpath(&self.subpath),
                    security: self.security.clone(),
                    mode: self.mode.clone(),
                    passive_nat_workaround: self.nat_workaround,
                    active_timeout_secs: self.active_timeout.trim().parse().unwrap_or(10),
                    accept_invalid_certs: self.insecure_tls,
                    max_connections: 4,
                    secret: SecretRef::Keychain,
                }),
            },
        }
    }
}

/// 설정 파일에 쓰이는 백엔드 이름을 화면에 쓸 이름으로.
fn kind_label(kind: &str) -> &str {
    match kind {
        "smb" => "NAS (SMB)",
        "ftp" => "FTP",
        "mem" => "테스트",
        other => other,
    }
}

/// `nas.example.com:5445` 처럼 포트가 붙어 들어온 주소를 갈라 놓습니다.
///
/// NAS 관리 화면도, 공유 주소도 포트를 붙여 보여 주므로 사람은 그대로 붙여
/// 넣습니다. IPv6 주소(`[::1]:445`)와 포트 없는 주소는 건드리지 않습니다.
fn split_host_port(raw: &str) -> Option<(String, u16)> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('[') {
        return None;
    }
    let (host, port) = raw.rsplit_once(':')?;
    // IPv6 주소에는 콜론이 여러 개입니다 - 포트가 아니라 주소의 일부입니다.
    if host.contains(':') || host.trim().is_empty() {
        return None;
    }
    let port: u16 = port.trim().parse().ok()?;
    if port == 0 {
        return None;
    }
    Some((host.trim().to_string(), port))
}

/// 공유 폴더 칸에 적힌 것을 **공유 이름**과 **그 안의 경로**로 가릅니다.
///
/// 사람은 이 칸에 `Public`만 적기도, `/Public`이라 적기도, `Public/2026`이나
/// `\\nas\Public\2026`처럼 들어가고 싶은 폴더까지 적기도 합니다. SMB가 받는
/// 공유 이름은 첫 조각뿐이고, 나머지는 그 공유 안에서 보여줄 폴더입니다. 첫
/// 조각 이외를 버리면 폴더를 적어도 공유의 맨 위가 뜨고, 마지막 조각만 남기면
/// 있지도 않은 공유를 찾습니다 - 둘 다 실제로 겪은 일입니다.
fn split_share_path(raw: &str) -> (String, Option<String>) {
    let trimmed = raw.trim();
    // `\\nas\Public\...`: UNC 경로의 첫 조각은 호스트입니다.
    let unc = trimmed.starts_with("\\\\") || trimmed.starts_with("//");
    let normalized = trimmed.replace('\\', "/");
    let mut parts: Vec<&str> = normalized
        .split('/')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if unc && !parts.is_empty() {
        parts.remove(0);
    }
    let share = parts.first().map(|s| s.to_string()).unwrap_or_default();
    let inside = (parts.len() > 1).then(|| format!("/{}", parts[1..].join("/")));
    (share, inside)
}

/// 하위 경로는 항상 `/`로 시작하는 한 줄로 만듭니다.
fn clean_subpath(raw: &str) -> Option<String> {
    let trimmed = raw.trim().replace('\\', "/");
    let trimmed = trimmed.trim_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    Some(format!("/{trimmed}"))
}

fn opt(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

impl App {
    fn new(start_on_form: bool) -> Self {
        App {
            rt: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("tokio 런타임"),
            remotes: Vec::new(),
            agent_error: None,
            status_call: None,
            action_call: None,
            update_call: None,
            edit_call: None,
            next_refresh: Instant::now(),
            update: None,
            message: None,
            view: if start_on_form {
                View::Add(Box::new(Form::default()))
            } else {
                View::Drive
            },
            selected: None,
            pending_mount: None,
            name_edit: String::new(),
            name_edit_for: None,
            sized: false,
            dark: None,
            last_beat: Instant::now(),
        }
    }

    /// 요청을 백그라운드로 보내고 결과 수신자를 돌려줍니다.
    fn send<T, F>(&self, label: &str, request: Request, extract: F) -> Pending<T>
    where
        T: Send + 'static,
        F: FnOnce(Response) -> Result<T, String> + Send + 'static,
    {
        let (tx, rx) = channel();
        self.rt.spawn(async move {
            let result = match mydrives_ipc::call(&request).await {
                Ok(Response::Error { message }) => Err(message),
                Ok(response) => extract(response),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(result);
        });
        Pending {
            label: label.to_string(),
            rx,
        }
    }

    fn refresh_status(&mut self) {
        if self.status_call.is_none() {
            self.status_call = Some(self.send("상태 조회", Request::Status, |r| match r {
                Response::Status { remotes } => Ok(remotes),
                other => Err(format!("예상 밖의 응답: {other:?}")),
            }));
        }
        if self.update_call.is_none() {
            self.update_call = Some(self.send(
                "업데이트 상태",
                Request::Update {
                    action: UpdateAction::Status,
                },
                |r| match r {
                    Response::Update { status } => Ok(status),
                    other => Err(format!("예상 밖의 응답: {other:?}")),
                },
            ));
        }
        self.next_refresh = Instant::now() + REFRESH;
    }

    fn start_action(&mut self, label: &str, request: Request) {
        if self.action_call.is_some() {
            return;
        }
        self.message = None;
        self.action_call = Some(self.send(label, request, |r| match r {
            Response::Ok => Ok("완료".to_string()),
            Response::Mounted { id, mountpoint } => Ok(format!("{id} → {mountpoint} 연결됨")),
            Response::Tested { summary } => Ok(summary),
            Response::Update { status } => Ok(status.summary()),
            other => Err(format!("예상 밖의 응답: {other:?}")),
        }));
    }

    fn poll(&mut self) {
        if let Some(call) = &self.status_call {
            match call.rx.try_recv() {
                Ok(Ok(remotes)) => {
                    self.remotes = remotes;
                    self.agent_error = None;
                    self.status_call = None;
                    self.fix_selection();
                }
                Ok(Err(e)) => {
                    self.agent_error = Some(e);
                    self.status_call = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(_) => self.status_call = None,
            }
        }
        if let Some(call) = &self.update_call {
            match call.rx.try_recv() {
                Ok(Ok(status)) => {
                    self.update = Some(status);
                    self.update_call = None;
                }
                Ok(Err(_)) => self.update_call = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(_) => self.update_call = None,
            }
        }
        if let Some(call) = &self.edit_call {
            match call.rx.try_recv() {
                Ok(Ok(remote)) => {
                    self.selected = Some(remote.id.clone());
                    self.view = View::Add(Box::new(Form::from_remote(&remote)));
                    self.edit_call = None;
                }
                Ok(Err(e)) => {
                    self.message = Some((format!("편집: {e}"), true));
                    self.edit_call = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(_) => self.edit_call = None,
            }
        }
        if let Some(call) = &self.action_call {
            match call.rx.try_recv() {
                Ok(result) => {
                    let succeeded = result.is_ok();
                    self.message = Some(match result {
                        Ok(text) => (text, false),
                        Err(e) => (format!("{}: {e}", call.label), true),
                    });
                    self.action_call = None;
                    self.next_refresh = Instant::now();
                    // 저장이 성공했으면 이어서 연결까지 해 줍니다.
                    if let Some(id) = self.pending_mount.take() {
                        if succeeded {
                            self.start_action("연결", Request::Mount { id });
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(_) => self.action_call = None,
            }
        }
        if self.status_call.is_none() && Instant::now() >= self.next_refresh {
            self.refresh_status();
        }
    }

    /// 삭제되었거나 아직 아무것도 고르지 않았을 때 선택을 바로잡습니다.
    fn fix_selection(&mut self) {
        let gone = match &self.selected {
            Some(id) => !self.remotes.iter().any(|r| &r.id == id),
            None => true,
        };
        if gone {
            self.selected = self.remotes.first().map(|r| r.id.clone());
        }
    }

    fn selected_remote(&self) -> Option<RemoteStatus> {
        let id = self.selected.as_ref()?;
        self.remotes.iter().find(|r| &r.id == id).cloned()
    }
}

impl eframe::App for App {
    /// 상태 갱신은 UI 밖에서. eframe이 매 프레임 `ui()` 전에 불러 줍니다.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 창이 살아 있다는 표시를 갱신하고, 두 번째 실행이 남긴 "앞으로 나와라"
        // 요청을 받아 이 창을 올립니다. 새 창을 띄우는 대신 이 창을 쓰므로,
        // 입력하던 내용과 키보드 포커스가 그대로 유지됩니다.
        if self.last_beat.elapsed() >= HEARTBEAT {
            self.last_beat = Instant::now();
            let _ = std::fs::write(lock_path(), std::process::id().to_string());
        }
        let raise = raise_path();
        if raise.exists() {
            let _ = std::fs::remove_file(&raise);
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        self.poll();
        // 작업 결과와 상태 변화를 바로 반영하기 위한 가벼운 주기 갱신.
        ctx.request_repaint_after(Duration::from_millis(300));

        if !self.sized {
            // Windows에서는 부모 프로세스의 "최대화로 표시" 힌트를 물려받아 창이
            // 전체 화면으로 열립니다. 첫 프레임에서 강제로 되돌립니다.
            self.sized = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(880.0, 580.0)));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // OS 테마를 따라갑니다. 바뀌었을 때만 다시 칠합니다.
        let dark = ui.ctx().theme() == egui::Theme::Dark;
        if self.dark != Some(dark) {
            theme::apply(ui.ctx(), dark);
            self.dark = Some(dark);
        }
        let p = Palette::of(dark);

        if let Some(error) = self.agent_error.clone() {
            self.draw_disconnected(ui, &p, &error);
            return;
        }

        egui::Panel::left("sidebar")
            .exact_size(SIDEBAR_WIDTH)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(p.sidebar)
                    .inner_margin(Margin::symmetric(10, 12)),
            )
            .show(ui, |ui| self.draw_sidebar(ui, &p));

        egui::Panel::bottom("statusbar")
            .exact_size(40.0)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(p.window)
                    .inner_margin(Margin::symmetric(20, 8)),
            )
            .show(ui, |ui| self.draw_statusbar(ui, &p));

        egui::CentralPanel::no_frame()
            .frame(
                egui::Frame::NONE
                    .fill(p.window)
                    .inner_margin(Margin::symmetric(24, 20)),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match std::mem::replace(&mut self.view, View::Drive) {
                        View::Add(form) => {
                            if let Some(form) = self.draw_form(ui, &p, *form) {
                                self.view = View::Add(Box::new(form));
                            }
                        }
                        View::Drive => self.draw_detail(ui, &p),
                    });
            });
    }
}

// ----------------------------------------------------------------- 사이드바

impl App {
    fn draw_sidebar(&mut self, ui: &mut egui::Ui, p: &Palette) {
        // 앱 이름은 창 제목줄에 이미 있습니다. 머리말로 한 번 더 적으면 같은
        // 글자가 위아래로 겹쳐 보이기만 하고, 목록 자리를 빼앗습니다.
        ui.add_space(2.0);

        let list_height = (ui.available_height() - 44.0).max(80.0);
        egui::ScrollArea::vertical()
            .max_height(list_height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let adding = matches!(self.view, View::Add(_));
                let remotes = self.remotes.clone();
                if remotes.is_empty() && !adding {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new("등록된 드라이브가 없습니다")
                            .small()
                            .color(p.dim),
                    );
                }
                for r in &remotes {
                    let selected = !adding && self.selected.as_deref() == Some(r.id.as_str());
                    if self.drive_row(ui, p, r, selected).clicked() {
                        self.selected = Some(r.id.clone());
                        self.view = View::Drive;
                        self.message = None;
                    }
                }
                if adding {
                    self.new_drive_row(ui, p);
                }
            });

        // 아래쪽에 고정: 추가는 목록의 일부라기보다 목록에 대한 동작입니다.
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(2.0);
            let button = egui::Button::new(RichText::new("＋  드라이브 추가").color(p.text))
                .fill(Color32::TRANSPARENT)
                .stroke(Stroke::NONE)
                .corner_radius(7)
                .min_size(Vec2::new(ui.available_width(), 30.0));
            if ui.add(button).clicked() {
                self.view = View::Add(Box::new(Form::default()));
                self.message = None;
            }
        });
    }

    /// 소스 리스트의 한 줄. 직접 그립니다 — 점·이름·부제가 한 줄 안에서 정확히
    /// 같은 자리에 오게 하려면 위젯 조합보다 이쪽이 단순합니다.
    fn drive_row(
        &self,
        ui: &mut egui::Ui,
        p: &Palette,
        r: &RemoteStatus,
        selected: bool,
    ) -> egui::Response {
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::click());
        let hovered = response.hovered();
        let bg = if selected {
            p.accent
        } else if hovered {
            p.hover
        } else {
            Color32::TRANSPARENT
        };
        if bg != Color32::TRANSPARENT {
            ui.painter()
                .rect_filled(rect.shrink2(Vec2::new(0.0, 2.0)), CornerRadius::same(7), bg);
        }

        let (text, dim) = if selected {
            (p.on_accent, p.on_accent.gamma_multiply(0.82))
        } else {
            (p.text, p.dim)
        };
        let painter = ui.painter();
        painter.circle_filled(
            egui::pos2(rect.left() + 14.0, rect.center().y),
            4.0,
            if selected {
                p.on_accent
            } else {
                state_color(&r.state, p)
            },
        );
        painter.text(
            egui::pos2(rect.left() + 28.0, rect.center().y - 10.0),
            Align2::LEFT_CENTER,
            // 제품 이름(= 보이는 이름). 어느 장비의 드라이브인지가 먼저입니다.
            ellipsize(r.label.as_str(), 22),
            FontId::proportional(13.0),
            text,
        );
        // 목록에서 "이게 NAS 드라이브인지 FTP인지"가 보여야 합니다. 같은 서버를
        // 두 가지 방법으로 붙여 쓰는 일이 흔하고, 그때 둘을 가르는 것은 종류뿐입니다.
        painter.text(
            egui::pos2(rect.left() + 28.0, rect.center().y + 8.0),
            Align2::LEFT_CENTER,
            format!(
                "{} · {}",
                kind_label(&r.kind),
                ellipsize(&short_state(&r.state), 20)
            ),
            FontId::proportional(11.0),
            dim,
        );
        response
    }

    /// 추가하는 동안 목록 맨 아래에 뜨는 가짜 줄. 지금 무엇을 하고 있는지가
    /// 목록 안에서도 보입니다.
    fn new_drive_row(&self, ui: &mut egui::Ui, p: &Palette) {
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::hover());
        ui.painter()
            .rect_filled(rect.shrink2(Vec2::new(0.0, 2.0)), CornerRadius::same(7), p.accent);
        ui.painter().circle_filled(
            egui::pos2(rect.left() + 14.0, rect.center().y),
            4.0,
            p.on_accent,
        );
        ui.painter().text(
            egui::pos2(rect.left() + 28.0, rect.center().y),
            Align2::LEFT_CENTER,
            "새 드라이브",
            FontId::proportional(13.0),
            p.on_accent,
        );
    }
}

// --------------------------------------------------------------- 상세 화면

impl App {
    fn draw_detail(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let Some(r) = self.selected_remote() else {
            self.draw_empty(ui, p);
            return;
        };

        // 제목은 **연결 대상**입니다. 이름은 왼쪽 목록에 이미 있으므로 여기서
        // 되풀이하지 않고, 아래 카드에서 고칠 수 있는 칸으로 둡니다.
        ui.horizontal(|ui| {
            ui.label(RichText::new(&r.source).size(18.0).strong().color(p.text));
            ui.add_space(8.0);
            let (text, color) = state_badge(&r.state, p);
            pill(ui, &text, color);
        });
        ui.add_space(16.0);

        card(ui, p, |ui| {
            let mountpoint = match &r.state {
                MountState::Mounted { mountpoint, .. } | MountState::Offline { mountpoint, .. } => {
                    Some(mountpoint.clone())
                }
                _ => None,
            };
            // 이름은 언제든 고칠 수 있어야 합니다 — NAS가 자기 이름을 알려
            // 주지 않으면 주소에서 따온 이름이 들어가기 때문입니다.
            widget_row(ui, p, "이름", |ui| {
                if self.name_edit_for.as_deref() != Some(r.id.as_str()) {
                    self.name_edit_for = Some(r.id.clone());
                    self.name_edit = r.label.clone();
                }
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.name_edit)
                        .desired_width(240.0)
                        .hint_text("드라이브 이름"),
                );
                // 다른 곳을 누르거나 Enter를 치면 저장합니다.
                let typed = self.name_edit.trim().to_string();
                if response.lost_focus() && !typed.is_empty() && typed != r.label {
                    self.start_action(
                        "이름 변경",
                        Request::Rename {
                            id: r.id.clone(),
                            label: typed,
                        },
                    );
                }
            });
            info_row(ui, p, "종류", kind_label(&r.kind));
            // 사이드바 행과 "컴퓨터" 보기는 마운트 폴더의 이름을 씁니다. 그 이름이
            // 위의 이름과 다르면 사용자는 어느 것이 드라이브인지 알 수 없으므로
            // 여기서 보여 줍니다.
            if cfg!(target_os = "macos") {
                let shown = mountpoint
                    .as_deref()
                    .and_then(|m| m.trim_end_matches('/').rsplit('/').next())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&r.id);
                info_row(ui, p, "Finder 표시", &format!("사이드바 '위치'에 '{shown}'"));
            }
            info_row(ui, p, "상태", &state_detail(&r.state));
            info_row(
                ui,
                p,
                "위치",
                mountpoint.as_deref().unwrap_or("연결되면 표시됩니다"),
            );
            if let Some(s) = r.stats {
                info_row(
                    ui,
                    p,
                    "전송량",
                    &format!("읽기 {} · 쓰기 {}", human(s.bytes_read), human(s.bytes_written)),
                );
                info_row(
                    ui,
                    p,
                    "열린 파일",
                    &format!("{}개 · 오류 {}건", s.open_handles, s.errors),
                );
            }
            info_row(
                ui,
                p,
                "모드",
                if r.readonly { "읽기 전용" } else { "읽기/쓰기" },
            );
        });

        ui.add_space(18.0);
        let busy = self.action_call.is_some();
        ui.horizontal(|ui| {
            if r.state.is_live() {
                if primary(ui, p, "연결 해제", !busy).clicked() {
                    self.start_action("연결 해제", Request::Unmount { id: r.id.clone() });
                }
                if let MountState::Mounted { mountpoint, .. } = &r.state {
                    if secondary(ui, p, "열기", !busy).clicked() {
                        open_path(mountpoint);
                    }
                }
            } else if primary(ui, p, "연결", !busy).clicked() {
                self.start_action("연결", Request::Mount { id: r.id.clone() });
            }

            // 잘못 넣은 주소·공유·계정·비밀번호를 지우고 다시 넣지 않고 고칠 수
            // 있게. 설정을 바꾸려면 마운트가 풀려 있어야 하므로 연결 중에는
            // 끕니다.
            let can_edit = !r.state.is_live() && !busy && self.edit_call.is_none();
            if secondary(ui, p, "편집", can_edit).clicked() {
                self.edit_call = Some(self.send(
                    "편집",
                    Request::Describe { id: r.id.clone() },
                    |resp| match resp {
                        Response::Described { remote } => Ok(remote),
                        other => Err(format!("예상 밖의 응답: {other:?}")),
                    },
                ));
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let can_remove = !r.state.is_live() && !busy;
                let remove = egui::Button::new(RichText::new("드라이브 삭제").color(p.err))
                    .fill(Color32::TRANSPARENT)
                    .stroke(Stroke::NONE)
                    .corner_radius(7);
                if ui.add_enabled(can_remove, remove).clicked() {
                    self.start_action("삭제", Request::Remove { id: r.id.clone() });
                }
            });
        });
        if r.state.is_live() {
            ui.add_space(6.0);
            ui.label(
                RichText::new("삭제하려면 먼저 연결을 해제하세요")
                    .small()
                    .color(p.dim),
            );
        }
    }

    fn draw_empty(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.add_space(70.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("드라이브가 없습니다").size(17.0).color(p.text));
            ui.add_space(4.0);
            ui.label(
                RichText::new("NAS 공유 폴더를 등록하면 로컬 드라이브처럼 쓸 수 있습니다")
                    .color(p.dim),
            );
            ui.add_space(16.0);
            if primary(ui, p, "＋  드라이브 추가", true).clicked() {
                self.view = View::Add(Box::new(Form::default()));
            }
        });
    }

    fn draw_disconnected(&mut self, ui: &mut egui::Ui, p: &Palette, error: &str) {
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::NONE.fill(p.window).inner_margin(Margin::same(28)))
            .show(ui, |ui| {
                ui.add_space(60.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new("에이전트에 연결할 수 없습니다")
                            .size(17.0)
                            .color(p.text),
                    );
                    ui.add_space(6.0);
                    ui.label(RichText::new(error).small().color(p.dim));
                    ui.add_space(16.0);
                    if primary(ui, p, "에이전트 시작", true).clicked() {
                        start_agent();
                        self.next_refresh = Instant::now() + Duration::from_secs(2);
                    }
                });
            });
    }
}

// ------------------------------------------------------------------- 추가 폼

impl App {
    /// 폼을 그리고, 계속 열어 둘 경우 그대로 돌려줍니다(닫으면 `None`).
    fn draw_form(&mut self, ui: &mut egui::Ui, p: &Palette, mut form: Form) -> Option<Form> {
        let title = if form.editing { "드라이브 편집" } else { "새 드라이브" };
        ui.label(RichText::new(title).size(20.0).strong().color(p.text));
        ui.add_space(2.0);
        if form.editing {
            ui.label(
                RichText::new(format!(
                    "'{}' 드라이브의 설정을 고칩니다. 비밀번호는 바꿀 때만 적으세요",
                    form.id
                ))
                .color(p.dim),
            );
        } else {
            ui.label(RichText::new(form.kind.blurb()).color(p.dim));
        }
        ui.add_space(12.0);

        let mut keep = true;
        let mut source_changed = false;

        // 종류를 먼저 고릅니다: 아래 칸들이 여기에 따라 달라집니다. 있는
        // 드라이브의 종류를 바꾸는 것은 다른 드라이브를 만드는 일이라 편집에서는
        // 잠급니다.
        if !form.editing {
            ui.horizontal(|ui| {
                for kind in [DriveKind::Smb, DriveKind::Ftp] {
                    if ui
                        .selectable_label(form.kind == kind, RichText::new(kind.label()))
                        .clicked()
                        && form.kind != kind
                    {
                        form.kind = kind;
                        source_changed = true;
                    }
                }
            });
            ui.add_space(10.0);
        }
        // 이름이 이미 쓰이고 있는지 보려면 지금 있는 드라이브들의 이름이 필요합니다.
        // 보이는 이름(label)과 드라이브 이름(id)은 쓰임이 달라 따로 봅니다 —
        // id가 겹치면 기존 드라이브를 덮어쓰기 때문입니다.
        let taken: Vec<String> = self.remotes.iter().map(|r| r.label.clone()).collect();
        let taken_ids: Vec<String> = self.remotes.iter().map(|r| r.id.clone()).collect();

        card(ui, p, |ui| {
            egui::Grid::new("form-main")
                .num_columns(2)
                .spacing([14.0, 10.0])
                .show(ui, |ui| {
                    // 호스트가 먼저입니다: 드라이브 이름이 여기서 나옵니다.
                    field(ui, p, "호스트", |ui| {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut form.host)
                                .hint_text(match form.kind {
                                    DriveKind::Smb => "192.168.0.10 또는 nas.example.com",
                                    DriveKind::Ftp => "ftp.example.com 또는 192.168.0.10",
                                })
                                .desired_width(260.0),
                        );
                        source_changed |= r.changed();
                    });
                    match form.kind {
                        DriveKind::Smb => field(ui, p, "공유 폴더", |ui| {
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut form.share)
                                    .hint_text("예: Public 또는 Public/2026 (안의 폴더까지)")
                                    .desired_width(260.0),
                            );
                            source_changed |= r.changed();
                        }),
                        // FTP에는 공유라는 개념이 없습니다. 계정 전체를 쓰거나,
                        // 그 안의 한 폴더만 드라이브로 띄웁니다.
                        DriveKind::Ftp => field(ui, p, "폴더", |ui| {
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut form.subpath)
                                    .hint_text("계정 전체를 쓰려면 비워 둠 (예: /public_html)")
                                    .desired_width(260.0),
                            );
                            source_changed |= r.changed();
                        }),
                    }
                    // 포트는 어느 종류든 기본 칸에 둡니다. NAS는 공유 포트를
                    // 바꿔 쓰는 일이 흔한데, 이 칸이 "고급 설정" 뒤에 숨어 있으면
                    // 주소 칸에 `host:5445`처럼 적는 수밖에 없습니다.
                    if form.kind == DriveKind::Smb {
                        field(ui, p, "포트", |ui| {
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut form.port).desired_width(90.0),
                            );
                            if r.changed() {
                                form.port_edited = true;
                            }
                            ui.add_space(8.0);
                            ui.label(RichText::new("기본 445").color(p.dim));
                        });
                    }
                    if form.kind == DriveKind::Ftp {
                        field(ui, p, "보안", |ui| {
                            for (value, label) in [
                                ("explicit", "FTPS (권장)"),
                                ("implicit", "FTPS (암묵적)"),
                                ("plain", "일반 FTP"),
                            ] {
                                if ui
                                    .selectable_label(form.security == value, label)
                                    .clicked()
                                    && form.security != value
                                {
                                    form.security = value.to_string();
                                    source_changed = true;
                                }
                            }
                        });
                        // 포트는 보안 설정을 따라 21/990으로 채워지지만, 호스팅마다
                        // 다른 포트를 쓰는 일이 흔해 여기서 바로 고칠 수 있어야 합니다.
                        field(ui, p, "포트", |ui| {
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut form.port).desired_width(90.0),
                            );
                            if r.changed() {
                                form.port_edited = true;
                            }
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(match form.security.as_str() {
                                    "implicit" => "기본 990",
                                    _ => "기본 21",
                                })
                                .color(p.dim),
                            );
                        });
                        // 데이터 연결 방식. 공유기 뒤에서는 패시브만 되고, 일부
                        // 서버는 액티브만 받으므로 고르게 둡니다.
                        field(ui, p, "데이터 연결", |ui| {
                            for (value, label) in [
                                ("passive", "패시브 (권장)"),
                                ("epsv", "패시브 EPSV"),
                                ("active", "액티브"),
                            ] {
                                if ui.selectable_label(form.mode == value, label).clicked()
                                    && form.mode != value
                                {
                                    form.mode = value.to_string();
                                }
                            }
                        });
                        if form.mode == "active" {
                            field(ui, p, "응답 대기", |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut form.active_timeout)
                                        .desired_width(60.0),
                                );
                                ui.add_space(6.0);
                                ui.label(RichText::new("초").color(p.dim));
                            });
                            // 액티브 모드가 쓰는 로컬 포트는 OS가 고릅니다. 범위를
                            // 정하는 설정을 흉내 내는 대신 사실대로 적습니다.
                            field(ui, p, "", |ui| {
                                ui.label(
                                    RichText::new(
                                        "서버가 되돌아 연결할 포트는 OS가 고릅니다 — 이 컴퓨터 \
                                         앞단 방화벽에서 임의 포트 수신이 열려 있어야 합니다",
                                    )
                                    .color(p.dim)
                                    .size(11.0),
                                );
                            });
                        } else {
                            field(ui, p, "", |ui| {
                                ui.checkbox(
                                    &mut form.nat_workaround,
                                    "서버가 알려준 주소 대신 접속한 주소 사용 (공유기 뒤 서버)",
                                );
                            });
                        }
                    }
                    field(ui, p, "이름", |ui| {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut form.name)
                                .hint_text("비워 두면 저장할 때 서버 이름으로")
                                .desired_width(260.0),
                        );
                        // 한 번이라도 손댔으면 다시 채우지 않습니다. 비우는 순간
                        // 다시 채워 넣으면 지우고 새로 적을 방법이 없습니다 - 비운
                        // 채로 저장하면 에이전트가 서버 이름을 넣습니다.
                        if r.changed() {
                            form.name_edited = true;
                        }
                    });
                    field(ui, p, "계정", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut form.user)
                                .hint_text(match form.kind {
                                    DriveKind::Smb => "NAS 로그인 ID",
                                    DriveKind::Ftp => "FTP 로그인 ID (익명이면 anonymous)",
                                })
                                .desired_width(260.0),
                        );
                    });
                    field(ui, p, "비밀번호", |ui| {
                        let hint = if form.editing { "비워 두면 저장된 비밀번호 유지" } else { "" };
                        ui.add(
                            egui::TextEdit::singleline(&mut form.password)
                                .password(true)
                                .hint_text(hint)
                                .desired_width(260.0),
                        );
                    });
                    if cfg!(windows) {
                        field(ui, p, "드라이브 문자", |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut form.drive)
                                    .hint_text("Z:")
                                    .desired_width(90.0),
                            );
                        });
                    } else {
                        field(ui, p, "마운트 폴더", |ui| {
                            let hint = format!("{}/MyDrives/<공유 폴더>", form.home);
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut form.mountpoint)
                                    .hint_text(hint)
                                    .desired_width(260.0),
                            );
                            if r.changed() {
                                form.mountpoint_edited = true;
                            }
                        });
                    }
                });
        });

        ui.add_space(10.0);
        // 글리프가 아니라 말로 씁니다: 한글 폰트에 삼각형 기호가 없는 경우가
        // 있어 □ 로 보입니다.
        let advanced_label = if form.advanced {
            "고급 설정 숨기기"
        } else {
            "고급 설정 보기"
        };
        let toggle = egui::Button::new(RichText::new(advanced_label).color(p.dim))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::NONE);
        if ui.add(toggle).clicked() {
            form.advanced = !form.advanced;
        }

        if form.advanced {
            ui.add_space(6.0);
            card(ui, p, |ui| {
                egui::Grid::new("form-advanced")
                    .num_columns(2)
                    .spacing([14.0, 10.0])
                    .show(ui, |ui| {
                        field(ui, p, "드라이브 이름", |ui| {
                            // 설정 키이자 키체인 계정이라, 있는 드라이브에서는
                            // 바꿀 수 없습니다(바꾸면 다른 드라이브가 됩니다).
                            let r = ui.add_enabled(
                                !form.editing,
                                egui::TextEdit::singleline(&mut form.id)
                                    .hint_text("마운트 폴더 이름이 됩니다")
                                    .desired_width(260.0),
                            );
                            // 위의 "이름"과 같은 이유로, 손댄 뒤에는 따라가지 않습니다.
                            if r.changed() {
                                form.id_edited = true;
                            }
                        });
                        // 포트는 두 종류 모두 기본 칸에 있습니다. 여기서는
                        // 공유 폴더 안의 하위 경로만 받습니다(FTP는 그 칸을
                        // "폴더"로 이미 받았습니다).
                        if form.kind == DriveKind::Smb {
                            field(ui, p, "하위 경로", |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut form.subpath)
                                        .hint_text("공유 폴더 전체를 쓰려면 비워 둠")
                                        .desired_width(260.0),
                                );
                            });
                        }
                        field(ui, p, "옵션", |ui| {
                            ui.checkbox(&mut form.automount, "시작할 때 자동 연결");
                            ui.add_space(10.0);
                            ui.checkbox(&mut form.readonly, "읽기 전용");
                            if form.kind == DriveKind::Ftp && form.security != "plain" {
                                ui.add_space(10.0);
                                ui.checkbox(
                                    &mut form.insecure_tls,
                                    "인증서를 검증하지 않음 (NAS 자체 서명)",
                                );
                            }
                        });
                    });
            });
        }

        // 호스트·공유 이름을 바꾸면 (아직 손대지 않은) 이름과 마운트 폴더가
        // 따라옵니다. 칸이 비었다는 이유로는 다시 채우지 않습니다 - 사용자가
        // 지운 것일 수 있고, 그걸 즉시 되돌리면 칸을 새로 적을 수가 없습니다.
        if source_changed {
            form.follow_sources(&taken, &taken_ids);
        }

        ui.add_space(18.0);
        let busy = self.action_call.is_some();
        ui.horizontal(|ui| {
            let save_label = if form.editing { "저장하고 다시 연결" } else { "저장하고 연결" };
            if primary(ui, p, save_label, !busy).clicked() {
                match form.validate() {
                    Ok(()) => {
                        // 편집에서 비밀번호 칸을 비워 두면 저장된 것을 그대로
                        // 둡니다(None). 새 드라이브는 빈 값도 그대로 보냅니다.
                        let secret = if form.editing && form.password.is_empty() {
                            None
                        } else {
                            Some(form.password.clone())
                        };
                        self.start_action(
                            "저장",
                            Request::Add {
                                remote: form.to_remote(),
                                secret,
                                replace: form.editing,
                            },
                        );
                        self.pending_mount = Some(form.id.trim().to_string());
                        self.selected = Some(form.id.trim().to_string());
                        keep = false;
                    }
                    Err(e) => self.message = Some((e.to_string(), true)),
                }
            }
            if secondary(ui, p, "연결 테스트", !busy).clicked() {
                match form.validate() {
                    Ok(()) => self.start_action(
                        "연결 테스트",
                        Request::Test {
                            remote: form.to_remote(),
                            secret: Some(form.password.clone()),
                        },
                    ),
                    Err(e) => self.message = Some((e.to_string(), true)),
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let cancel = egui::Button::new(RichText::new("취소").color(p.dim))
                    .fill(Color32::TRANSPARENT)
                    .stroke(Stroke::NONE);
                if ui.add(cancel).clicked() {
                    keep = false;
                    self.message = None;
                }
            });
        });

        if keep {
            Some(form)
        } else {
            None
        }
    }
}

// -------------------------------------------------------------- 상태 표시줄

impl App {
    fn draw_statusbar(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal_centered(|ui| {
            if let Some(busy) = &self.action_call {
                ui.add(egui::Spinner::new().size(13.0));
                ui.label(RichText::new(format!("{}...", busy.label)).color(p.dim));
            } else if let Some((text, is_error)) = self.message.clone() {
                let color = if is_error { p.err } else { p.ok };
                // 잘라 버리면 정작 원인이 적힌 뒷부분이 사라집니다 - 줄 안에서는
                // 줄이되, 가리키면 전문을 보여 줍니다.
                ui.label(RichText::new(ellipsize(&text, 70)).color(color))
                    .on_hover_text(&text);
            } else {
                ui.label(
                    RichText::new("비밀번호는 OS 키체인에 저장되며 설정 파일에 남지 않습니다")
                        .small()
                        .color(p.dim),
                );
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.draw_update_chip(ui, p);
            });
        });
    }

    fn draw_update_chip(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let Some(status) = self.update.clone() else {
            return;
        };
        match &status.state {
            UpdateState::Staged { version } => {
                let button = egui::Button::new(
                    RichText::new(format!("{version} 업데이트 적용")).color(p.on_accent),
                )
                .fill(p.accent)
                .corner_radius(6);
                if ui.add(button).on_hover_text("에이전트를 다시 시작합니다").clicked() {
                    self.start_action(
                        "업데이트 적용",
                        Request::Update {
                            action: UpdateAction::Apply,
                        },
                    );
                }
            }
            UpdateState::Downloading { .. } | UpdateState::Applying { .. } => {
                ui.label(RichText::new(status.summary()).small().color(p.dim));
                ui.add(egui::Spinner::new().size(12.0));
            }
            UpdateState::Disabled => {}
            _ => {
                let check = egui::Button::new(RichText::new("업데이트 확인").color(p.dim))
                    .fill(Color32::TRANSPARENT)
                    .stroke(Stroke::NONE);
                if ui
                    .add(check)
                    .on_hover_text(format!("버전 {}", status.current))
                    .clicked()
                {
                    self.start_action(
                        "업데이트 확인",
                        Request::Update {
                            action: UpdateAction::Check,
                        },
                    );
                }
            }
        }
    }
}

// ------------------------------------------------------------------- 위젯들

/// 눌러야 할 단 하나의 버튼. 화면당 하나만 씁니다.
fn primary(ui: &mut egui::Ui, p: &Palette, text: &str, enabled: bool) -> egui::Response {
    let button = egui::Button::new(RichText::new(text).color(p.on_accent))
        .fill(p.accent)
        .stroke(Stroke::NONE)
        .corner_radius(7)
        .min_size(Vec2::new(0.0, 28.0));
    ui.add_enabled(enabled, button)
}

fn secondary(ui: &mut egui::Ui, p: &Palette, text: &str, enabled: bool) -> egui::Response {
    let button = egui::Button::new(RichText::new(text).color(p.text))
        .fill(p.field)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(7)
        .min_size(Vec2::new(0.0, 28.0));
    ui.add_enabled(enabled, button)
}

/// 상태를 나타내는 작은 알약. 색이 정보이고 글자는 확인입니다.
fn pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::NONE
        .fill(color.linear_multiply(0.18))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(9, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().color(color));
        });
}

/// 내용 묶음 하나. macOS 설정의 그룹 상자에 해당합니다.
fn card(ui: &mut egui::Ui, p: &Palette, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::NONE
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(16, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width() - 2.0);
            add(ui);
        });
}

/// 라벨 + 임의의 위젯 한 줄. [`info_row`]와 같은 자리에 맞춰 그립니다.
fn widget_row(ui: &mut egui::Ui, p: &Palette, key: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.add_sized(
            Vec2::new(92.0, 18.0),
            egui::Label::new(RichText::new(key).color(p.dim)).halign(egui::Align::LEFT),
        );
        add(ui);
    });
    ui.add_space(4.0);
}

fn info_row(ui: &mut egui::Ui, p: &Palette, key: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            Vec2::new(92.0, 18.0),
            egui::Label::new(RichText::new(key).color(p.dim)).halign(egui::Align::LEFT),
        );
        ui.label(RichText::new(value).color(p.text));
    });
    ui.add_space(4.0);
}

/// 폼의 한 줄: 오른쪽 맞춤 라벨 + 입력칸.
fn field(ui: &mut egui::Ui, p: &Palette, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.add_space(2.0);
        ui.label(RichText::new(label).color(p.dim));
    });
    ui.horizontal(|ui| add(ui));
    ui.end_row();
}

fn state_color(state: &MountState, p: &Palette) -> Color32 {
    match state {
        MountState::Mounted { .. } => p.ok,
        MountState::Offline { .. } => p.warn,
        MountState::Failed { .. } => p.err,
        MountState::Mounting | MountState::Unmounted => p.dim,
    }
}

fn state_badge(state: &MountState, p: &Palette) -> (String, Color32) {
    match state {
        MountState::Mounted { .. } => ("연결됨".into(), p.ok),
        MountState::Offline { .. } => ("연결 끊김".into(), p.warn),
        MountState::Failed { .. } => ("실패".into(), p.err),
        MountState::Mounting => ("연결 중".into(), p.dim),
        MountState::Unmounted => ("연결 안 됨".into(), p.dim),
    }
}

fn ellipsize(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let keep: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{keep}…")
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

fn open_path(path: &str) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(path).spawn()
    } else if cfg!(windows) {
        std::process::Command::new("explorer").arg(path).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(path).spawn()
    };
    if let Err(e) = result {
        tracing::warn!(path, error = %e, "경로를 열 수 없습니다");
    }
}

/// 에이전트가 꺼져 있을 때 창에서 바로 띄울 수 있게 합니다.
fn start_agent() {
    let exe_name = if cfg!(windows) {
        "mydrivesd.exe"
    } else {
        "mydrivesd"
    };
    let beside_us = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(exe_name)));
    for candidate in beside_us
        .into_iter()
        .chain(std::iter::once(std::path::PathBuf::from(exe_name)))
    {
        if std::process::Command::new(&candidate)
            .arg("run")
            .spawn()
            .is_ok()
        {
            return;
        }
    }
    tracing::error!("에이전트를 시작할 수 없습니다 ({exe_name})");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_and_the_folder_both_default_to_the_share() {
        let mut form = Form {
            home: "/Users/x".into(),
            ..Form::default()
        };
        form.host = "turtlelab.ddns.net".into();
        form.share = "Public".into();
        form.follow_sources(&[], &[]);

        // 드라이브 이름(id), 마운트 폴더, 화면에 보이는 이름이 모두 공유 폴더
        // 이름을 기본값으로 합니다. Finder가 보여 주는 것이 폴더 이름이므로
        // 표시 이름이 그와 같아야 헷갈리지 않습니다.
        assert_eq!(form.id, "Public");
        assert_eq!(form.name, "Public");
        if !cfg!(windows) {
            assert_eq!(form.mountpoint, "/Users/x/MyDrives/Public");
        }

        // 이름을 다른 것으로 고치면 그때부터는 따라가지 않습니다 - 폴더는
        // 그대로 두고 표시 이름만 바뀝니다.
        form.name = "거북이 NAS".into();
        form.name_edited = true;
        form.share = "Other".into();
        form.id_edited = false;
        form.mountpoint_edited = false;
        form.follow_sources(&[], &[]);
        assert_eq!(form.id, "Other");
        assert_eq!(form.name, "거북이 NAS");
        if !cfg!(windows) {
            assert_eq!(form.mountpoint, "/Users/x/MyDrives/Other");
        }
    }

    #[test]
    fn an_ftp_drive_is_named_after_the_folder_it_exposes() {
        let mut form = Form {
            home: "/Users/x".into(),
            kind: DriveKind::Ftp,
            ..Form::default()
        };
        form.host = "files.example.com".into();
        form.subpath = "/public_html".into();
        form.follow_sources(&[], &[]);

        assert_eq!(form.id, "public_html");
        assert_eq!(form.name, "public_html");
        // 명시적 FTPS는 평문과 같은 포트에서 시작합니다.
        assert_eq!(form.port, "21");

        // 계정 전체를 쓰면 이름 삼을 폴더가 없으므로 호스트를 따릅니다.
        form.subpath.clear();
        form.follow_sources(&[], &[]);
        assert_eq!(form.id, "files");

        // 암묵적 FTPS만 포트가 다릅니다.
        form.security = "implicit".into();
        form.follow_sources(&[], &[]);
        assert_eq!(form.port, "990");

        // 포트를 직접 고치면 보안 설정이 바뀌어도 그 값을 지킵니다.
        form.port = "2121".into();
        form.port_edited = true;
        form.security = "plain".into();
        form.follow_sources(&[], &[]);
        assert_eq!(form.port, "2121");

        // 데이터 연결은 패시브가 기본입니다.
        assert_eq!(form.mode, "passive");
        assert_eq!(
            match form.to_remote().backend {
                BackendConfig::Ftp(ftp) => (ftp.mode, ftp.port, ftp.active_timeout_secs),
                _ => panic!("FTP 백엔드여야 합니다"),
            },
            ("passive".to_string(), 2121, 10)
        );

        // 공유 폴더는 FTP에 없는 개념이라 검사하지 않습니다.
        form.drive = "Z:".into();
        form.mountpoint = "/Users/x/MyDrives/files".into();
        assert!(form.validate().is_ok());
    }

    /// 사용자가 실제로 당한 일: NAS 드라이브 `jazzlife`가 있는데 같은 계정의
    /// FTP를 `/jazzlife` 폴더로 추가했더니, 기본 이름이 똑같이 `jazzlife`라
    /// **기존 NAS 드라이브 설정이 통째로 교체**됐습니다.
    /// 사용자가 실제로 친 입력: 주소 칸에 포트까지, 공유 폴더에 `/`를 붙여서.
    /// 이걸 그대로 넘기면 SMB가 `bad SMB target: Share name...` 으로 거절합니다.
    #[test]
    fn the_form_accepts_what_people_actually_type() {
        let mut form = Form {
            home: "/Users/x".into(),
            ..Form::default()
        };
        form.host = "turtlelab.ddns.net:5545".into();
        form.share = "/jazzlife".into();
        form.user = "jazzlife".into();
        form.follow_sources(&[], &[]);

        // 주소와 포트가 갈라집니다.
        assert_eq!(form.host, "turtlelab.ddns.net");
        assert_eq!(form.port, "5545");
        // 드라이브 이름과 마운트 경로에 슬래시가 섞이지 않습니다.
        assert_eq!(form.id, "jazzlife");
        if !cfg!(windows) {
            assert_eq!(form.mountpoint, "/Users/x/MyDrives/jazzlife");
        }

        match form.to_remote().backend {
            BackendConfig::Smb(smb) => {
                assert_eq!(smb.host, "turtlelab.ddns.net");
                assert_eq!(smb.port, 5545);
                assert_eq!(smb.share, "jazzlife");
            }
            _ => panic!("SMB 백엔드여야 합니다"),
        }
    }

    #[test]
    fn addresses_and_shares_are_tidied_without_being_mangled() {
        // 포트가 없는 주소, IPv6 주소는 그대로 둡니다.
        assert_eq!(split_host_port("nas.example.com"), None);
        assert_eq!(split_host_port("[fe80::1]:445"), None);
        assert_eq!(split_host_port("fe80::1"), None);
        assert_eq!(
            split_host_port("192.168.0.10:445"),
            Some(("192.168.0.10".to_string(), 445))
        );

        // 공유 폴더 칸: 첫 조각이 공유, 나머지가 그 안의 폴더입니다.
        assert_eq!(split_share_path("/jazzlife"), ("jazzlife".into(), None));
        assert_eq!(split_share_path("  Public/  "), ("Public".into(), None));
        assert_eq!(split_share_path(""), (String::new(), None));
        assert_eq!(
            split_share_path("jazzlife/Study"),
            ("jazzlife".into(), Some("/Study".into()))
        );
        assert_eq!(
            split_share_path("/jazzlife/Study/2026"),
            ("jazzlife".into(), Some("/Study/2026".into()))
        );
        // UNC 경로의 첫 조각은 호스트라 공유가 아닙니다.
        assert_eq!(
            split_share_path("\\\\nas\\jazzlife\\Study"),
            ("jazzlife".into(), Some("/Study".into()))
        );

        // 하위 경로는 항상 / 로 시작합니다.
        assert_eq!(clean_subpath("photos/2026"), Some("/photos/2026".into()));
        assert_eq!(clean_subpath("/photos/"), Some("/photos".into()));
        assert_eq!(clean_subpath("   "), None);
    }

    /// 이름 칸을 비우고 새로 적으려 하면, 비는 순간 호스트 이름으로 다시
    /// 채워져 적을 수가 없었습니다. 손댄 칸은 그대로 둬야 합니다.
    /// 사용자가 겪은 일: 공유 폴더 칸에 `jazzlife/Study`라 적었는데 드라이브는
    /// 공유의 맨 위를 보여줬습니다. 폴더까지 적었으면 그 폴더가 맨 위여야 합니다.
    #[test]
    fn a_folder_inside_the_share_becomes_the_top_of_the_drive() {
        let mut form = Form {
            home: "/Users/x".into(),
            ..Form::default()
        };
        form.host = "turtlelab.ddns.net".into();
        form.share = "jazzlife/Study".into();
        form.follow_sources(&[], &[]);

        // 드라이브 이름과 마운트 폴더는 들어가는 폴더를 따릅니다.
        assert_eq!(form.id, "Study");
        if !cfg!(windows) {
            assert_eq!(form.mountpoint, "/Users/x/MyDrives/Study");
        }
        match form.to_remote().backend {
            BackendConfig::Smb(smb) => {
                assert_eq!(smb.share, "jazzlife");
                assert_eq!(smb.subpath.as_deref(), Some("/Study"));
            }
            _ => panic!("SMB 백엔드여야 합니다"),
        }
    }

    #[test]
    fn a_field_the_user_cleared_stays_cleared() {
        let mut form = Form {
            home: "/Users/x".into(),
            ..Form::default()
        };
        form.host = "turtlelab.ddns.net".into();
        form.share = "jazzlife".into();
        form.follow_sources(&[], &[]);
        assert_eq!(form.name, "jazzlife");

        // 사용자가 이름을 지웠다 (= changed 이벤트, 값은 빈 문자열).
        form.name.clear();
        form.name_edited = true;
        // 호스트를 고쳐도 이름은 다시 채워지지 않습니다.
        form.host = "other.example.com".into();
        form.follow_sources(&[], &[]);
        assert_eq!(form.name, "");

        // 드라이브 이름(id)도 마찬가지입니다.
        form.id.clear();
        form.id_edited = true;
        form.share = "Public".into();
        form.follow_sources(&[], &[]);
        assert_eq!(form.id, "");
    }

    #[test]
    fn a_new_drive_never_takes_a_name_another_drive_is_using() {
        let mut form = Form {
            home: "/Users/x".into(),
            kind: DriveKind::Ftp,
            ..Form::default()
        };
        form.host = "turtlelab.ddns.net".into();
        form.subpath = "/jazzlife".into();

        // 비어 있을 때는 폴더 이름 그대로.
        form.follow_sources(&[], &[]);
        assert_eq!(form.id, "jazzlife");

        // 그 이름을 쓰는 드라이브가 이미 있으면 다른 이름을 제안합니다.
        form.follow_sources(&[], &["jazzlife".to_string()]);
        assert_eq!(form.id, "jazzlife-2");
        if !cfg!(windows) {
            assert_eq!(form.mountpoint, "/Users/x/MyDrives/jazzlife-2");
        }

        form.follow_sources(&[], &["jazzlife".to_string(), "jazzlife-2".to_string()]);
        assert_eq!(form.id, "jazzlife-3");
    }

    #[test]
    fn only_one_settings_window_at_a_time() {
        let dir = std::env::temp_dir().join(format!("mydrives-ui-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // 아무도 없으면 자리를 차지합니다.
        assert!(claim_the_window_in(&dir));
        assert!(dir.join(LOCK_NAME).exists());
        assert!(!dir.join(RAISE_NAME).exists());

        // 살아 있는 창이 있으면 물러나면서 "앞으로 나와라"만 남깁니다.
        assert!(!claim_the_window_in(&dir));
        assert!(dir.join(RAISE_NAME).exists());

        // 창이 죽어 표시가 낡으면 다음 실행이 자리를 넘겨받습니다 — 지워지지
        // 못한 잠금 파일 하나로 설정 창이 영영 안 열리면 안 됩니다.
        let stale = std::time::SystemTime::now() - STALE_AFTER - Duration::from_secs(1);
        let lock = std::fs::File::options()
            .write(true)
            .open(dir.join(LOCK_NAME))
            .expect("잠금 파일");
        lock.set_modified(stale).expect("시각 되돌리기");
        drop(lock);
        assert!(claim_the_window_in(&dir));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_labels_are_shortened_for_the_sidebar() {
        assert_eq!(ellipsize("짧음", 10), "짧음");
        let long = "매우".repeat(20);
        assert_eq!(ellipsize(&long, 8).chars().count(), 8);
    }

    #[test]
    fn validation_names_the_missing_field() {
        let form = Form::default();
        assert!(form.validate().unwrap_err().contains("공유 폴더"));
    }
}

/// 소스 리스트 한 줄에 들어갈 짧은 설명.
fn short_state(state: &MountState) -> String {
    match state {
        MountState::Mounted { mountpoint, .. } | MountState::Offline { mountpoint, .. } => {
            mountpoint.clone()
        }
        MountState::Failed { .. } => "연결 실패".into(),
        MountState::Mounting => "연결 중...".into(),
        MountState::Unmounted => "연결되지 않음".into(),
    }
}

/// 상세 화면의 "상태" 줄. 위치는 따로 있으므로 경로를 되풀이하지 않습니다.
fn state_detail(state: &MountState) -> String {
    match state {
        MountState::Mounted { provider, .. } => format!("연결됨 · {provider} 제공자"),
        MountState::Offline { reason, .. } => format!("연결 끊김 — {reason}"),
        MountState::Failed { reason } => format!("실패 — {reason}"),
        MountState::Mounting => "마운트 중...".into(),
        MountState::Unmounted => "연결되지 않음".into(),
    }
}
