//! Windows integration that differs between a Velopack/portable install and a
//! Microsoft Store (MSIX) install: package identity, start-with-Windows, toasts.

use crate::paths::{APP_TITLE, AUMID};
use windows::core::HSTRING;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::Foundation::{APPMODEL_ERROR_NO_PACKAGE, ERROR_INSUFFICIENT_BUFFER};
use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

/// The task id declared in `AppxManifest.xml` (`windows.startupTask`).
pub const STARTUP_TASK_ID: &str = "NetworkMonitorStartup";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "NetworkMonitor";

/// True when running with MSIX package identity (installed from the Store).
/// Store builds must be updated by the Store and start through a StartupTask.
pub fn is_packaged() -> bool {
    unsafe {
        let mut len = 0u32;
        let rc = GetCurrentPackageFullName(&mut len, None);
        rc != APPMODEL_ERROR_NO_PACKAGE && (rc == ERROR_INSUFFICIENT_BUFFER || rc.0 == 0)
    }
}

/// Give the process its own taskbar/toast identity. Must run before any window.
pub fn set_app_identity() {
    unsafe {
        let _ = SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(AUMID));
    }
}

/// Unpackaged apps need a registry entry before Windows will show their toasts.
pub fn register_toast_identity() {
    if is_packaged() {
        return; // package identity already provides it
    }
    let key = format!(r"Software\Classes\AppUserModelId\{AUMID}");
    if let Ok(k) = windows_registry::CURRENT_USER.create(&key) {
        let _ = k.set_string("DisplayName", APP_TITLE);
    }
}

pub fn toast(title: &str, body: &str) {
    if let Err(e) = try_toast(title, body) {
        crate::logging::warn(&format!("toast failed: {e}"));
    }
}

fn try_toast(title: &str, body: &str) -> windows::core::Result<()> {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(title),
        xml_escape(body)
    );
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    let aumid = if is_packaged() { HSTRING::new() } else { HSTRING::from(AUMID) };
    let notifier = if aumid.is_empty() {
        ToastNotificationManager::CreateToastNotifier()?
    } else {
        ToastNotificationManager::CreateToastNotifierWithId(&aumid)?
    };
    notifier.Show(&toast)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Windows 11 parks new tray icons in the overflow flyout. Once the shell has
/// created this exe's NotifyIconSettings entry, set `IsPromoted` so the icon is
/// shown on the taskbar. Only done while the value is absent: if the user moves
/// the icon back, Windows writes `IsPromoted = 0` and we leave it alone.
pub fn promote_tray_icon() {
    if is_packaged() {
        return;
    }
    std::thread::spawn(|| {
        let Ok(exe) = std::env::current_exe() else { return };
        let exe = exe.to_string_lossy().to_lowercase();
        let base = r"Control Panel\NotifyIconSettings";
        // The shell registers the icon asynchronously; retry for a few seconds.
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let Ok(root) = windows_registry::CURRENT_USER.open(base) else { continue };
            let Ok(names) = root.keys() else { continue };
            for name in names {
                let Ok(k) = windows_registry::CURRENT_USER.create(format!(r"{base}\{name}")) else { continue };
                let path = k.get_string("ExecutablePath").unwrap_or_default().to_lowercase();
                if path == exe {
                    if k.get_u32("IsPromoted").is_err() {
                        let _ = k.set_u32("IsPromoted", 1);
                    }
                    return;
                }
            }
        }
    });
}

// ---- start with Windows -----------------------------------------------------

/// Enable/disable launching at sign-in. Store builds use the manifest's
/// StartupTask (the only mechanism the Store allows); everything else uses the
/// per-user Run key, which needs no elevation.
pub fn set_start_with_windows(enable: bool) -> Result<(), String> {
    if is_packaged() {
        return set_startup_task(enable);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let key = windows_registry::CURRENT_USER.create(RUN_KEY).map_err(|e| e.to_string())?;
    if enable {
        key.set_string(RUN_VALUE, &format!("\"{}\" --background", exe.display()))
            .map_err(|e| e.to_string())
    } else {
        match key.remove_value(RUN_VALUE) {
            Ok(()) => Ok(()),
            Err(_) => Ok(()), // already absent
        }
    }
}

pub fn start_with_windows_enabled() -> bool {
    if is_packaged() {
        return startup_task_enabled().unwrap_or(false);
    }
    windows_registry::CURRENT_USER
        .open(RUN_KEY)
        .and_then(|k| k.get_string(RUN_VALUE))
        .is_ok()
}

fn set_startup_task(enable: bool) -> Result<(), String> {
    use windows::ApplicationModel::{StartupTask, StartupTaskState};
    let task = StartupTask::GetAsync(&HSTRING::from(STARTUP_TASK_ID))
        .and_then(|op| op.join())
        .map_err(|e| e.to_string())?;
    if enable {
        let state = task.RequestEnableAsync().and_then(|op| op.join()).map_err(|e| e.to_string())?;
        if state == StartupTaskState::Enabled { Ok(()) } else { Err("Windows blocked the startup request; enable it in Settings > Apps > Startup".into()) }
    } else {
        task.Disable().map_err(|e| e.to_string())
    }
}

fn startup_task_enabled() -> Option<bool> {
    use windows::ApplicationModel::{StartupTask, StartupTaskState};
    let task = StartupTask::GetAsync(&HSTRING::from(STARTUP_TASK_ID)).ok()?.join().ok()?;
    Some(task.State().ok()? == StartupTaskState::Enabled)
}
