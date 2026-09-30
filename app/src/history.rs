//! Rolling ping history persisted as `unix_ms,latency_ms` lines (empty latency =
//! drop). A restart restores the chart window; the file is pruned to the window
//! once a minute so it never grows.

use crate::model::Sample;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn parse_line(line: &str) -> Option<Sample> {
    let (t, ms) = line.trim().split_once(',')?;
    let t: i64 = t.parse().ok()?;
    let ms = if ms.is_empty() { None } else { Some(ms.parse().ok()?) };
    Some(Sample { t, ms })
}

pub fn format_line(s: &Sample) -> String {
    match s.ms {
        Some(ms) => format!("{},{}\n", s.t, ms),
        None => format!("{},\n", s.t),
    }
}

/// Samples at or after `cutoff_ms`, oldest first, capped to the newest `max`.
pub fn load(path: &Path, cutoff_ms: i64, max: usize) -> Vec<Sample> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut out: Vec<Sample> = text
        .lines()
        .filter_map(parse_line)
        .filter(|s| s.t >= cutoff_ms)
        .collect();
    if out.len() > max {
        out.drain(..out.len() - max);
    }
    out
}

pub struct HistoryFile {
    path: PathBuf,
    file: Option<File>,
}

impl HistoryFile {
    pub fn new(path: PathBuf) -> Self {
        Self { path, file: None }
    }

    pub fn append(&mut self, s: &Sample) {
        if self.file.is_none() {
            self.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
        }
        if let Some(f) = self.file.as_mut() {
            let _ = f.write_all(format_line(s).as_bytes());
        }
    }

    /// Rewrite the file keeping only `keep` (already trimmed by the caller).
    pub fn rewrite(&mut self, keep: &[Sample]) {
        self.file = None; // release the append handle before replacing the file
        let tmp = self.path.with_extension("csv.tmp");
        let mut body = String::with_capacity(keep.len() * 20);
        for s in keep {
            body.push_str(&format_line(s));
        }
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip() {
        let a = Sample { t: 1_700_000_000_123, ms: Some(42) };
        let b = Sample { t: 1_700_000_001_123, ms: None };
        assert_eq!(parse_line(&format_line(&a)), Some(a));
        assert_eq!(parse_line(&format_line(&b)), Some(b));
        assert_eq!(parse_line("garbage"), None);
        assert_eq!(parse_line("12,abc"), None);
    }

    #[test]
    fn load_filters_by_age_and_caps_count() {
        let dir = std::env::temp_dir().join(format!("netmon-hist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.csv");
        let mut h = HistoryFile::new(path.clone());
        for i in 0..10 {
            h.append(&Sample { t: i * 1000, ms: Some(i as u32) });
        }
        let all = load(&path, 0, 100);
        assert_eq!(all.len(), 10);
        let recent = load(&path, 5000, 100);
        assert_eq!(recent.first().unwrap().t, 5000);
        let capped = load(&path, 0, 3);
        assert_eq!(capped.len(), 3);
        assert_eq!(capped.last().unwrap().t, 9000);

        h.rewrite(&recent);
        assert_eq!(load(&path, 0, 100).len(), recent.len());
        std::fs::remove_dir_all(dir).ok();
    }
}
