//! Data shared between the tray daemon and the UI process. Everything here is
//! serialisable because it crosses the named pipe as JSON lines.

use crate::settings::Settings;
use serde::{Deserialize, Serialize};

/// One ping result. `ms == None` is a drop. `t` is unix milliseconds.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub t: i64,
    pub ms: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct WifiInfo {
    pub ssid: String,
    pub bssid: String,
    pub band: String,
    pub channel: u32,
    pub signal_pct: u32,
    pub rssi: i32,
    pub rx_mbps: u32,
    pub tx_mbps: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub enum UpdateStatus {
    /// Running from a build that cannot self-update (dev build or portable).
    #[default]
    Unavailable,
    /// Store/MSIX builds are updated by the Microsoft Store, never by the app.
    ManagedByStore,
    Idle,
    Checking,
    UpToDate,
    Downloading(u8),
    /// A newer version is downloaded and waiting for a restart.
    Ready(String),
    Failed(String),
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub last_ping_ms: Option<u32>,
    pub have_ping: bool,
    pub total: u64,
    pub drops: u64,
    pub high_latency: u64,
    pub wifi: Option<WifiInfo>,
    pub link_loss: u32,
    pub driver_events: u32,
    pub reroaming: bool,
    pub roams: u32,
    pub adapter: Option<String>,
    pub update: UpdateStatus,
    pub version: String,
}

/// Coarse health used for the tray icon colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Health {
    Idle,
    Good,
    Warn,
    Bad,
    Roaming,
}

impl Snapshot {
    pub fn health(&self, s: &Settings) -> Health {
        if self.reroaming {
            return Health::Roaming;
        }
        if !self.have_ping {
            return Health::Idle;
        }
        match self.last_ping_ms {
            None => Health::Bad,
            Some(ms) => {
                let weak = self.wifi.as_ref().map_or(false, |w| w.rssi <= s.weak_signal_dbm);
                if ms > s.high_latency_ms || weak {
                    Health::Warn
                } else {
                    Health::Good
                }
            }
        }
    }
}

// ---- IPC protocol ---------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Request {
    /// Start streaming. The server answers with `Hello`, then a `Tick` per sample.
    Subscribe,
    SetSettings(Box<Settings>),
    Reroam,
    CheckUpdate,
    ApplyUpdate,
    OpenLogFolder,
    /// Ask the daemon to open a UI window (`status`, `settings`, `popup`).
    ShowWindow(String),
    Quit,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Message {
    Hello { settings: Box<Settings>, history: Vec<Sample>, snapshot: Box<Snapshot> },
    Tick { sample: Sample, snapshot: Box<Snapshot> },
    /// Non-ping state change (wifi, update, roam) pushed between ticks.
    State(Box<Snapshot>),
    SettingsApplied(Box<Settings>),
    Notice(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_follows_ping_and_signal() {
        let s = Settings::default();
        let mut snap = Snapshot::default();
        assert_eq!(snap.health(&s), Health::Idle);
        snap.have_ping = true;
        snap.last_ping_ms = Some(20);
        assert_eq!(snap.health(&s), Health::Good);
        snap.last_ping_ms = Some(500);
        assert_eq!(snap.health(&s), Health::Warn);
        snap.last_ping_ms = None;
        assert_eq!(snap.health(&s), Health::Bad);
        snap.reroaming = true;
        assert_eq!(snap.health(&s), Health::Roaming);
    }

    #[test]
    fn protocol_round_trips_as_json() {
        let m = Message::Tick { sample: Sample { t: 5, ms: None }, snapshot: Box::new(Snapshot::default()) };
        let line = serde_json::to_string(&m).unwrap();
        assert!(!line.contains('\n'));
        let back: Message = serde_json::from_str(&line).unwrap();
        assert!(matches!(back, Message::Tick { .. }));
    }
}
