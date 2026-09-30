//! Automatic updates with Velopack.
//!
//! Direct installs (the Velopack `Setup.exe`) update themselves from the
//! configured source. Microsoft Store (MSIX) installs must NOT self-update: the
//! Store owns that, and Store policy forbids replacing the package from inside
//! the app. So when package identity is detected this module only reports
//! "managed by the Store" and never touches the network.

use crate::engine::Shared;
use crate::logging;
use crate::model::UpdateStatus;
use crate::platform;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;
use velopack::sources::AutoSource;
use velopack::{UpdateCheck, UpdateManager, VelopackAsset};

pub enum Cmd {
    CheckNow,
    ApplyNow,
}

pub fn start(shared: Arc<Shared>) -> Sender<Cmd> {
    let (tx, rx) = channel::<Cmd>();
    std::thread::Builder::new()
        .name("update".into())
        .spawn(move || {
            if platform::is_packaged() {
                shared.set_update_status(UpdateStatus::ManagedByStore);
                for _ in rx {} // swallow commands; nothing to do
                return;
            }
            run(shared, rx);
        })
        .ok();
    tx
}

fn manager(source: &str) -> Result<UpdateManager, String> {
    UpdateManager::new(AutoSource::new(source), None, None).map_err(|e| e.to_string())
}

fn run(shared: Arc<Shared>, rx: std::sync::mpsc::Receiver<Cmd>) {
    let mut pending: Option<VelopackAsset> = None;

    // A previous run may have downloaded an update that was never applied.
    if let Ok(um) = manager("") {
        if let Some(asset) = um.get_update_pending_restart() {
            shared.set_update_status(UpdateStatus::Ready(asset.Version.clone()));
            pending = Some(asset);
        } else {
            shared.set_update_status(UpdateStatus::Idle);
        }
    } else {
        shared.set_update_status(UpdateStatus::Unavailable);
        for cmd in rx {
            if let Cmd::CheckNow = cmd {
                shared.set_update_status(UpdateStatus::Failed(
                    "Updates need the installed version of the app (this is a portable or development build).".into(),
                ));
            }
        }
        return;
    }

    // Give the tray a moment to settle before the first network call.
    let mut wait = Duration::from_secs(20);
    let mut forced = false;
    loop {
        match rx.recv_timeout(wait) {
            Ok(Cmd::CheckNow) => forced = true,
            Ok(Cmd::ApplyNow) => {
                if let Some(asset) = &pending {
                    apply(&shared, asset);
                }
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let s = shared.settings();
        wait = Duration::from_secs(s.update_interval_hours as u64 * 3600);
        if shared.is_quitting() {
            return;
        }
        if !forced && !s.auto_update {
            continue;
        }
        forced = false;
        if s.update_source.is_empty() {
            shared.set_update_status(UpdateStatus::Failed("No update source is configured.".into()));
            continue;
        }
        if pending.is_some() {
            continue; // already downloaded; waiting on a restart
        }
        match check_and_download(&shared, &s.update_source) {
            Ok(Some(asset)) => {
                shared.set_update_status(UpdateStatus::Ready(asset.Version.clone()));
                logging::info(&format!("update {} downloaded", asset.Version));
                if s.auto_install_updates {
                    apply(&shared, &asset);
                }
                pending = Some(asset);
            }
            Ok(None) => shared.set_update_status(UpdateStatus::UpToDate),
            Err(e) => {
                logging::warn(&format!("update check failed: {e}"));
                shared.set_update_status(UpdateStatus::Failed(e));
            }
        }
    }
}

fn check_and_download(shared: &Arc<Shared>, source: &str) -> Result<Option<VelopackAsset>, String> {
    let um = manager(source)?;
    shared.set_update_status(UpdateStatus::Checking);
    match um.check_for_updates().map_err(|e| e.to_string())? {
        UpdateCheck::UpdateAvailable(info) => {
            let (ptx, prx) = channel::<i16>();
            let s2 = Arc::clone(shared);
            let watcher = std::thread::spawn(move || {
                for p in prx {
                    s2.set_update_status(UpdateStatus::Downloading(p.clamp(0, 100) as u8));
                }
            });
            let res = um.download_updates(&info, Some(ptx));
            let _ = watcher.join();
            res.map_err(|e| e.to_string())?;
            Ok(Some(info.TargetFullRelease.clone()))
        }
        UpdateCheck::NoUpdateAvailable | UpdateCheck::RemoteIsEmpty => Ok(None),
    }
}

fn apply(shared: &Arc<Shared>, asset: &VelopackAsset) {
    logging::info(&format!("applying update {} and restarting", asset.Version));
    let Ok(um) = manager("") else { return };
    // Launches the updater, which waits for this process to exit, swaps the
    // version in and restarts the app. We then shut down cleanly.
    match um.wait_exit_then_apply_updates(asset, true, true, Vec::<String>::new()) {
        Ok(()) => shared.request_quit(),
        Err(e) => {
            logging::error(&format!("apply update failed: {e}"));
            shared.set_update_status(UpdateStatus::Failed(e.to_string()));
        }
    }
}
