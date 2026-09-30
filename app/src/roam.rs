//! When to roam. Pure decision logic, kept free of Windows calls so it can be
//! tested exhaustively.
//!
//! The legacy script's auto-roam flapped: it chased a distant, weak 5 GHz
//! satellite as the "strongest" AP, landed on a bad link, then roamed again,
//! and each attempt bounced the adapter for 15-20 s. The guards here exist to
//! make that impossible:
//!   * the target must itself be a usable signal (`min_target_rssi`),
//!   * it must beat the current AP by `roam_threshold_db`,
//!   * the same target must win `roam_confirmations` scans in a row,
//!   * a cooldown and an hourly cap bound the damage of any remaining mistake.

use crate::settings::Settings;
use crate::wifi::BssEntry;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Best other access point for `ssid`, or `None` when staying put is right.
pub fn pick_candidate(
    current_bssid: &[u8; 6],
    current_rssi: i32,
    ssid: &str,
    bss: &[BssEntry],
    s: &Settings,
) -> Option<BssEntry> {
    if current_rssi >= s.min_rssi_to_consider {
        return None;
    }
    let best = strongest_other(current_bssid, ssid, bss)?;
    if best.rssi < s.min_target_rssi {
        return None;
    }
    if best.rssi - current_rssi < s.roam_threshold_db as i32 {
        return None;
    }
    Some(best)
}

/// Strongest BSS on `ssid` that is not the one we are on.
pub fn strongest_other(current_bssid: &[u8; 6], ssid: &str, bss: &[BssEntry]) -> Option<BssEntry> {
    bss.iter()
        .filter(|b| b.ssid == ssid && &b.bssid != current_bssid)
        .max_by_key(|b| b.rssi)
        .cloned()
}

#[derive(Default)]
pub struct Governor {
    pending: Option<([u8; 6], u32)>,
    recent: VecDeque<Instant>,
    last: Option<Instant>,
}

impl Governor {
    /// Feed the outcome of one scan. Returns true when an auto-roam to `candidate`
    /// should go ahead now.
    pub fn should_roam(&mut self, candidate: Option<&BssEntry>, now: Instant, s: &Settings) -> bool {
        let Some(c) = candidate else {
            self.pending = None; // a scan with no winner breaks the streak
            return false;
        };
        let streak = match self.pending {
            Some((b, n)) if b == c.bssid => n + 1,
            _ => 1,
        };
        self.pending = Some((c.bssid, streak));
        if streak < s.roam_confirmations {
            return false;
        }
        if let Some(last) = self.last {
            if now.duration_since(last) < Duration::from_secs(s.roam_cooldown_s as u64) {
                return false;
            }
        }
        self.prune(now);
        if self.recent.len() as u32 >= s.max_roams_per_hour {
            return false;
        }
        true
    }

    pub fn record_roam(&mut self, now: Instant) {
        self.last = Some(now);
        self.recent.push_back(now);
        self.pending = None;
        self.prune(now);
    }

    /// Cooldown check used by the manual path (which ignores confirmations/caps).
    pub fn cooldown_remaining(&self, now: Instant, s: &Settings) -> Option<Duration> {
        let last = self.last?;
        let wait = Duration::from_secs(s.roam_cooldown_s.min(30) as u64);
        let since = now.duration_since(last);
        if since < wait { Some(wait - since) } else { None }
    }

    fn prune(&mut self, now: Instant) {
        while let Some(front) = self.recent.front() {
            if now.duration_since(*front) > Duration::from_secs(3600) {
                self.recent.pop_front();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bss(id: u8, ssid: &str, rssi: i32) -> BssEntry {
        BssEntry { bssid: [0, 0, 0, 0, 0, id], ssid: ssid.into(), rssi, link_quality: 0, freq_khz: 0 }
    }
    const CUR: [u8; 6] = [0, 0, 0, 0, 0, 1];

    fn settings() -> Settings {
        Settings { auto_roam: true, ..Settings::default() } // -72 / 12 dB / -67 target
    }

    #[test]
    fn picks_a_clearly_better_ap() {
        let list = [bss(1, "home", -80), bss(2, "home", -55)];
        let c = pick_candidate(&CUR, -80, "home", &list, &settings()).unwrap();
        assert_eq!(c.bssid[5], 2);
    }

    #[test]
    fn stays_when_current_signal_is_fine() {
        let list = [bss(1, "home", -50), bss(2, "home", -30)];
        assert!(pick_candidate(&CUR, -50, "home", &list, &settings()).is_none());
    }

    #[test]
    fn ignores_other_ssids() {
        let list = [bss(1, "home", -80), bss(2, "neighbour", -40)];
        assert!(pick_candidate(&CUR, -80, "home", &list, &settings()).is_none());
    }

    #[test]
    fn requires_the_full_threshold() {
        let list = [bss(1, "home", -78), bss(2, "home", -68)]; // only 10 dB better
        assert!(pick_candidate(&CUR, -78, "home", &list, &settings()).is_none());
    }

    #[test]
    fn will_not_chase_a_weak_target() {
        // The legacy failure: current is poor, "better" target is itself barely usable.
        let list = [bss(1, "home", -95), bss(2, "home", -82)];
        assert!(pick_candidate(&CUR, -95, "home", &list, &settings()).is_none());
    }

    #[test]
    fn needs_consecutive_confirmations() {
        let mut g = Governor::default();
        let s = settings(); // 2 confirmations
        let t0 = Instant::now();
        let c = bss(2, "home", -55);
        assert!(!g.should_roam(Some(&c), t0, &s));
        assert!(g.should_roam(Some(&c), t0, &s));
    }

    #[test]
    fn a_different_winner_or_a_miss_resets_the_streak() {
        let mut g = Governor::default();
        let s = settings();
        let t0 = Instant::now();
        assert!(!g.should_roam(Some(&bss(2, "home", -55)), t0, &s));
        assert!(!g.should_roam(Some(&bss(3, "home", -55)), t0, &s)); // new winner
        assert!(!g.should_roam(None, t0, &s)); // scan with no winner
        assert!(!g.should_roam(Some(&bss(3, "home", -55)), t0, &s)); // streak restarted
    }

    #[test]
    fn cooldown_blocks_back_to_back_roams() {
        let mut g = Governor::default();
        let s = Settings { roam_confirmations: 1, ..settings() };
        let t0 = Instant::now();
        let c = bss(2, "home", -55);
        assert!(g.should_roam(Some(&c), t0, &s));
        g.record_roam(t0);
        assert!(!g.should_roam(Some(&c), t0 + Duration::from_secs(60), &s));
        assert!(g.should_roam(Some(&c), t0 + Duration::from_secs(121), &s));
    }

    #[test]
    fn hourly_cap_is_enforced_and_expires() {
        let mut g = Governor::default();
        let s = Settings { roam_confirmations: 1, roam_cooldown_s: 30, max_roams_per_hour: 2, ..settings() };
        let t0 = Instant::now();
        let c = bss(2, "home", -55);
        g.record_roam(t0);
        g.record_roam(t0 + Duration::from_secs(100));
        assert!(!g.should_roam(Some(&c), t0 + Duration::from_secs(200), &s));
        assert!(g.should_roam(Some(&c), t0 + Duration::from_secs(3700), &s));
    }
}
