//! The Settings tab. Every tunable in `Settings` is editable here; changes are
//! held as a draft until "Save", then sent to the daemon, which validates,
//! persists and applies them live.

use super::{update_text, Data};
use crate::model::Request;
use crate::settings::Settings;
use eframe::egui::{self, Color32, DragValue, RichText, TextEdit};

#[derive(Default)]
pub struct State {
    draft: Settings,
    loaded: bool,
    saved_flash: Option<std::time::Instant>,
}

impl State {
    /// First sync from the daemon (or a reconnect) replaces any draft.
    pub fn sync_from(&mut self, s: &Settings) {
        self.draft = s.clone();
        self.loaded = true;
    }

    /// The daemon confirmed a save; adopt its (possibly clamped) values.
    pub fn applied(&mut self, s: &Settings) {
        self.draft = s.clone();
        self.loaded = true;
        self.saved_flash = Some(std::time::Instant::now());
    }

    pub fn show(&mut self, ui: &mut egui::Ui, data: &Data) -> Option<Request> {
        if !self.loaded {
            ui.add_space(20.0);
            ui.label("Waiting for the monitor...");
            return None;
        }
        let mut request = None;
        let dirty = self.draft != data.settings;

        // Buttons live in a bottom bar so they stay visible while the form scrolls.
        egui::Panel::bottom("settings_buttons").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(dirty, egui::Button::new(RichText::new("Save").strong())).clicked() {
                    request = Some(Request::SetSettings(Box::new(self.draft.clone())));
                }
                if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
                    self.draft = data.settings.clone();
                }
                if ui.button("Reset to defaults").clicked() {
                    self.draft = Settings { start_with_windows: self.draft.start_with_windows, ..Settings::default() };
                }
                if dirty {
                    ui.colored_label(Color32::from_rgb(230, 160, 40), "Unsaved changes");
                } else if self.saved_flash.map_or(false, |t| t.elapsed().as_secs() < 4) {
                    ui.colored_label(Color32::from_rgb(90, 200, 120), "Saved");
                    ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
                }
            });
            ui.add_space(4.0);
        });

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let d = &mut self.draft;
            section(ui, "Ping", |ui| {
                row(ui, "Target host or IP", |ui| {
                    ui.add(TextEdit::singleline(&mut d.target).desired_width(220.0));
                });
                row(ui, "Ping every", |ui| ms(ui, &mut d.ping_interval_ms, 250..=60_000, 250.0));
                row(ui, "Timeout", |ui| ms(ui, &mut d.ping_timeout_ms, 200..=10_000, 100.0));
                row(ui, "Warn above (latency)", |ui| ms(ui, &mut d.high_latency_ms, 10..=10_000, 10.0));
                row(ui, "Warn at or below (signal)", |ui| dbm(ui, &mut d.weak_signal_dbm));
            });
            section(ui, "Wi-Fi", |ui| {
                row(ui, "Refresh link state every", |ui| ms(ui, &mut d.wifi_refresh_ms, 1000..=60_000, 250.0));
                row(ui, "Scan for access points every", |ui| ms(ui, &mut d.scan_interval_ms, 5000..=600_000, 1000.0));
            });
            let auto = d.auto_roam;
            section(ui, "Roaming", |ui| {
                row(ui, "Automatic roaming", |ui| {
                    ui.checkbox(&mut d.auto_roam, "Roam to a stronger access point automatically");
                });
                row_if(ui, auto, "Only when current signal is below", |ui| dbm(ui, &mut d.min_rssi_to_consider));
                row_if(ui, auto, "Target must be stronger by", |ui| {
                    ui.add(DragValue::new(&mut d.roam_threshold_db).range(3..=40).suffix(" dB"));
                });
                row_if(ui, auto, "Target must itself be at least", |ui| dbm(ui, &mut d.min_target_rssi));
                row_if(ui, auto, "Confirm over scans", |ui| {
                    ui.add(DragValue::new(&mut d.roam_confirmations).range(1..=10));
                });
                row_if(ui, auto, "Cooldown between roams", |ui| {
                    ui.add(DragValue::new(&mut d.roam_cooldown_s).range(30..=3600).suffix(" s"));
                });
                row_if(ui, auto, "Maximum roams per hour", |ui| {
                    ui.add(DragValue::new(&mut d.max_roams_per_hour).range(1..=60));
                });
                row_if(ui, auto, "Network (SSID) override", |ui| {
                    ui.add(TextEdit::singleline(&mut d.ssid_override).hint_text("blank = current network").desired_width(220.0));
                });
            });
            note(ui, "Roaming asks Windows to reconnect to a chosen access point. It never disables the adapter and needs no administrator rights.");
            let watch = d.watch_driver_events;
            section(ui, "Driver events", |ui| {
                row(ui, "Watch the system log", |ui| {
                    ui.checkbox(&mut d.watch_driver_events, "Report Wi-Fi driver errors");
                });
                row_if(ui, watch, "Driver providers", |ui| {
                    ui.add(TextEdit::singleline(&mut d.driver_providers).desired_width(320.0));
                });
                row_if(ui, watch, "Check every", |ui| ms(ui, &mut d.event_interval_ms, 10_000..=3_600_000, 1000.0));
            });
            note(ui, "The defaults match Intel Wi-Fi drivers (Netwtw*). On other adapters, list your driver's event provider names or turn this off.");
            section(ui, "History and logs", |ui| {
                row(ui, "Chart window", |ui| {
                    ui.add(DragValue::new(&mut d.history_minutes).range(1..=240).suffix(" min"));
                });
                row(ui, "Maximum samples kept", |ui| {
                    ui.add(DragValue::new(&mut d.history_max_samples).range(60..=50_000).speed(10.0));
                });
                row(ui, "Keep log files for", |ui| {
                    ui.add(DragValue::new(&mut d.log_retention_days).range(1..=365).suffix(" days"));
                });
            });
            section(ui, "General", |ui| {
                row(ui, "Notifications", |ui| {
                    ui.checkbox(&mut d.notifications, "Show a notification on driver faults and roams");
                });
                row(ui, "Startup", |ui| {
                    ui.checkbox(&mut d.start_with_windows, "Start Network Monitor when I sign in");
                });
            });
            let upd = d.auto_update;
            section(ui, "Updates", |ui| {
                row(ui, "Check automatically", |ui| {
                    ui.checkbox(&mut d.auto_update, "Download new versions in the background");
                });
                row_if(ui, upd, "Install automatically", |ui| {
                    ui.checkbox(&mut d.auto_install_updates, "Restart and install as soon as one is ready");
                });
                row_if(ui, upd, "Check every", |ui| {
                    ui.add(DragValue::new(&mut d.update_interval_hours).range(1..=168).suffix(" h"));
                });
                row(ui, "Update source", |ui| {
                    ui.add(
                        TextEdit::singleline(&mut d.update_source)
                            .hint_text("GitHub repo URL or release-feed folder")
                            .desired_width(320.0),
                    );
                });
            });
            note(ui, &update_text(&data.snap.update));
            ui.add_space(8.0);
        });
        request
    }
}

fn section(ui: &mut egui::Ui, title: &str, rows: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(8.0);
    ui.label(RichText::new(title).strong().size(15.0));
    ui.separator();
    egui::Grid::new(title).num_columns(2).spacing([16.0, 6.0]).show(ui, rows);
}

fn note(ui: &mut egui::Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(RichText::new(text).weak().small());
}

fn row(ui: &mut egui::Ui, label: &str, widget: impl FnOnce(&mut egui::Ui)) {
    ui.label(label);
    widget(ui);
    ui.end_row();
}

/// A row that is greyed out and inert unless `enabled`.
fn row_if(ui: &mut egui::Ui, enabled: bool, label: &str, widget: impl FnOnce(&mut egui::Ui)) {
    ui.add_enabled_ui(enabled, |ui| ui.label(label));
    ui.add_enabled_ui(enabled, widget);
    ui.end_row();
}

fn ms(ui: &mut egui::Ui, v: &mut u32, range: std::ops::RangeInclusive<u32>, speed: f64) {
    ui.add(DragValue::new(v).range(range).speed(speed).suffix(" ms"));
}

fn dbm(ui: &mut egui::Ui, v: &mut i32) {
    ui.add(DragValue::new(v).range(-100..=-30).suffix(" dBm"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applied_adopts_clamped_values_from_the_daemon() {
        let mut st = State::default();
        st.sync_from(&Settings::default());
        st.draft.ping_interval_ms = 1;
        let clamped = Settings { ping_interval_ms: 250, ..Settings::default() };
        st.applied(&clamped);
        assert_eq!(st.draft.ping_interval_ms, 250);
        assert!(st.saved_flash.is_some());
    }
}
