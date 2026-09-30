//! Per-user data locations. Everything the app writes lives under
//! `%LOCALAPPDATA%\NetworkMonitor` (MSIX redirects this into the package's
//! private store automatically), never next to the executable.

use std::path::PathBuf;

pub const APP_ID: &str = "NetworkMonitor";
pub const AUMID: &str = "NetworkMonitor.TrayApp";
pub const APP_TITLE: &str = "Network Monitor";

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    // Velopack owns `%LOCALAPPDATA%\<packId>` (app binaries), so user data gets a
    // `Data` subfolder to keep the two from ever colliding.
    let dir = base.join(APP_ID).join("Data");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn log_dir() -> PathBuf {
    let dir = data_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn settings_file() -> PathBuf {
    data_dir().join("settings.json")
}

pub fn history_file() -> PathBuf {
    data_dir().join("ping-history.csv")
}
