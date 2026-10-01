//! User-editable settings, persisted as JSON. Every tunable in the app lives
//! here and is exposed in the Settings panel; nothing is configured by editing
//! files by hand.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Providers the legacy script watched. Only meaningful on Intel Wi-Fi adapters;
/// on other hardware the watcher simply never matches and can be switched off.
pub const DEFAULT_DRIVER_PROVIDERS: &str = "Netwtw06, Netwtw08, Netwtw10, Netwtw12, Netwtw14, Netwtw16";

/// Where updates are fetched from. Accepts a GitHub repo URL, any HTTP(S) folder
/// that holds Velopack release files, or a local folder. Empty disables updates.
pub const DEFAULT_UPDATE_SOURCE: &str = "https://github.com/fredle/network-monitor";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    // Ping
    pub target: String,
    pub ping_interval_ms: u32,
    pub ping_timeout_ms: u32,
    pub high_latency_ms: u32,
    pub weak_signal_dbm: i32,

    // Wi-Fi
    pub wifi_refresh_ms: u32,
    pub scan_interval_ms: u32,

    // Roaming
    pub auto_roam: bool,
    pub roam_threshold_db: u32,
    pub min_rssi_to_consider: i32,
    pub min_target_rssi: i32,
    pub roam_confirmations: u32,
    pub roam_cooldown_s: u32,
    pub max_roams_per_hour: u32,
    pub ssid_override: String,

    // Driver events
    pub watch_driver_events: bool,
    pub driver_providers: String,
    pub event_interval_ms: u32,

    // History and logs
    pub history_minutes: u32,
    pub history_max_samples: u32,
    pub log_retention_days: u32,

    // App
    pub notifications: bool,
    pub start_with_windows: bool,

    // Updates
    pub auto_update: bool,
    pub auto_install_updates: bool,
    pub update_interval_hours: u32,
    pub update_source: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            target: "8.8.8.8".into(),
            ping_interval_ms: 1000,
            ping_timeout_ms: 1500,
            high_latency_ms: 200,
            weak_signal_dbm: -80,

            wifi_refresh_ms: 5000,
            scan_interval_ms: 30_000,

            // Off by default: a roam attempt interrupts the link, and the
            // guard-rails below only matter once the user opts in.
            auto_roam: false,
            roam_threshold_db: 12,
            min_rssi_to_consider: -72,
            min_target_rssi: -67,
            roam_confirmations: 2,
            roam_cooldown_s: 120,
            max_roams_per_hour: 4,
            ssid_override: String::new(),

            watch_driver_events: true,
            driver_providers: DEFAULT_DRIVER_PROVIDERS.into(),
            event_interval_ms: 60_000,

            history_minutes: 10,
            history_max_samples: 3600,
            log_retention_days: 14,

            notifications: true,
            start_with_windows: true,

            auto_update: true,
            auto_install_updates: false,
            update_interval_hours: 6,
            update_source: DEFAULT_UPDATE_SOURCE.into(),
        }
    }
}

impl Settings {
    /// Clamp every field into a range the engine can safely run with. Applied on
    /// load and whenever the Settings panel submits a change, so a hand-edited or
    /// corrupt file can never produce a zero-length timer or a runaway loop.
    pub fn sanitized(mut self) -> Self {
        self.target = self.target.trim().to_string();
        if self.target.is_empty() {
            self.target = Settings::default().target;
        }
        self.ping_interval_ms = self.ping_interval_ms.clamp(250, 60_000);
        self.ping_timeout_ms = self.ping_timeout_ms.clamp(200, 10_000);
        self.high_latency_ms = self.high_latency_ms.clamp(10, 10_000);
        self.weak_signal_dbm = self.weak_signal_dbm.clamp(-100, -30);

        self.wifi_refresh_ms = self.wifi_refresh_ms.clamp(1000, 60_000);
        self.scan_interval_ms = self.scan_interval_ms.clamp(5000, 600_000);

        self.roam_threshold_db = self.roam_threshold_db.clamp(3, 40);
        self.min_rssi_to_consider = self.min_rssi_to_consider.clamp(-100, -30);
        self.min_target_rssi = self.min_target_rssi.clamp(-100, -30);
        self.roam_confirmations = self.roam_confirmations.clamp(1, 10);
        self.roam_cooldown_s = self.roam_cooldown_s.clamp(30, 3600);
        self.max_roams_per_hour = self.max_roams_per_hour.clamp(1, 60);
        self.ssid_override = self.ssid_override.trim().to_string();

        self.driver_providers = parse_providers(&self.driver_providers).join(", ");
        self.event_interval_ms = self.event_interval_ms.clamp(10_000, 3_600_000);

        self.history_minutes = self.history_minutes.clamp(1, 240);
        self.history_max_samples = self.history_max_samples.clamp(60, 50_000);
        self.log_retention_days = self.log_retention_days.clamp(1, 365);

        self.update_interval_hours = self.update_interval_hours.clamp(1, 168);
        self.update_source = self.update_source.trim().to_string();
        self
    }

    /// Parsed provider names, restricted to characters that are safe to splice
    /// into an XPath string literal.
    pub fn providers(&self) -> Vec<String> {
        parse_providers(&self.driver_providers)
    }

    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str::<Settings>(&text)
                .map(Settings::sanitized)
                .unwrap_or_default(),
            Err(_) => Settings::default(),
        }
    }

    /// Atomic save: write a sibling temp file, then rename over the target.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)
    }
}

/// Split on commas, semicolons and whitespace; drop duplicates and anything with
/// characters outside a conservative set.
fn parse_providers(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in raw.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
        if p.is_empty() || p.len() > 128 {
            continue;
        }
        let ok = p
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if ok && !out.iter().any(|e| e.eq_ignore_ascii_case(p)) {
            out.push(p.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_survive_sanitizing() {
        let d = Settings::default();
        assert_eq!(d.clone().sanitized(), d);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let s = Settings {
            ping_interval_ms: 0,
            scan_interval_ms: 1,
            history_minutes: 0,
            target: "   ".into(),
            roam_confirmations: 0,
            ..Settings::default()
        }
        .sanitized();
        assert_eq!(s.ping_interval_ms, 250);
        assert_eq!(s.scan_interval_ms, 5000);
        assert_eq!(s.history_minutes, 1);
        assert_eq!(s.target, "8.8.8.8");
        assert_eq!(s.roam_confirmations, 1);
    }

    #[test]
    fn partial_json_fills_in_defaults() {
        let s: Settings = serde_json::from_str(r#"{"target":"1.1.1.1"}"#).unwrap();
        assert_eq!(s.target, "1.1.1.1");
        assert_eq!(s.ping_interval_ms, 1000);
    }

    #[test]
    fn providers_are_split_deduped_and_xpath_safe() {
        let s = Settings {
            driver_providers: "Netwtw14, netwtw14;Evil'or;Good-One".into(),
            ..Settings::default()
        };
        assert_eq!(s.providers(), vec!["Netwtw14", "Good-One"]);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!("netmon-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("settings.json");
        let s = Settings { target: "9.9.9.9".into(), auto_roam: true, ..Settings::default() };
        s.save(&file).unwrap();
        assert_eq!(Settings::load(&file), s);
        std::fs::remove_dir_all(dir).ok();
    }
}
