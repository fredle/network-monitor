//! Daily rolling text log in the per-user data folder, with retention.
//! Format matches the old script: `HH:mm:ss.fff LEVEL message`.

use crate::timefmt::{local_date_compact, local_hms_millis, now_ms};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

struct Sink {
    dir: PathBuf,
    day: String,
    file: Option<File>,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

pub fn init(dir: PathBuf) {
    if let Ok(mut g) = SINK.lock() {
        *g = Some(Sink { dir, day: String::new(), file: None });
    }
}

pub fn log(level: &str, msg: &str) {
    let now = now_ms();
    let line = format!("{} {:<5} {}\n", local_hms_millis(now), level, msg);
    let Ok(mut guard) = SINK.lock() else { return };
    let Some(sink) = guard.as_mut() else {
        eprint!("{line}");
        return;
    };
    let day = local_date_compact(now);
    if sink.day != day || sink.file.is_none() {
        sink.day = day.clone();
        sink.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(sink.dir.join(format!("netmon-{day}.log")))
            .ok();
    }
    if let Some(f) = sink.file.as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
}

pub fn info(msg: &str) {
    log("INFO", msg)
}
pub fn warn(msg: &str) {
    log("WARN", msg)
}
pub fn error(msg: &str) {
    log("ERR", msg)
}

/// Delete `netmon-YYYYMMDD.log` files older than `keep_days`.
pub fn purge_old(dir: &std::path::Path, keep_days: u32) {
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(keep_days as u64 * 86_400));
    let Some(cutoff) = cutoff else { return };
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("netmon-") && name.ends_with(".log")) {
            continue;
        }
        if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
            if modified < cutoff {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}
