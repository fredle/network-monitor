// Release builds are GUI-subsystem: no console window behind the tray app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod engine;
mod events;
mod history;
mod icon;
mod ipc;
mod logging;
mod model;
mod paths;
mod ping;
mod platform;
mod roam;
mod settings;
mod timefmt;
mod tray;
mod ui;
mod update;
mod wifi;

use model::Request;
use settings::Settings;
use windows::core::HSTRING;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;

fn main() {
    // Velopack install/update/uninstall hooks. Must be first: during those events
    // the process does its bookkeeping and exits before anything else runs.
    velopack::VelopackApp::build()
        .set_app_user_model_id(paths::AUMID)
        .on_first_run(|_| update::FIRST_RUN.store(true, std::sync::atomic::Ordering::Relaxed))
        .run();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(i) = args.iter().position(|a| a == "--ui") {
        let kind = args.get(i + 1).map(String::as_str).unwrap_or("status");
        let anchor = args
            .iter()
            .position(|a| a == "--anchor")
            .and_then(|j| args.get(j + 1))
            .and_then(|s| s.split_once(','))
            .and_then(|(x, y)| Some((x.parse().ok()?, y.parse().ok()?)));
        platform::set_app_identity();
        std::process::exit(ui::run(kind, anchor));
    }
    run_daemon(args.iter().any(|a| a == "--background"));
}

fn run_daemon(background: bool) {
    platform::set_app_identity();

    // One daemon per user session. A second launch (Start menu, double-click)
    // just asks the running one to show its window.
    let _single = unsafe {
        let h = CreateMutexW(None, true, &HSTRING::from("Local\\NetworkMonitor.Daemon"));
        if matches!(&h, Ok(_)) && GetLastError() == ERROR_ALREADY_EXISTS {
            if !background {
                let _ = ipc::send_once(Request::ShowWindow("status".into()));
            }
            return;
        }
        h.ok()
    };

    logging::init(paths::log_dir());
    platform::register_toast_identity();

    // Start with Windows by default: register on the very first run only (no
    // settings file yet), so turning it off later is respected.
    if !paths::settings_file().exists() {
        if let Err(e) = platform::set_start_with_windows(true) {
            logging::warn(&format!("could not enable start with Windows: {e}"));
        }
    }
    let settings = Settings::load(&paths::settings_file());
    let shared = engine::Shared::new(settings);
    shared.start();
    let updater = update::start(std::sync::Arc::clone(&shared));

    if !background {
        let s = shared.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            if s.settings().notifications {
                platform::toast("Network Monitor", "Running in the notification area.");
            }
        });
    }

    tray::run(std::sync::Arc::clone(&shared), updater);

    shared.request_quit();
    logging::info("quit");
    // Give worker threads a moment to notice the flag and flush.
    std::thread::sleep(std::time::Duration::from_millis(300));
}
