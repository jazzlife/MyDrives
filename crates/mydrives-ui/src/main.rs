//! MyDrives 설정 창.
//!
//! 트레이/메뉴바의 "드라이브 관리..."가 이 프로세스를 띄웁니다. 에이전트와는 제어
//! 소켓(IPC)으로만 대화하므로, 이 창이 죽어도 마운트는 그대로 유지됩니다.
//!
//! 화면은 macOS 시스템 설정과 같은 두 칸 구조입니다 — 왼쪽은 드라이브 목록(소스
//! 리스트), 오른쪽은 선택한 드라이브의 상세. 추가도 같은 자리에서 하므로 "추가"와
//! "관리"가 따로 있지 않습니다.

// 창만 띄우고 콘솔은 띄우지 않습니다(Windows).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod theme;

use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, CornerRadius, FontId, Margin, RichText, Sense, Stroke, Vec2};

use mydrives_core::config::{BackendConfig, RemoteConfig, SecretRef, SmbConfig};
use mydrives_core::manager::{MountState, RemoteStatus};
use mydrives_core::update::{UpdateState, UpdateStatus};
use mydrives_ipc::{Request, Response, UpdateAction};

use theme::Palette;

const REFRESH: Duration = Duration::from_millis(1500);
const SIDEBAR_WIDTH: f32 = 236.0;
const ROW_HEIGHT: f32 = 48.0;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

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
}

/// 추가 폼. 심플 버전은 SMB(NAS)만 받습니다.
struct Form {
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
}

impl Default for Form {
    fn default() -> Self {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default();
        Form {
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
        }
    }
}

impl Form {
    /// 입력한 값에서 나머지를 채웁니다(사용자가 직접 고친 칸은 건드리지 않음).
    ///
    /// 보이는 **이름**은 제품(NAS·서비스) 이름을 따르고, **id와 마운트 폴더**는
    /// 공유 폴더 이름을 따릅니다 — 이름은 "어느 장비인가"를, 경로는 "어느
    /// 폴더인가"를 말해야 하기 때문입니다.
    fn follow_sources(&mut self, taken: &[String]) {
        let share = self.share.trim().to_string();
        if !self.id_edited {
            self.id = share.clone();
        }
        if !self.name_edited {
            let backend = self.to_remote().backend;
            self.name = mydrives_core::naming::unique_label(
                &mydrives_core::naming::product_name(&backend),
                &share,
                taken,
            );
        }
        if !self.mountpoint_edited && !cfg!(windows) {
            // 공유 이름을 받기 전에는 비워 둡니다. ".../MyDrives/drive" 같은
            // 자리표시 경로가 들어가 있으면 진짜 값처럼 보입니다.
            self.mountpoint = match self.id.trim() {
                "" => String::new(),
                name => format!("{}/MyDrives/{name}", self.home),
            };
        }
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self.share.trim().is_empty() {
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

    fn to_remote(&self) -> RemoteConfig {
        RemoteConfig {
            id: self.id.trim().to_string(),
            // 비워 두면 에이전트가 제품 이름으로 채웁니다.
            label: self.name.trim().to_string(),
            automount: self.automount,
            readonly: self.readonly,
            provider: None,
            drive: opt(&self.drive),
            mountpoint: opt(&self.mountpoint),
            backend: BackendConfig::Smb(SmbConfig {
                host: self.host.trim().to_string(),
                port: self.port.trim().parse().unwrap_or(445),
                share: self.share.trim().to_string(),
                user: self.user.trim().to_string(),
                subpath: opt(&self.subpath),
                // 저장은 에이전트가 OS 키체인에 합니다.
                secret: SecretRef::Keychain,
            }),
        }
    }
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
        painter.text(
            egui::pos2(rect.left() + 28.0, rect.center().y + 8.0),
            Align2::LEFT_CENTER,
            ellipsize(&short_state(&r.state), 26),
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
            info_row(ui, p, "드라이브 이름", &r.id);
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
        ui.label(RichText::new("새 드라이브").size(20.0).strong().color(p.text));
        ui.add_space(2.0);
        ui.label(
            RichText::new("QNAP·Synology 등 NAS의 공유 폴더를 드라이브로 연결합니다")
                .color(p.dim),
        );
        ui.add_space(16.0);

        let mut keep = true;
        let mut source_changed = false;
        // 이름이 이미 쓰이고 있는지 보려면 지금 있는 드라이브들의 이름이 필요합니다.
        let taken: Vec<String> = self
            .remotes
            .iter()
            .map(|r| r.label.clone())
            .collect();

        card(ui, p, |ui| {
            egui::Grid::new("form-main")
                .num_columns(2)
                .spacing([14.0, 10.0])
                .show(ui, |ui| {
                    // 호스트가 먼저입니다: 드라이브 이름이 여기서 나옵니다.
                    field(ui, p, "호스트", |ui| {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut form.host)
                                .hint_text("192.168.0.10 또는 nas.example.com")
                                .desired_width(260.0),
                        );
                        source_changed |= r.changed();
                    });
                    field(ui, p, "공유 폴더", |ui| {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut form.share)
                                .hint_text("예: Public")
                                .desired_width(260.0),
                        );
                        source_changed |= r.changed();
                    });
                    field(ui, p, "이름", |ui| {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut form.name)
                                .hint_text("NAS 이름 (비워 두면 자동)")
                                .desired_width(260.0),
                        );
                        if r.changed() {
                            form.name_edited = !form.name.trim().is_empty();
                        }
                    });
                    field(ui, p, "계정", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut form.user)
                                .hint_text("NAS 로그인 ID")
                                .desired_width(260.0),
                        );
                    });
                    field(ui, p, "비밀번호", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut form.password)
                                .password(true)
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
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut form.id)
                                    .hint_text("비워 두면 공유 폴더 이름")
                                    .desired_width(260.0),
                            );
                            if r.changed() {
                                form.id_edited = !form.id.trim().is_empty();
                            }
                        });
                        field(ui, p, "포트", |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut form.port).desired_width(90.0),
                            );
                        });
                        field(ui, p, "하위 경로", |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut form.subpath)
                                    .hint_text("공유 폴더 전체를 쓰려면 비워 둠")
                                    .desired_width(260.0),
                            );
                        });
                        field(ui, p, "옵션", |ui| {
                            ui.checkbox(&mut form.automount, "시작할 때 자동 연결");
                            ui.add_space(10.0);
                            ui.checkbox(&mut form.readonly, "읽기 전용");
                        });
                    });
            });
        }

        // 호스트·공유 이름을 바꾸면 이름과 마운트 폴더가 따라옵니다.
        if source_changed || form.mountpoint.is_empty() || form.name.is_empty() {
            form.follow_sources(&taken);
        }

        ui.add_space(18.0);
        let busy = self.action_call.is_some();
        ui.horizontal(|ui| {
            if primary(ui, p, "저장하고 연결", !busy).clicked() {
                match form.validate() {
                    Ok(()) => {
                        self.start_action(
                            "저장",
                            Request::Add {
                                remote: form.to_remote(),
                                secret: Some(form.password.clone()),
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
                ui.label(RichText::new(ellipsize(&text, 70)).color(color));
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
    fn the_path_follows_the_share_and_the_name_follows_the_nas() {
        let mut form = Form {
            home: "/Users/x".into(),
            ..Form::default()
        };
        form.host = "turtlelab.ddns.net".into();
        form.share = "Public".into();
        form.follow_sources(&[]);

        // 경로는 공유 폴더를, 이름은 장비를 따릅니다.
        assert_eq!(form.id, "Public");
        assert_eq!(form.name, "turtlelab");
        if !cfg!(windows) {
            assert_eq!(form.mountpoint, "/Users/x/MyDrives/Public");
        }

        // 같은 NAS의 두 번째 공유는 이름이 겹치지 않게 공유 이름이 붙습니다.
        form.follow_sources(&["turtlelab".to_string()]);
        assert_eq!(form.name, "turtlelab (Public)");

        // 사용자가 직접 고친 칸은 따라가지 않습니다.
        form.id = "nas".into();
        form.id_edited = true;
        form.name = "거북이".into();
        form.name_edited = true;
        form.share = "Other".into();
        form.follow_sources(&[]);
        assert_eq!(form.id, "nas");
        assert_eq!(form.name, "거북이");
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
