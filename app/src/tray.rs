//! The tray daemon: owns the notification-area icon, its menu and the Win32
//! message loop, and launches UI windows as separate short-lived processes so the
//! always-running part stays tiny (no GPU/OpenGL stack is loaded here).

use crate::engine::Shared;
use crate::icon;
use crate::model::{Health, Request, Sample, Snapshot, UpdateStatus};
use crate::paths::APP_TITLE;
use crate::update;
use std::process::{Child, Command};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, DispatchMessageW, GetMessageW, KillTimer, PostQuitMessage, PostThreadMessageW,
    SetTimer, TranslateMessage, ASFW_ANY, MSG, WM_APP, WM_TIMER,
};

const WM_WAKE: u32 = WM_APP + 1;
const TIMER_ID: usize = 1;

/// Things other threads (pipe server, menu/tray callbacks) ask the main thread to do.
pub enum Action {
    Menu(MenuId),
    Tray(TrayIconEvent),
    Request(Request),
}

struct Ui {
    window: Option<Child>,
    popup: Option<Child>,
}

impl Ui {
    fn spawn(kind: &str, anchor: Option<(i32, i32)>) -> Option<Child> {
        unsafe {
            let _ = AllowSetForegroundWindow(ASFW_ANY);
        }
        let exe = std::env::current_exe().ok()?;
        let mut cmd = Command::new(exe);
        cmd.arg("--ui").arg(kind);
        if let Some((x, y)) = anchor {
            cmd.arg("--anchor").arg(format!("{x},{y}"));
        }
        cmd.spawn().ok()
    }

    fn show_window(&mut self, tab: &str) {
        // The UI process is single-instance per user; a second launch just focuses
        // the existing window, so spawning unconditionally is safe and simple.
        self.window = Self::spawn(tab, None);
    }

    /// Hovering the tray icon opens the mini window; the popup process closes
    /// itself once the cursor leaves both the icon and the popup.
    fn open_popup(&mut self, anchor: (i32, i32)) {
        if let Some(child) = self.popup.as_mut() {
            if matches!(child.try_wait(), Ok(None)) {
                return;
            }
        }
        self.popup = Self::spawn("popup", Some(anchor));
    }

    fn close_popup(&mut self) {
        if let Some(mut c) = self.popup.take() {
            let _ = c.kill();
        }
    }
}

pub fn run(shared: Arc<Shared>, updater: Sender<update::Cmd>) -> Receiver<Action> {
    // The pipe server and tray callbacks feed this channel; the loop below drains it.
    let (tx, rx) = channel::<Action>();
    let main_tid = unsafe { GetCurrentThreadId() };

    let wake = move || unsafe {
        let _ = PostThreadMessageW(main_tid, WM_WAKE, Default::default(), Default::default());
    };
    let tx_menu = Mutex::new(tx.clone());
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if let Ok(tx) = tx_menu.lock() {
            let _ = tx.send(Action::Menu(e.id));
        }
        wake();
    }));
    let tx_tray = Mutex::new(tx.clone());
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let Ok(tx) = tx_tray.lock() {
            let _ = tx.send(Action::Tray(e));
        }
        wake();
    }));

    // Pipe requests that need the main thread arrive through `hook`.
    let tx_hook = Mutex::new(tx);
    let hook: crate::ipc::UiHook = Arc::new(move |req| {
        if let Ok(tx) = tx_hook.lock() {
            let _ = tx.send(Action::Request(req));
        }
        wake();
    });
    if let Err(e) = crate::ipc::serve(Arc::clone(&shared), updater.clone(), hook) {
        crate::logging::error(&format!("ipc server failed: {e}"));
    }

    // ---- menu ----------------------------------------------------------------
    let menu = Menu::new();
    let m_status = MenuItem::new("Starting...", false, None);
    let m_show = MenuItem::new("Show window", true, None);
    let m_settings = MenuItem::new("Settings...", true, None);
    let m_reroam = MenuItem::new("Force re-roam now", true, None);
    let m_logs = MenuItem::new("Open log folder", true, None);
    let m_update = MenuItem::new("Check for updates", true, None);
    let m_quit = MenuItem::new("Quit", true, None);
    let _ = menu.append_items(&[
        &m_status,
        &PredefinedMenuItem::separator(),
        &m_show,
        &m_settings,
        &m_reroam,
        &m_logs,
        &PredefinedMenuItem::separator(),
        &m_update,
        &m_quit,
    ]);

    let tray = build_tray(menu);
    let Some(tray) = tray else {
        crate::logging::error("could not create the tray icon");
        return rx;
    };

    crate::platform::promote_tray_icon();

    let mut ui = Ui { window: None, popup: None };
    let mut shown_health = (Health::Idle, i64::MIN);
    let mut update_ready = false;

    unsafe {
        SetTimer(None, TIMER_ID, 1000, None);
    }
    refresh(&shared, &tray, &m_status, &m_update, &mut shown_health, &mut update_ready);

    let mut msg = MSG::default();
    loop {
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if got.0 <= 0 {
            break; // WM_QUIT or error
        }
        match msg.message {
            WM_TIMER => {
                refresh(&shared, &tray, &m_status, &m_update, &mut shown_health, &mut update_ready);
                if shared.is_quitting() {
                    unsafe { PostQuitMessage(0) };
                }
            }
            WM_WAKE => {
                while let Ok(action) = rx.try_recv() {
                    match action {
                        Action::Menu(id) => {
                            if id == *m_show.id() {
                                ui.show_window("status");
                            } else if id == *m_settings.id() {
                                ui.show_window("settings");
                            } else if id == *m_reroam.id() {
                                shared.request_reroam();
                            } else if id == *m_logs.id() {
                                let _ = Command::new("explorer.exe").arg(crate::paths::log_dir()).spawn();
                            } else if id == *m_update.id() {
                                let _ = updater.send(if update_ready { update::Cmd::ApplyNow } else { update::Cmd::CheckNow });
                            } else if id == *m_quit.id() {
                                shared.request_quit();
                                unsafe { PostQuitMessage(0) };
                            }
                        }
                        // tray-icon only re-arms `Enter` after a `Leave`, which it can miss, so Move counts too.
                        Action::Tray(TrayIconEvent::Enter { position, .. } | TrayIconEvent::Move { position, .. }) => {
                            ui.open_popup((position.x as i32, position.y as i32))
                        }
                        Action::Tray(TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        })
                        | Action::Tray(TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }) => {
                            ui.close_popup();
                            ui.show_window("status");
                        }
                        Action::Tray(_) => {}
                        Action::Request(Request::ShowWindow(kind)) => ui.show_window(&kind),
                        Action::Request(Request::Quit) => {
                            shared.request_quit();
                            unsafe { PostQuitMessage(0) };
                        }
                        Action::Request(_) => {}
                    }
                }
            }
            _ => unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            },
        }
    }

    unsafe {
        let _ = KillTimer(None, TIMER_ID);
    }
    ui.close_popup();
    drop(tray);
    rx
}

fn build_tray(menu: Menu) -> Option<TrayIcon> {
    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_icon(health_icon(&[], Health::Idle)?)
        .with_tooltip(APP_TITLE)
        .build()
        .ok()
}

fn health_icon(samples: &[Sample], h: Health) -> Option<Icon> {
    Icon::from_rgba(icon::sparkline_rgba(samples, h, 32), 32, 32).ok()
}

fn refresh(
    shared: &Arc<Shared>,
    tray: &TrayIcon,
    m_status: &MenuItem,
    m_update: &MenuItem,
    shown: &mut (Health, i64),
    update_ready: &mut bool,
) {
    let snap = shared.snapshot();
    let health = snap.health(&shared.settings());
    // Redraw the sparkline only when a new ping arrived or the health colour changed.
    let samples = shared.recent_samples(28);
    let key = (health, samples.last().map_or(0, |s| s.t));
    if key != *shown {
        if let Some(i) = health_icon(&samples, health) {
            let _ = tray.set_icon(Some(i));
            *shown = key;
        }
    }
    let _ = tray.set_tooltip(Some(tooltip(&snap)));
    m_status.set_text(status_line(&snap));

    let (text, ready) = update_menu_text(&snap.update);
    if ready != *update_ready {
        *update_ready = ready;
    }
    m_update.set_text(text);
    m_update.set_enabled(!matches!(snap.update, UpdateStatus::ManagedByStore | UpdateStatus::Checking | UpdateStatus::Downloading(_)));
}

fn update_menu_text(u: &UpdateStatus) -> (String, bool) {
    match u {
        UpdateStatus::Ready(v) => (format!("Restart to update to v{v}"), true),
        UpdateStatus::Checking => ("Checking for updates...".into(), false),
        UpdateStatus::Downloading(p) => (format!("Downloading update ({p}%)"), false),
        UpdateStatus::ManagedByStore => ("Updates: managed by Microsoft Store".into(), false),
        _ => ("Check for updates".into(), false),
    }
}

/// `NotifyIconData.szTip` holds 127 characters; stay under that.
pub fn tooltip(snap: &Snapshot) -> String {
    let ms = match (snap.have_ping, snap.last_ping_ms) {
        (false, _) => "...".to_string(),
        (true, None) => "DROP".to_string(),
        (true, Some(v)) => format!("{v}ms"),
    };
    let flag = if snap.reroaming {
        " ROAM".to_string()
    } else if snap.driver_events > 0 {
        format!(" !{}", snap.driver_events)
    } else {
        String::new()
    };
    let mut t = match &snap.wifi {
        Some(w) => {
            let tail = w.bssid.rsplit(':').take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(":");
            format!("{ms}  {}% {}dBm{flag}\n{tail}  ch{} {}/{}M", w.signal_pct, w.rssi, w.channel, w.rx_mbps, w.tx_mbps)
        }
        None => format!("{ms}  no Wi-Fi{flag}"),
    };
    if t.chars().count() > 120 {
        t = t.chars().take(120).collect();
    }
    t
}

fn status_line(snap: &Snapshot) -> String {
    let ms = match (snap.have_ping, snap.last_ping_ms) {
        (false, _) => "starting".to_string(),
        (true, None) => "DROP".to_string(),
        (true, Some(v)) => format!("{v}ms"),
    };
    match &snap.wifi {
        Some(w) => format!("{ms}   {}   {}%", w.bssid, w.signal_pct),
        None => format!("{ms}   not connected"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WifiInfo;

    #[test]
    fn tooltip_fits_the_shell_limit() {
        let snap = Snapshot {
            have_ping: true,
            last_ping_ms: Some(23),
            driver_events: 12,
            wifi: Some(WifiInfo {
                ssid: "x".into(),
                bssid: "02:00:5e:10:00:01".into(),
                band: "5 GHz".into(),
                channel: 149,
                signal_pct: 99,
                rssi: -51,
                rx_mbps: 866,
                tx_mbps: 866,
            }),
            ..Snapshot::default()
        };
        let t = tooltip(&snap);
        assert!(t.chars().count() <= 127);
        assert!(t.contains("23ms"));
        assert!(t.contains("10:00:01"));
        assert!(t.contains("!12"));
    }

    #[test]
    fn tooltip_handles_no_wifi_and_drops() {
        let snap = Snapshot { have_ping: true, last_ping_ms: None, ..Snapshot::default() };
        assert_eq!(tooltip(&snap), "DROP  no Wi-Fi");
    }
}
