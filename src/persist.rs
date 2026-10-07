//! Durable state: zones, watchlist, alert log and optional track history.
//!
//! Deliberately file-based (atomic JSON writes, JSONL appends) so there is no
//! C dependency and the data stays inspectable with any text tool. The shapes
//! are small (tens of zones, hundreds of watchlist entries, an alert log that
//! rotates by size), so this holds up well; a SQLite layer would be the next
//! step only if heavy querying over history is needed.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::de::DeserializeOwned;
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
    pub history: PathBuf,
}

impl Paths {
    pub fn new(dir: &str) -> Self {
        let root = PathBuf::from(if dir.trim().is_empty() { "data" } else { dir });
        let history = root.join("history");
        if let Err(e) = fs::create_dir_all(&history) {
            tracing::warn!("could not create data directory {}: {e}", history.display());
        }
        // Project Icarus keeps its own corner: aircraft watchlist and archive,
        // separate from the maritime history so neither can corrupt the other.
        let icarus = root.join("icarus");
        if let Err(e) = fs::create_dir_all(icarus.join("history")) {
            tracing::warn!(
                "could not create aircraft data directory {}: {e}",
                icarus.display()
            );
        }
        Paths { root, history }
    }

    pub fn zones(&self) -> PathBuf {
        self.root.join("zones.json")
    }

    pub fn watchlist(&self) -> PathBuf {
        self.root.join("watchlist.json")
    }

    pub fn alerts(&self) -> PathBuf {
        self.root.join("alerts.jsonl")
    }

    pub fn history_day(&self, day: &str) -> PathBuf {
        self.history.join(format!("{day}.jsonl"))
    }

    /// Project Icarus (aerospace) data root.
    pub fn icarus_dir(&self) -> PathBuf {
        self.root.join("icarus")
    }

    pub fn icarus_watchlist(&self) -> PathBuf {
        self.icarus_dir().join("watchlist.json")
    }

    pub fn icarus_history_day(&self, day: &str) -> PathBuf {
        self.icarus_dir()
            .join("history")
            .join(format!("{day}.jsonl"))
    }
}

/// Day key ("2026-10-03") for a timestamp, used as the history file name.
pub fn day_key(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d").to_string()
}

pub fn load_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let text = fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("{} could not be parsed ({e}); ignoring it", path.display());
            None
        }
    }
}

/// Atomic write: temp file then rename, so a crash mid-write cannot corrupt state.
pub fn save_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(value).unwrap_or_else(|_| b"null".to_vec());
    fs::write(&tmp, body)?;
    fs::rename(&tmp, path)
}

/// Best-effort append; persistence must never take the app down.
pub fn append_line(path: &Path, line: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut f) => {
            let _ = writeln!(f, "{line}");
        }
        Err(e) => tracing::debug!("could not append to {}: {e}", path.display()),
    }
}

pub fn load_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Append a pre-built multi-line block in one open/write (history batches).
pub fn append_raw(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut f) => {
            let _ = f.write_all(body.as_bytes());
        }
        Err(e) => tracing::debug!("could not append to {}: {e}", path.display()),
    }
}

/// Delete history files older than the retention window.
pub fn prune_history(history: &Path, retention_days: u32) {
    let cutoff = Utc::now() - Duration::days(retention_days.max(1) as i64);
    let cutoff_key = day_key(cutoff);
    let Ok(entries) = fs::read_dir(history) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(day) = name.strip_suffix(".jsonl") {
            if day.len() == 10 && day < cutoff_key.as_str() {
                match fs::remove_file(e.path()) {
                    Ok(()) => tracing::info!("pruned old track history {name}"),
                    Err(err) => tracing::debug!("could not prune {name}: {err}"),
                }
            }
        }
    }
}

/// One recorded position sample (the history/replay record).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HistoryPoint {
    /// Recording instant; every point in one batch shares it.
    pub ts: DateTime<Utc>,
    pub id: String,
    pub mmsi: Option<u32>,
    pub name: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub sog: Option<f32>,
    pub cog: Option<f32>,
    #[serde(default)]
    pub dark: bool,
    pub class: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip_is_atomic_and_readable() {
        let dir = std::env::temp_dir().join(format!("os-persist-{}", uuid::Uuid::new_v4()));
        let paths = Paths::new(dir.to_str().unwrap());
        let zones = vec![serde_json::json!({"id": "z1", "name": "T"}).to_string()];
        save_json(&paths.zones(), &zones).unwrap();
        let back: Vec<String> = load_json(&paths.zones()).unwrap();
        assert_eq!(back, zones);
        assert!(
            !paths.zones().with_extension("json.tmp").exists(),
            "temp file should be renamed away"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appends_and_reads_lines() {
        let dir = std::env::temp_dir().join(format!("os-lines-{}", uuid::Uuid::new_v4()));
        let paths = Paths::new(dir.to_str().unwrap());
        append_line(&paths.alerts(), "{\"a\":1}");
        append_line(&paths.alerts(), "{\"a\":2}");
        let lines = load_lines(&paths.alerts());
        assert_eq!(lines.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prunes_only_files_older_than_retention() {
        let dir = std::env::temp_dir().join(format!("os-prune-{}", uuid::Uuid::new_v4()));
        let paths = Paths::new(dir.to_str().unwrap());
        let old = day_key(Utc::now() - Duration::days(30));
        let fresh = day_key(Utc::now());
        append_line(&paths.history_day(&old), "{}");
        append_line(&paths.history_day(&fresh), "{}");
        prune_history(&paths.history, 3);
        assert!(
            !paths.history_day(&old).exists(),
            "old file should be pruned"
        );
        assert!(
            paths.history_day(&fresh).exists(),
            "today's file must survive"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
