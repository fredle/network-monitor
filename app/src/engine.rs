//! The monitoring core. Runs on plain threads (no UI, no GPU) and owns all state;
//! the tray and the UI processes only ever read snapshots or send requests.

use crate::events;
use crate::history::{self, HistoryFile};
use crate::logging;
use crate::model::{Message, Sample, Snapshot, UpdateStatus, WifiInfo};
use crate::paths;
use crate::ping::Pinger;
use crate::platform;
use crate::roam::{self, Governor};
use crate::settings::Settings;
use crate::timefmt::now_ms;
use crate::wifi::{self, BssEntry, Interface, Link, WifiClient};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const CLIENT_RETRY: Duration = Duration::from_secs(30);
const PRUNE_EVERY: Duration = Duration::from_secs(60);
const PURGE_LOGS_EVERY: Duration = Duration::from_secs(6 * 3600);
const ROAM_SETTLE_TIMEOUT: Duration = Duration::from_secs(25);

struct Inner {
    settings: Settings,
    snap: Snapshot,
    history: VecDeque<Sample>,
    link: Option<Link>,
    bss: Vec<BssEntry>,
    iface: Option<Interface>,
    governor: Governor,
    events_since_ms: i64,
    client: Option<Arc<WifiClient>>,
}

pub struct Shared {
    inner: Mutex<Inner>,
    subs: Mutex<Vec<Sender<Message>>>,
    quit: AtomicBool,
}

impl Shared {
    pub fn new(mut settings: Settings) -> Arc<Self> {
        // The OS is the source of truth for launch-at-sign-in (the user can also
        // toggle it from Windows Settings > Startup).
        settings.start_with_windows = platform::start_with_windows_enabled();
        let snap = Snapshot { version: env!("CARGO_PKG_VERSION").to_string(), ..Snapshot::default() };
        Arc::new(Self {
            inner: Mutex::new(Inner {
                settings,
                snap,
                history: VecDeque::new(),
                link: None,
                bss: Vec::new(),
                iface: None,
                governor: Governor::default(),
                events_since_ms: now_ms(),
                client: None,
            }),
            subs: Mutex::new(Vec::new()),
            quit: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---- public accessors ---------------------------------------------------

    pub fn settings(&self) -> Settings {
        self.lock().settings.clone()
    }

    pub fn snapshot(&self) -> Snapshot {
        self.lock().snap.clone()
    }

    pub fn is_quitting(&self) -> bool {
        self.quit.load(Ordering::Relaxed)
    }

    pub fn request_quit(&self) {
        self.quit.store(true, Ordering::Relaxed);
    }

    pub fn hello(&self) -> Message {
        let g = self.lock();
        Message::Hello {
            settings: Box::new(g.settings.clone()),
            history: g.history.iter().copied().collect(),
            snapshot: Box::new(g.snap.clone()),
        }
    }

    /// Register a connection's outgoing channel for live updates.
    pub fn subscribe(&self, tx: Sender<Message>) {
        if let Ok(mut s) = self.subs.lock() {
            s.push(tx);
        }
    }

    pub fn broadcast(&self, msg: Message) {
        if let Ok(mut s) = self.subs.lock() {
            s.retain(|tx| tx.send(msg.clone()).is_ok());
        }
    }

    fn broadcast_state(&self) {
        let snap = self.snapshot();
        self.broadcast(Message::State(Box::new(snap)));
    }

    // ---- settings -------------------------------------------------------------

    /// Validate, persist and apply new settings. Returns the values actually in
    /// force (after clamping).
    pub fn apply_settings(self: &Arc<Self>, new: Settings) -> Settings {
        let new = new.sanitized();
        let (old_startup, cutoff_changed) = {
            let mut g = self.lock();
            let old = g.settings.start_with_windows;
            let changed = g.settings.history_minutes != new.history_minutes
                || g.settings.history_max_samples != new.history_max_samples;
            g.settings = new.clone();
            (old, changed)
        };
        if let Err(e) = new.save(&paths::settings_file()) {
            logging::error(&format!("saving settings failed: {e}"));
        }
        if cutoff_changed {
            self.trim_history(now_ms());
        }
        if new.start_with_windows != old_startup {
            let me = Arc::clone(self);
            let enable = new.start_with_windows;
            std::thread::spawn(move || match platform::set_start_with_windows(enable) {
                Ok(()) => logging::info(&format!("start with Windows: {}", if enable { "on" } else { "off" })),
                Err(e) => {
                    logging::error(&format!("start with Windows failed: {e}"));
                    me.lock().settings.start_with_windows = platform::start_with_windows_enabled();
                    me.broadcast(Message::Notice(format!("Could not change start-with-Windows: {e}")));
                }
            });
        }
        logging::info("settings updated");
        self.broadcast(Message::SettingsApplied(Box::new(new.clone())));
        new
    }

    pub fn set_update_status(&self, status: UpdateStatus) {
        {
            let mut g = self.lock();
            if g.snap.update == status {
                return;
            }
            g.snap.update = status;
        }
        self.broadcast_state();
    }

    fn notify(&self, title: &str, body: &str) {
        if self.lock().settings.notifications {
            let (t, b) = (title.to_string(), body.to_string());
            std::thread::spawn(move || platform::toast(&t, &b));
        }
    }

    fn trim_history(&self, now: i64) {
        let mut g = self.lock();
        let cutoff = now - g.settings.history_minutes as i64 * 60_000;
        let max = g.settings.history_max_samples as usize;
        while g.history.front().map_or(false, |s| s.t < cutoff) {
            g.history.pop_front();
        }
        while g.history.len() > max {
            g.history.pop_front();
        }
    }

    // ---- threads ----------------------------------------------------------------

    pub fn start(self: &Arc<Self>) {
        let s = self.settings();
        let cutoff = now_ms() - s.history_minutes as i64 * 60_000;
        let restored = history::load(&paths::history_file(), cutoff, s.history_max_samples as usize);
        if !restored.is_empty() {
            logging::info(&format!("restored {} ping samples from previous run", restored.len()));
            self.lock().history = restored.into();
        }
        logging::info(&format!("started v{} target={}", env!("CARGO_PKG_VERSION"), s.target));

        let me = Arc::clone(self);
        std::thread::Builder::new().name("ping".into()).spawn(move || me.ping_loop()).ok();
        let me = Arc::clone(self);
        std::thread::Builder::new().name("wifi".into()).spawn(move || me.housekeeping_loop()).ok();
    }

    fn sleep_unless_quit(&self, total: Duration) {
        let end = Instant::now() + total;
        while !self.is_quitting() {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(Duration::from_millis(200)));
        }
    }

    fn ping_loop(self: Arc<Self>) {
        let Some(mut pinger) = Pinger::new() else {
            logging::error("could not open an ICMP handle; ping disabled");
            return;
        };
        let mut file = HistoryFile::new(paths::history_file());
        let mut last_prune = Instant::now();
        while !self.is_quitting() {
            let started = Instant::now();
            let s = self.settings();
            let ms = pinger.ping(&s.target, s.ping_timeout_ms);
            let sample = Sample { t: now_ms(), ms };
            let snap = {
                let mut g = self.lock();
                g.snap.have_ping = true;
                g.snap.last_ping_ms = ms;
                g.snap.total += 1;
                match ms {
                    None => g.snap.drops += 1,
                    Some(v) if v > g.settings.high_latency_ms => g.snap.high_latency += 1,
                    _ => {}
                }
                g.history.push_back(sample);
                g.snap.clone()
            };
            if ms.is_none() {
                logging::warn(&format!("ping DROP -> {}", s.target));
            }
            file.append(&sample);
            self.trim_history(sample.t);
            if last_prune.elapsed() >= PRUNE_EVERY {
                last_prune = Instant::now();
                let keep: Vec<Sample> = self.lock().history.iter().copied().collect();
                file.rewrite(&keep);
            }
            self.broadcast(Message::Tick { sample, snapshot: Box::new(snap) });

            let interval = Duration::from_millis(s.ping_interval_ms as u64);
            self.sleep_unless_quit(interval.saturating_sub(started.elapsed()).max(Duration::from_millis(50)));
        }
    }

    fn housekeeping_loop(self: Arc<Self>) {
        let mut next_wifi = Instant::now();
        let mut next_scan = Instant::now() + Duration::from_secs(5);
        let mut next_events = Instant::now() + Duration::from_secs(10);
        let mut next_purge = Instant::now();
        let mut next_client_try = Instant::now();
        while !self.is_quitting() {
            let now = Instant::now();
            let s = self.settings();

            if self.lock().client.is_none() && now >= next_client_try {
                next_client_try = now + CLIENT_RETRY;
                match WifiClient::open() {
                    Ok(c) => {
                        self.lock().client = Some(Arc::new(c));
                        logging::info("Wi-Fi service connected");
                    }
                    Err(code) => logging::warn(&format!("Wi-Fi service unavailable: {}", wifi::describe_error(code))),
                }
            }
            if now >= next_wifi {
                next_wifi = now + Duration::from_millis(s.wifi_refresh_ms as u64);
                self.wifi_tick();
            }
            if now >= next_scan {
                next_scan = now + Duration::from_millis(s.scan_interval_ms as u64);
                self.scan_tick(&s);
            }
            if now >= next_events {
                next_events = now + Duration::from_millis(s.event_interval_ms as u64);
                if s.watch_driver_events {
                    self.events_tick(&s);
                }
            }
            if now >= next_purge {
                next_purge = now + PURGE_LOGS_EVERY;
                logging::purge_old(&paths::log_dir(), s.log_retention_days);
            }
            self.sleep_unless_quit(Duration::from_millis(250));
        }
    }

    // ---- Wi-Fi state --------------------------------------------------------------

    fn wifi_tick(&self) {
        let Some(client) = self.lock().client.clone() else { return };
        let iface = client.primary_interface();
        let (bss, link) = match &iface {
            Some(i) => {
                let bss = client.bss_list(&i.guid);
                let link = client.link(&i.guid, &bss);
                // The current AP is missing from the cached scan results (first run,
                // or a fresh association): ask for a scan so band/channel/dBm fill in.
                if link.as_ref().map_or(false, |l| !bss.iter().any(|b| b.bssid == l.bssid)) {
                    client.scan(&i.guid);
                }
                (bss, link)
            }
            None => (Vec::new(), None),
        };

        let changed = {
            let mut g = self.lock();
            let prev = g.snap.wifi.clone();
            let new_info: Option<WifiInfo> = link.as_ref().map(|l| l.info.clone());
            match (&prev, &new_info) {
                (Some(p), None) => {
                    logging::warn(&format!("Wi-Fi link lost (was {})", p.bssid));
                    g.snap.link_loss += 1;
                }
                (Some(p), Some(n)) if p.bssid != n.bssid => logging::info(&format!(
                    "BSSID changed: {} -> {} ({} ch {} {}% {} dBm)",
                    p.bssid, n.bssid, n.band, n.channel, n.signal_pct, n.rssi
                )),
                (None, Some(n)) => logging::info(&format!(
                    "link up: {} {} ch {} {}% {} dBm",
                    n.bssid, n.band, n.channel, n.signal_pct, n.rssi
                )),
                _ => {}
            }
            let adapter = iface.as_ref().map(|i| i.description.clone());
            if g.snap.adapter != adapter {
                if let Some(a) = &adapter {
                    logging::info(&format!("adapter: {a}"));
                }
                g.snap.adapter = adapter;
            }
            g.snap.wifi = new_info.clone();
            g.link = link;
            g.bss = bss;
            g.iface = iface;
            prev != new_info
        };
        if changed {
            self.broadcast_state();
        }
    }

    fn scan_tick(self: &Arc<Self>, s: &Settings) {
        let (client, iface, link, bss, busy) = {
            let g = self.lock();
            (g.client.clone(), g.iface.clone(), g.link.clone(), g.bss.clone(), g.snap.reroaming)
        };
        let (Some(client), Some(iface)) = (client, iface) else { return };
        client.scan(&iface.guid);
        if !s.auto_roam || busy {
            return;
        }
        let Some(link) = link else { return };
        let ssid = if s.ssid_override.is_empty() { link.info.ssid.clone() } else { s.ssid_override.clone() };
        let candidate = roam::pick_candidate(&link.bssid, link.info.rssi, &ssid, &bss, s);
        let go = self.lock().governor.should_roam(candidate.as_ref(), Instant::now(), s);
        if go {
            if let Some(c) = candidate {
                logging::info(&format!(
                    "roam candidate: current={} ({} dBm) best={} ({} {} dBm)",
                    link.info.bssid,
                    link.info.rssi,
                    wifi::format_bssid(&c.bssid),
                    wifi::band_and_channel(c.freq_khz).0,
                    c.rssi
                ));
                let me = Arc::clone(self);
                std::thread::spawn(move || me.perform_roam(Some(c), false));
            }
        }
    }

    // ---- roaming ------------------------------------------------------------------

    /// Manual "force re-roam" from the tray or the UI.
    pub fn request_reroam(self: &Arc<Self>) {
        let (link, bss, cooldown, busy) = {
            let g = self.lock();
            (
                g.link.clone(),
                g.bss.clone(),
                g.governor.cooldown_remaining(Instant::now(), &g.settings),
                g.snap.reroaming,
            )
        };
        if busy {
            self.notify("Re-roam", "A re-roam is already in progress.");
            return;
        }
        if let Some(left) = cooldown {
            self.notify("Re-roam", &format!("Cooldown active - try again in {}s.", left.as_secs() + 1));
            return;
        }
        let Some(link) = link else {
            self.notify("Re-roam", "Not connected to Wi-Fi.");
            return;
        };
        // Prefer the strongest other AP on this network if it beats the current one;
        // otherwise just reconnect and let Windows choose.
        let target = roam::strongest_other(&link.bssid, &link.info.ssid, &bss).filter(|b| b.rssi > link.info.rssi);
        let me = Arc::clone(self);
        std::thread::spawn(move || me.perform_roam(target, true));
    }

    fn perform_roam(self: Arc<Self>, target: Option<BssEntry>, manual: bool) {
        let (client, iface, link) = {
            let mut g = self.lock();
            if g.snap.reroaming {
                return;
            }
            let ready = g.client.clone().zip(g.iface.clone()).zip(g.link.clone());
            let Some(((client, iface), link)) = ready else { return };
            g.snap.reroaming = true;
            (client, iface, link)
        };
        self.broadcast_state();
        let kind = if manual { "manual" } else { "auto" };
        let started = Instant::now();
        let result = match &target {
            Some(t) => {
                logging::info(&format!("re-roam start ({kind}) -> {}", wifi::format_bssid(&t.bssid)));
                client.connect_bssid(&iface.guid, &link.profile, &link.ssid_raw, t.bssid)
            }
            None => {
                logging::info(&format!("re-roam start ({kind}) -> reconnect"));
                client.reconnect(&iface.guid, &link.profile)
            }
        };

        let mut landed: Option<Link> = None;
        if result.is_ok() {
            while started.elapsed() < ROAM_SETTLE_TIMEOUT && !self.is_quitting() {
                std::thread::sleep(Duration::from_secs(1));
                let bss = client.bss_list(&iface.guid);
                if let Some(l) = client.link(&iface.guid, &bss) {
                    if started.elapsed() >= Duration::from_secs(2) {
                        landed = Some(l);
                        break;
                    }
                }
            }
        }

        {
            let mut g = self.lock();
            g.snap.reroaming = false;
            if result.is_ok() {
                g.snap.roams += 1;
                g.governor.record_roam(Instant::now());
            }
        }
        self.broadcast_state();

        match (result, landed) {
            (Err(code), _) => {
                logging::error(&format!("re-roam failed: {}", wifi::describe_error(code)));
                self.notify("Re-roam failed", &wifi::describe_error(code));
            }
            (Ok(()), Some(l)) => {
                logging::info(&format!(
                    "re-roam finished in {:.1}s: now {} {} {} dBm",
                    started.elapsed().as_secs_f32(),
                    l.info.bssid,
                    l.info.band,
                    l.info.rssi
                ));
                if manual {
                    self.notify("Re-roam complete", &format!("Connected to {} ({} dBm)", l.info.bssid, l.info.rssi));
                }
            }
            (Ok(()), None) => {
                logging::warn("re-roam: link did not come back within the timeout");
                self.notify("Re-roam", "The connection has not come back yet.");
            }
        }
    }

    // ---- driver events --------------------------------------------------------------

    fn events_tick(&self, s: &Settings) {
        let since = self.lock().events_since_ms;
        let now = now_ms();
        match events::query_since(&s.providers(), since) {
            Ok(list) => {
                self.lock().events_since_ms = now;
                if list.is_empty() {
                    return;
                }
                for e in &list {
                    let detail: String = e.detail.chars().take(160).collect();
                    logging::warn(&format!("DRIVER {} Id={} {} {}", e.level_name(), e.id, e.provider, detail));
                }
                let total = {
                    let mut g = self.lock();
                    g.snap.driver_events += list.len() as u32;
                    g.snap.driver_events
                };
                self.notify(
                    "Wi-Fi driver fault",
                    &format!("{} new driver event(s). Total since start: {}", list.len(), total),
                );
                self.broadcast_state();
            }
            Err(e) => logging::error(&format!("event log query failed: {e}")),
        }
    }
}
