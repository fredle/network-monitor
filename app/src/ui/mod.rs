//! The window process. Launched on demand by the tray daemon (`--ui status|settings|popup`),
//! connects to the daemon over the named pipe, and exits when its window closes,
//! so none of this (OpenGL context, fonts, textures) sits in memory while idle.

mod chart;
mod settings_panel;

use crate::ipc::{self, Connection};
use crate::model::{Message, Request, Sample, Snapshot, UpdateStatus};
use crate::paths::APP_TITLE;
use crate::settings::Settings;
use eframe::egui::{self, Color32, RichText, ViewportCommand};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use windows::core::HSTRING;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, POINT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE};

const POPUP_SIZE: [f32; 2] = [400.0, 240.0];
const WINDOW_SIZE: [f32; 2] = [760.0, 600.0];
const RECONNECT_EVERY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Status,
    Settings,
    Popup,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Status,
    Settings,
    About,
}

pub fn run(kind: &str, anchor: Option<(i32, i32)>) -> i32 {
    let kind = match kind {
        "settings" => Kind::Settings,
        "popup" => Kind::Popup,
        _ => Kind::Status,
    };

    // One full window per user: a second request just brings the first to the front.
    let _guard = if kind != Kind::Popup {
        match single_instance_or_focus() {
            Some(h) => Some(h),
            None => return 0,
        }
    } else {
        None
    };

    let icon = egui::IconData { rgba: crate::icon::app_rgba(64), width: 64, height: 64 };
    let mut vp = egui::ViewportBuilder::default().with_icon(icon).with_title(APP_TITLE);
    vp = match kind {
        Kind::Popup => {
            let pos = place_popup(anchor.unwrap_or((0, 0)), POPUP_SIZE);
            vp.with_title("Network Monitor popup")
                .with_inner_size(POPUP_SIZE)
                .with_position(pos)
                .with_decorations(false)
                .with_always_on_top()
                .with_taskbar(false)
                .with_resizable(false)
        }
        _ => vp.with_inner_size(WINDOW_SIZE).with_min_inner_size([520.0, 420.0]),
    };
    let opts = eframe::NativeOptions { viewport: vp, centered: kind != Kind::Popup, ..Default::default() };
    let tab = if kind == Kind::Settings { Tab::Settings } else { Tab::Status };

    let result = eframe::run_native(
        APP_TITLE,
        opts,
        Box::new(move |cc| {
            // Always dark, regardless of the system theme: the chart is designed for it.
            cc.egui_ctx.set_theme(egui::ThemePreference::Dark);
            cc.egui_ctx.all_styles_mut(|style| {
                style.visuals.panel_fill = Color32::from_rgb(24, 24, 28);
                style.visuals.window_fill = Color32::from_rgb(24, 24, 28);
            });
            Ok(Box::new(UiApp::new(kind, tab, cc.egui_ctx.clone())))
        }),
    );
    if result.is_err() { 1 } else { 0 }
}

/// Returns a guard handle if we are the only UI window; otherwise focuses the
/// existing one and returns None.
fn single_instance_or_focus() -> Option<windows::Win32::Foundation::HANDLE> {
    unsafe {
        let h = CreateMutexW(None, true, &HSTRING::from("Local\\NetworkMonitor.UI.Window")).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(w) = FindWindowW(None, &HSTRING::from(APP_TITLE)) {
                let _ = ShowWindow(w, SW_RESTORE);
                let _ = SetForegroundWindow(w);
            }
            return None;
        }
        Some(h)
    }
}

/// Place the popup next to the tray icon inside the monitor's work area.
fn place_popup(anchor: (i32, i32), size_logical: [f32; 2]) -> egui::Pos2 {
    unsafe {
        let pt = POINT { x: anchor.0, y: anchor.1 };
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let scale = dx as f32 / 96.0;
        if !GetMonitorInfoW(mon, &mut info).as_bool() {
            return egui::pos2(100.0, 100.0);
        }
        let work = info.rcWork;
        let (w, h) = (size_logical[0] * scale, size_logical[1] * scale);
        let margin = 8.0 * scale;
        let x = (anchor.0 as f32 - w / 2.0).clamp(work.left as f32 + margin, (work.right as f32 - w - margin).max(work.left as f32));
        let mid = (work.top + work.bottom) as f32 / 2.0;
        let y = if anchor.1 as f32 > mid { work.bottom as f32 - h - margin } else { work.top as f32 + margin };
        egui::pos2(x / scale, y / scale)
    }
}

pub(crate) struct Data {
    pub connected: bool,
    pub settings: Settings,
    pub history: VecDeque<Sample>,
    pub snap: Snapshot,
    pub notice: Option<(String, Instant)>,
}

struct UiApp {
    kind: Kind,
    tab: Tab,
    ctx: egui::Context,
    conn: Option<Connection>,
    next_connect: Instant,
    data: Data,
    panel: settings_panel::State,
    had_focus: bool,
    opened_at: Instant,
}

impl UiApp {
    fn new(kind: Kind, tab: Tab, ctx: egui::Context) -> Self {
        Self {
            kind,
            tab,
            ctx,
            conn: None,
            next_connect: Instant::now(),
            data: Data {
                connected: false,
                settings: Settings::default(),
                history: VecDeque::new(),
                snap: Snapshot::default(),
                notice: None,
            },
            panel: settings_panel::State::default(),
            had_focus: false,
            opened_at: Instant::now(),
        }
    }

    fn send(&self, req: Request) {
        if let Some(c) = &self.conn {
            let _ = c.requests.send(req);
        }
    }

    fn pump(&mut self) {
        if self.conn.is_none() && Instant::now() >= self.next_connect {
            self.next_connect = Instant::now() + RECONNECT_EVERY;
            let ctx = self.ctx.clone();
            if let Ok(c) = ipc::connect(move || ctx.request_repaint()) {
                let _ = c.requests.send(Request::Subscribe);
                self.conn = Some(c);
            }
        }
        let mut lost = false;
        if let Some(c) = &self.conn {
            loop {
                match c.messages.try_recv() {
                    Ok(m) => apply_message(&mut self.data, &mut self.panel, m),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        lost = true;
                        break;
                    }
                }
            }
        }
        if lost {
            self.conn = None;
            self.data.connected = false;
            self.next_connect = Instant::now() + RECONNECT_EVERY;
        }
        if let Some((_, at)) = &self.data.notice {
            if at.elapsed() > Duration::from_secs(8) {
                self.data.notice = None;
            }
        }
    }
}

fn apply_message(d: &mut Data, panel: &mut settings_panel::State, m: Message) {
    match m {
        Message::Hello { settings, history, snapshot } => {
            d.connected = true;
            d.settings = *settings;
            d.history = history.into();
            d.snap = *snapshot;
            panel.sync_from(&d.settings);
        }
        Message::Tick { sample, snapshot } => {
            d.history.push_back(sample);
            let cutoff = sample.t - d.settings.history_minutes as i64 * 60_000;
            while d.history.front().map_or(false, |s| s.t < cutoff) {
                d.history.pop_front();
            }
            while d.history.len() > d.settings.history_max_samples as usize {
                d.history.pop_front();
            }
            d.snap = *snapshot;
        }
        Message::State(s) => d.snap = *s,
        Message::SettingsApplied(s) => {
            d.settings = *s;
            panel.applied(&d.settings);
        }
        Message::Notice(text) => d.notice = Some((text, Instant::now())),
    }
}

impl eframe::App for UiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump();
        let ctx = ui.ctx().clone();
        // Repaint at least once a second so the clock and "reconnecting" state stay live.
        ctx.request_repaint_after(Duration::from_millis(1000));

        if self.kind == Kind::Popup {
            self.popup_ui(ui, &ctx);
            return;
        }

        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                for (t, label) in [(Tab::Status, "Status"), (Tab::Settings, "Settings"), (Tab::About, "About")] {
                    if ui.selectable_label(self.tab == t, RichText::new(label).size(15.0)).clicked() {
                        self.tab = t;
                    }
                }
                if !self.data.connected {
                    ui.colored_label(Color32::from_rgb(230, 160, 40), "  Connecting to the monitor...");
                }
            });
        });

        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Status => self.status_ui(ui),
            Tab::Settings => {
                if let Some(req) = self.panel.show(ui, &self.data) {
                    self.send(req);
                }
            }
            Tab::About => self.about_ui(ui),
        });
        if let Some((text, _)) = &self.data.notice {
            egui::Panel::bottom("notice").show(ui, |ui| {
                ui.colored_label(Color32::from_rgb(230, 160, 40), text);
            });
        }
    }
}

impl UiApp {
    fn popup_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(false));
        if focused {
            self.had_focus = true;
        } else if self.had_focus || self.opened_at.elapsed() > Duration::from_secs(3) {
            // Lost focus (clicked elsewhere), or never got it: close like a flyout.
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        if self.opened_at.elapsed() < Duration::from_millis(200) {
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::from_rgb(24, 24, 28))).show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(6.0);
                ui.label(RichText::new(summary_line(&self.data.snap)).monospace());
            });
            ui.add_space(2.0);
            self.data.history.make_contiguous();
            let (samples, _) = self.data.history.as_slices();
            let size = ui.available_size();
            chart::draw(ui, size, samples, self.data.settings.high_latency_ms * 3 / 4);
        });
    }

    fn status_ui(&mut self, ui: &mut egui::Ui) {
        let snap = self.data.snap.clone();
        let w = snap.wifi.clone();
        ui.add_space(6.0);
        egui::Grid::new("status").num_columns(4).spacing([24.0, 4.0]).show(ui, |ui| {
            let dash = "-".to_string();
            let kv = |ui: &mut egui::Ui, k: &str, v: String| {
                ui.label(RichText::new(k).color(Color32::from_rgb(150, 150, 165)));
                ui.label(RichText::new(v).monospace());
            };
            kv(ui, "SSID", w.as_ref().map_or(dash.clone(), |w| w.ssid.clone()));
            kv(ui, "BSSID", w.as_ref().map_or(dash.clone(), |w| w.bssid.clone()));
            ui.end_row();
            kv(ui, "Band / channel", w.as_ref().map_or(dash.clone(), |w| format!("{} / {}", w.band, w.channel)));
            kv(ui, "Signal", w.as_ref().map_or(dash.clone(), |w| format!("{}%  ({} dBm)", w.signal_pct, w.rssi)));
            ui.end_row();
            kv(ui, "Rate", w.as_ref().map_or(dash.clone(), |w| format!("{}/{} Mbps", w.rx_mbps, w.tx_mbps)));
            kv(ui, "Adapter", snap.adapter.clone().unwrap_or(dash.clone()));
            ui.end_row();
            kv(ui, "Ping", ping_text(&snap));
            kv(ui, "Drops", format!("{} / {}", snap.drops, snap.total));
            ui.end_row();
            kv(ui, "Driver events", snap.driver_events.to_string());
            kv(ui, "Roams / link losses", format!("{} / {}{}", snap.roams, snap.link_loss, if snap.reroaming { "   [RE-ROAMING]" } else { "" }));
            ui.end_row();
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.add_enabled(!snap.reroaming && snap.wifi.is_some(), egui::Button::new("Force re-roam now")).clicked() {
                self.send(Request::Reroam);
            }
            if ui.button("Open log folder").clicked() {
                self.send(Request::OpenLogFolder);
            }
        });
        ui.add_space(8.0);
        self.data.history.make_contiguous();
        let (samples, _) = self.data.history.as_slices();
        let size = ui.available_size();
        chart::draw(ui, size, samples, self.data.settings.high_latency_ms * 3 / 4);
    }

    fn about_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.heading(APP_TITLE);
        ui.label(format!("Version {}", self.data.snap.version));
        ui.add_space(10.0);
        ui.label(RichText::new("Updates").strong());
        ui.label(update_text(&self.data.snap.update));
        ui.horizontal(|ui| {
            let idle = !matches!(
                self.data.snap.update,
                UpdateStatus::Checking | UpdateStatus::Downloading(_) | UpdateStatus::ManagedByStore
            );
            if let UpdateStatus::Ready(_) = self.data.snap.update {
                if ui.button("Restart to update").clicked() {
                    self.send(Request::ApplyUpdate);
                }
            } else if ui.add_enabled(idle, egui::Button::new("Check for updates")).clicked() {
                self.send(Request::CheckUpdate);
            }
        });
        ui.add_space(10.0);
        ui.label(RichText::new("Data").strong());
        ui.label(crate::paths::data_dir().display().to_string());
        ui.add_space(14.0);
        if ui.button("Quit Network Monitor").clicked() {
            self.send(Request::Quit);
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        }
    }
}

pub(crate) fn update_text(u: &UpdateStatus) -> String {
    match u {
        UpdateStatus::Unavailable => "Automatic updates are not available for this build.".into(),
        UpdateStatus::ManagedByStore => "Updates are delivered by the Microsoft Store.".into(),
        UpdateStatus::Idle => "Up to date (not checked yet this session).".into(),
        UpdateStatus::Checking => "Checking for updates...".into(),
        UpdateStatus::UpToDate => "You have the latest version.".into(),
        UpdateStatus::Downloading(p) => format!("Downloading update... {p}%"),
        UpdateStatus::Ready(v) => format!("Version {v} is downloaded and ready. Restart to install."),
        UpdateStatus::Failed(e) => format!("Update check failed: {e}"),
    }
}

fn ping_text(snap: &Snapshot) -> String {
    match (snap.have_ping, snap.last_ping_ms) {
        (false, _) => "...".into(),
        (true, None) => "DROP".into(),
        (true, Some(ms)) => format!("{ms} ms"),
    }
}

fn summary_line(snap: &Snapshot) -> String {
    let (ssid, sig, ch) = match &snap.wifi {
        Some(w) => (w.ssid.clone(), format!("{}% {}dBm", w.signal_pct, w.rssi), format!("ch{}", w.channel)),
        None => ("no Wi-Fi".into(), String::new(), String::new()),
    };
    format!("{}  {}  {}  {}", ping_text(snap), ssid, sig, ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_handles_missing_wifi() {
        let s = Snapshot { have_ping: true, last_ping_ms: None, ..Snapshot::default() };
        assert!(summary_line(&s).starts_with("DROP  no Wi-Fi"));
    }

    #[test]
    fn hello_then_tick_builds_history_and_trims_to_window() {
        let mut d = Data {
            connected: false,
            settings: Settings { history_minutes: 1, ..Settings::default() },
            history: VecDeque::new(),
            snap: Snapshot::default(),
            notice: None,
        };
        let mut p = settings_panel::State::default();
        let hello_settings = d.settings.clone();
        apply_message(
            &mut d,
            &mut p,
            Message::Hello {
                settings: Box::new(hello_settings),
                history: vec![Sample { t: 0, ms: Some(1) }],
                snapshot: Box::new(Snapshot::default()),
            },
        );
        assert!(d.connected);
        apply_message(&mut d, &mut p, Message::Tick { sample: Sample { t: 120_000, ms: Some(2) }, snapshot: Box::new(Snapshot::default()) });
        assert_eq!(d.history.len(), 1, "the 2-minute-old sample falls outside a 1-minute window");
        assert_eq!(d.history[0].t, 120_000);
    }
}
