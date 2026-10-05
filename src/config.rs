use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub server: ServerCfg,
    pub aoi: AoiCfg,
    pub simulation: SimCfg,
    pub sources: SourcesCfg,
    pub gfw: GfwCfg,
    pub fusion: FusionCfg,
    pub storage: StorageCfg,
    pub alerts: AlertsCfg,
    pub watchlist: WatchlistCfg,
}

impl Config {
    /// Load config.toml, writing the example file on first run if missing.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            std::fs::write(path, include_str!("../config.example.toml"))
                .with_context(|| format!("writing default config to {}", path.display()))?;
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    pub fn apply_overrides(
        &mut self,
        port: Option<u16>,
        host: Option<String>,
        sim: Option<bool>,
        aoi: Option<&str>,
    ) {
        // Precedence: config file < environment < explicit CLI arguments.
        // (The CLI used to lose to a stale OS_PORT in .env, which silently
        // ignored --port; environment values are applied first now.)
        if let Ok(v) = std::env::var("OS_PORT") {
            if let Ok(p) = v.parse() {
                self.server.port = p;
            }
        }
        if let Ok(v) = std::env::var("OS_HOST") {
            if !v.is_empty() {
                self.server.host = v;
            }
        }
        if let Ok(v) = std::env::var("OS_SIM") {
            let on = matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
            self.simulation.enabled = on;
        }
        if let Some(p) = port {
            self.server.port = p;
        }
        if let Some(h) = host {
            self.server.host = h;
        }
        if let Some(s) = sim {
            self.simulation.enabled = s;
        }
        if let Some(a) = aoi {
            let parts: Vec<f64> = a
                .split(',')
                .filter_map(|v| v.trim().parse::<f64>().ok())
                .collect();
            if parts.len() >= 2 {
                self.aoi.center_lat = parts[0];
                self.aoi.center_lon = parts[1];
            }
        }
        if let Ok(tok) = std::env::var("GFW_API_TOKEN") {
            if !tok.trim().is_empty() {
                self.gfw.token = Some(tok.trim().to_string());
            }
        }
        if let Ok(tok) = std::env::var("AISSTREAM_API_KEY") {
            if !tok.trim().is_empty() {
                self.sources.aisstream.api_key = Some(tok.trim().to_string());
            }
        }
        if let Ok(tok) = std::env::var("OS_API_TOKEN") {
            if !tok.trim().is_empty() {
                self.server.api_token = Some(tok.trim().to_string());
            }
        }
        if let Ok(url) = std::env::var("OS_ALERT_WEBHOOK") {
            if !url.trim().is_empty() {
                self.alerts.webhooks.push(url.trim().to_string());
            }
        }
        if let Ok(tok) = std::env::var("OS_TELEGRAM_BOT_TOKEN") {
            if !tok.trim().is_empty() {
                self.alerts.telegram_bot_token = Some(tok.trim().to_string());
            }
        }
        if let Ok(chat) = std::env::var("OS_TELEGRAM_CHAT_ID") {
            if !chat.trim().is_empty() {
                self.alerts.telegram_chat_id = Some(chat.trim().to_string());
            }
        }
    }
}

/// Where persistent state lives (zones, watchlist, alert log, track history).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StorageCfg {
    pub dir: String,
    /// Record vessel positions periodically so the map can be replayed.
    /// Costs disk: roughly (vessels x 130 bytes) per interval.
    pub record_tracks: bool,
    pub record_interval_s: u64,
    pub retention_days: u32,
}

impl Default for StorageCfg {
    fn default() -> Self {
        StorageCfg {
            dir: "data".to_string(),
            record_tracks: false,
            record_interval_s: 300,
            retention_days: 3,
        }
    }
}

/// Out-of-band alert delivery. Empty = alerts stay in the UI only.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AlertsCfg {
    /// Generic JSON webhooks: POSTs {text, content, alert}. Works with Slack,
    /// Discord, Teams and automation platforms (Zapier/n8n/Make) as-is.
    pub webhooks: Vec<String>,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    /// Lowest severity forwarded: info | medium | high
    pub min_severity: String,
    pub max_per_minute: u32,
}

impl Default for AlertsCfg {
    fn default() -> Self {
        AlertsCfg {
            webhooks: Vec::new(),
            telegram_bot_token: None,
            telegram_chat_id: None,
            min_severity: "medium".to_string(),
            max_per_minute: 30,
        }
    }
}

impl AlertsCfg {
    pub fn destinations(&self) -> Vec<String> {
        let mut out: Vec<String> = self.webhooks.clone();
        if self.telegram_bot_token.is_some() && self.telegram_chat_id.is_some() {
            out.push("telegram".to_string());
        }
        out
    }
}

/// Watchlist entries may live in config and/or be added from the UI (the UI
/// copy is persisted to `storage.dir/watchlist.json`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WatchlistCfg {
    pub entries: Vec<crate::model::WatchEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerCfg {
    pub host: String,
    pub port: u16,
    pub open_browser: bool,
    /// When set, every API and WebSocket request must present it
    /// (Authorization: Bearer <token>, or ?token= for the WebSocket).
    /// Required before binding to a non-loopback host.
    pub api_token: Option<String>,
}

impl Default for ServerCfg {
    fn default() -> Self {
        ServerCfg {
            host: "127.0.0.1".to_string(),
            port: 8787,
            open_browser: true,
            api_token: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AoiCfg {
    pub name: Option<String>,
    pub center_lat: f64,
    pub center_lon: f64,
    pub zoom: f64,
}

impl Default for AoiCfg {
    fn default() -> Self {
        AoiCfg {
            name: Some("Strait of Gibraltar".to_string()),
            center_lat: 36.02,
            center_lon: -5.36,
            zoom: 9.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SimCfg {
    pub enabled: bool,
    pub vessels: u32,
    pub dark_vessels: u32,
    pub interval_ms: u64,
    pub sonar_range_km: f64,
    pub lidar_range_km: f64,
}

impl Default for SimCfg {
    fn default() -> Self {
        SimCfg {
            enabled: true,
            vessels: 14,
            dark_vessels: 5,
            interval_ms: 1000,
            sonar_range_km: 16.0,
            lidar_range_km: 4.0,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SourcesCfg {
    pub ais: FeedCfg,
    pub sonar: FeedCfg,
    pub lidar: FeedCfg,
    pub aisstream: AisStreamCfg,
}

/// Global live AIS from AISStream.io (WebSocket). Free API key: https://aisstream.io
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AisStreamCfg {
    pub enabled: bool,
    pub url: String,
    /// Geographic filter as [[south, west], [north, east]]; the default is the
    /// whole globe. Narrow it to cut message volume.
    pub bounding_boxes: Vec<[[f64; 2]; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl Default for AisStreamCfg {
    fn default() -> Self {
        AisStreamCfg {
            enabled: false,
            url: "wss://stream.aisstream.io/v0/stream".to_string(),
            bounding_boxes: vec![[[-90.0, -180.0], [90.0, 180.0]]],
            api_key: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FeedCfg {
    pub enabled: bool,
    pub tcp: Vec<String>,
    pub udp: Vec<String>,
    pub talkers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GfwCfg {
    pub enabled: bool,
    pub base_url: String,
    pub token: Option<String>,
    pub cache_ttl_s: u64,
}

impl Default for GfwCfg {
    fn default() -> Self {
        GfwCfg {
            enabled: true,
            base_url: "https://gateway.api.globalfishingwatch.org".to_string(),
            token: None,
            cache_ttl_s: 21_600,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FusionCfg {
    pub gate_m: f64,
    pub dark_alert_after_s: i64,
    pub ais_lost_after_s: i64,
    pub stale_after_s: i64,
    pub drop_after_s: i64,
    pub alert_cooldown_s: i64,
    pub trail_points: usize,
    pub snapshot_trail: usize,
    pub tick_ms: u64,
    /// Hard cap on live tracks; a global AIS subscription can carry tens of
    /// thousands of vessels, so the least recently seen are evicted.
    pub max_tracks: usize,
    /// Raise a collision-risk alert when a TTM target reports a closest point
    /// of approach inside these limits.
    pub collision_cpa_m: f64,
    pub collision_tcpa_min: f64,
    /// Alert when a connected feed goes quiet for this many seconds.
    pub feed_stall_after_s: i64,
}

impl Default for FusionCfg {
    fn default() -> Self {
        FusionCfg {
            gate_m: 900.0,
            dark_alert_after_s: 25,
            ais_lost_after_s: 180,
            stale_after_s: 120,
            drop_after_s: 1800,
            alert_cooldown_s: 600,
            trail_points: 400,
            snapshot_trail: 90,
            tick_ms: 1000,
            max_tracks: 2500,
            collision_cpa_m: 500.0,
            collision_tcpa_min: 10.0,
            feed_stall_after_s: 90,
        }
    }
}
