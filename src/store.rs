use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::ais::AisBody;
use crate::config::{AoiCfg, FusionCfg};
use crate::geo;
use crate::model::*;

/// What the snapshot needs to know about the running app.
#[derive(Debug, Clone)]
pub struct SnapshotMeta {
    pub trail_limit: usize,
    pub version: String,
    pub aoi: AoiCfg,
    pub gfw_enabled: bool,
    pub gfw_token: bool,
    pub simulation: bool,
    pub port: u16,
    pub recording: bool,
    pub alert_destinations: Vec<String>,
    pub auth_required: bool,
}

pub struct Store {
    pub tracks: HashMap<String, Track>,
    pub mmsi_index: HashMap<u32, String>,
    pub alerts: VecDeque<Alert>,
    pub zones: Vec<Zone>,
    pub watchlist: Vec<WatchEntry>,
    pub feeds: HashMap<String, FeedStatus>,
    pub own_ship: Option<OwnShipFix>,
    pub gfw_events: Vec<GfwEvent>,
    pub started: DateTime<Utc>,
    pub trail_points: usize,
    pub alert_cap: usize,
    pub total_alerts: u64,
    pub dark_alerts: u64,
    pub paths: crate::persist::Paths,
}

impl Store {
    pub fn new(
        cfg: &FusionCfg,
        paths: crate::persist::Paths,
        watch_from_config: Vec<WatchEntry>,
    ) -> Self {
        let mut store = Store {
            tracks: HashMap::new(),
            mmsi_index: HashMap::new(),
            alerts: VecDeque::new(),
            zones: Vec::new(),
            watchlist: watch_from_config,
            feeds: HashMap::new(),
            own_ship: None,
            gfw_events: Vec::new(),
            started: Utc::now(),
            trail_points: cfg.trail_points,
            alert_cap: 500,
            total_alerts: 0,
            dark_alerts: 0,
            paths,
        };
        // reload anything saved by a previous run
        if let Some(zones) = crate::persist::load_json::<Vec<Zone>>(&store.paths.zones()) {
            if !zones.is_empty() {
                tracing::info!("restored {} saved geofence zones", zones.len());
            }
            store.zones = zones;
        }
        if let Some(saved) = crate::persist::load_json::<Vec<WatchEntry>>(&store.paths.watchlist())
        {
            for e in saved {
                if !store.watchlist.iter().any(|w| w.id == e.id) {
                    store.watchlist.push(e);
                }
            }
            tracing::info!("watchlist holds {} entries", store.watchlist.len());
        }
        // entries declared in config.toml may omit the id
        for e in store.watchlist.iter_mut() {
            if e.id.trim().is_empty() {
                e.id = format!(
                    "cfg-{}",
                    e.mmsi
                        .map(|m| m.to_string())
                        .or_else(|| e.imo.map(|i| format!("imo{i}")))
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()[..8].to_string())
                );
            }
        }
        // restore the tail of the alert log so the feed is not empty after a
        // restart (the full history stays on disk in alerts.jsonl)
        let lines = crate::persist::load_lines(&store.paths.alerts());
        let tail = lines.len().saturating_sub(120);
        for line in &lines[tail..] {
            if let Ok(a) = serde_json::from_str::<Alert>(line) {
                store.alerts.push_back(a);
                store.total_alerts += 1;
            }
        }
        if !store.alerts.is_empty() {
            tracing::info!("restored {} recent alerts from the log", store.alerts.len());
        }
        store
    }

    pub fn save_zones(&self) {
        if let Err(e) = crate::persist::save_json(&self.paths.zones(), &self.zones) {
            tracing::warn!("could not persist zones: {e}");
        }
    }

    pub fn save_watchlist(&self) {
        if let Err(e) = crate::persist::save_json(&self.paths.watchlist(), &self.watchlist) {
            tracing::warn!("could not persist watchlist: {e}");
        }
    }

    pub fn push_trail(&mut self, id: &str, lat: f64, lon: f64, ts: DateTime<Utc>) {
        let cap = self.trail_points;
        if let Some(t) = self.tracks.get_mut(id) {
            let add = match t.trail.last() {
                Some(p) => {
                    geo::haversine_m(p.lat, p.lon, lat, lon) > 25.0
                        || (ts - p.ts).num_seconds() >= 20
                }
                None => true,
            };
            if add {
                t.trail.push(TrailPoint { lat, lon, ts });
                if t.trail.len() > cap {
                    let excess = t.trail.len() - cap;
                    t.trail.drain(0..excess);
                }
            }
        }
    }

    pub fn nearest(
        &self,
        lat: f64,
        lon: f64,
        max_m: f64,
        pred: impl Fn(&Track) -> bool,
    ) -> Option<String> {
        let mut best: Option<(String, f64)> = None;
        for t in self.tracks.values() {
            if !pred(t) {
                continue;
            }
            let d = geo::haversine_m(lat, lon, t.lat, t.lon);
            if d <= max_m && best.as_ref().map(|(_, bd)| d < *bd).unwrap_or(true) {
                best = Some((t.id.clone(), d));
            }
        }
        best.map(|(id, _)| id)
    }

    /// Resolve (or create) the track that an AIS MMSI belongs to. If a
    /// sensor-only track already exists near the first known position, the
    /// identity is attached to it: that is the sonar/lidar <-> AIS fusion.
    pub fn ensure_mmsi_track(
        &mut self,
        mmsi: u32,
        lat: f64,
        lon: f64,
        ts: DateTime<Utc>,
        gate_m: f64,
    ) -> String {
        if let Some(id) = self.mmsi_index.get(&mmsi) {
            if self.tracks.contains_key(id) {
                return id.clone();
            }
        }
        let id = match self.nearest(lat, lon, gate_m, |t| !t.has_ais()) {
            Some(existing) => existing,
            None => {
                let id = format!("ais:{mmsi}");
                self.tracks
                    .entry(id.clone())
                    .or_insert_with(|| Track::new_ais(mmsi, lat, lon, ts));
                id
            }
        };
        if let Some(t) = self.tracks.get_mut(&id) {
            t.mmsi = Some(mmsi);
            if t.name.is_none() {
                // keep sensor label as a placeholder until AIS static arrives
            }
        }
        self.mmsi_index.insert(mmsi, id.clone());
        id
    }

    pub fn create_sensor_track(&mut self, c: &Contact, seq: &mut u64) -> String {
        *seq += 1;
        let short = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let id = format!("sensor:{short}");
        // sensor labels often carry a type prefix (F/V, MV, MT, TUG...), which
        // gives sensor-only contacts a meaningful icon
        let classification = c
            .label
            .as_deref()
            .map(classify_from_label)
            .unwrap_or("unknown");
        self.tracks
            .insert(id.clone(), Track::new_sensor(id.clone(), c, classification));
        id
    }

    /// Re-derive the naval/carrier flags and classification from whatever
    /// identity the track now holds. Returns true when this is the first time
    /// the track is known to be a carrier, so the caller can alert on it.
    pub fn classify_identity(&mut self, id: &str) -> bool {
        let Some(t) = self.tracks.get_mut(id) else {
            return false;
        };
        let name_hit = t.name.as_deref().map(is_carrier_name).unwrap_or(false);
        let naval_id = t.ship_type == Some(35)
            || t.name.as_deref().map(is_naval_name).unwrap_or(false)
            || name_hit;
        let was_carrier = t.carrier;
        // flags are sticky: a carrier or warship does not become a cargo ship
        // because a later message carried a bland type code
        t.carrier = t.carrier || name_hit;
        t.naval = t.naval || naval_id || t.carrier;
        if t.carrier {
            t.classification = "carrier".to_string();
        } else if t.naval && t.classification == "unknown" {
            t.classification = "military".to_string();
        }
        t.carrier && !was_carrier
    }

    pub fn apply_static_body(&mut self, body: &AisBody) {
        let mmsi = match body.mmsi() {
            Some(m) => m,
            None => return,
        };
        let id = self
            .mmsi_index
            .get(&mmsi)
            .cloned()
            .unwrap_or_else(|| format!("ais:{mmsi}"));
        if let Some(t) = self.tracks.get_mut(&id) {
            match body {
                AisBody::StaticA {
                    name,
                    callsign,
                    ship_type,
                    imo,
                    length,
                    beam,
                    destination,
                    draught,
                    ..
                } => {
                    if name.is_some() {
                        t.name = name.clone();
                    }
                    if callsign.is_some() {
                        t.callsign = callsign.clone();
                    }
                    if let Some(st) = ship_type {
                        t.ship_type = Some(*st);
                        t.classification = classify_ship_type(*st).to_string();
                    }
                    if imo.is_some() {
                        t.imo = *imo;
                    }
                    if length.is_some() {
                        t.length = *length;
                    }
                    if beam.is_some() {
                        t.beam = *beam;
                    }
                    if destination.is_some() {
                        t.destination = destination.clone();
                    }
                    if draught.is_some() {
                        t.draught = *draught;
                    }
                    t.confidence = (t.confidence + 0.1).min(1.0);
                }
                AisBody::StaticBName { name, .. } => {
                    if name.is_some() {
                        t.name = name.clone();
                    }
                }
                AisBody::StaticB {
                    ship_type,
                    callsign,
                    length,
                    beam,
                    ..
                } => {
                    if let Some(st) = ship_type {
                        t.ship_type = Some(*st);
                        t.classification = classify_ship_type(*st).to_string();
                    }
                    if callsign.is_some() {
                        t.callsign = callsign.clone();
                    }
                    if length.is_some() {
                        t.length = *length;
                    }
                    if beam.is_some() {
                        t.beam = *beam;
                    }
                }
                AisBody::ExtendedClassB {
                    name, ship_type, ..
                } => {
                    if name.is_some() {
                        t.name = name.clone();
                    }
                    if let Some(st) = ship_type {
                        t.ship_type = Some(*st);
                        t.classification = classify_ship_type(*st).to_string();
                    }
                }
                _ => {}
            }
            // identity may have just arrived (or changed): refresh the flags
            let _ = self.classify_identity(&id);
            if let Some(t) = self.tracks.get_mut(&id) {
                let want = crate::model::classify_with_name(t.name.as_deref(), t.ship_type);
                if !t.carrier && !t.naval {
                    t.classification = want.to_string();
                }
            }
        }
    }

    pub fn push_alert(&mut self, a: Alert) {
        if a.kind == "dark_contact" {
            self.dark_alerts += 1;
        }
        self.total_alerts += 1;
        self.alerts.push_back(a);
        while self.alerts.len() > self.alert_cap {
            self.alerts.pop_front();
        }
    }

    pub fn snapshot(&self, meta: &SnapshotMeta) -> Value {
        let now = Utc::now();
        let mut tracks: Vec<Value> = self
            .tracks
            .values()
            .map(|t| {
                let mut v = serde_json::to_value(t).unwrap_or(Value::Null);
                if let Some(arr) = v.get_mut("trail").and_then(|x| x.as_array_mut()) {
                    if arr.len() > meta.trail_limit {
                        let skip = arr.len() - meta.trail_limit;
                        arr.drain(0..skip);
                    }
                }
                v
            })
            .collect();
        tracks.sort_by(|a, b| {
            let ca = a.get("carrier").and_then(|v| v.as_bool()).unwrap_or(false);
            let cb = b.get("carrier").and_then(|v| v.as_bool()).unwrap_or(false);
            let na = a.get("naval").and_then(|v| v.as_bool()).unwrap_or(false);
            let nb = b.get("naval").and_then(|v| v.as_bool()).unwrap_or(false);
            let da = a.get("dark").and_then(|v| v.as_bool()).unwrap_or(false);
            let db = b.get("dark").and_then(|v| v.as_bool()).unwrap_or(false);
            cb.cmp(&ca)
                .then_with(|| nb.cmp(&na))
                .then_with(|| db.cmp(&da))
                .then_with(|| {
                    b.get("last_seen")
                        .and_then(|v| v.as_str())
                        .cmp(&a.get("last_seen").and_then(|v| v.as_str()))
                })
        });

        let mut alerts: Vec<Value> = self
            .alerts
            .iter()
            .rev()
            .take(150)
            .map(|a| serde_json::to_value(a).unwrap_or(Value::Null))
            .collect();
        alerts.truncate(150);

        let mut feeds: Vec<FeedStatus> = self.feeds.values().cloned().collect();
        feeds.sort_by(|a, b| a.name.cmp(&b.name));

        let ais_count = self.tracks.values().filter(|t| t.has_ais()).count();
        let dark_count = self.tracks.values().filter(|t| t.dark).count();
        let sensor_count = self
            .tracks
            .values()
            .filter(|t| !t.sensor_sources().is_empty())
            .count();
        let corroborated = self.tracks.values().filter(|t| t.corroborated).count();
        let naval = self.tracks.values().filter(|t| t.naval).count();
        let carriers = self.tracks.values().filter(|t| t.carrier).count();

        json!({
            "type": "state",
            "ts": now,
            "own_ship": self.own_ship,
            "tracks": tracks,
            "alerts": alerts,
            "zones": self.zones,
            "watchlist": self.watchlist,
            "feeds": feeds,
            "gfw_events": self.gfw_events,
            "stats": {
                "tracks": self.tracks.len(),
                "ais": ais_count,
                "dark": dark_count,
                "sensor_tracked": sensor_count,
                "corroborated": corroborated,
                "alerts": self.total_alerts,
                "dark_alerts": self.dark_alerts,
                "watchlist": self.watchlist.len(),
                "naval": naval,
                "carriers": carriers,
                "uptime_s": (now - self.started).num_seconds(),
            },
            "app": {
                "version": meta.version,
                "aoi": { "name": meta.aoi.name, "lat": meta.aoi.center_lat, "lon": meta.aoi.center_lon, "zoom": meta.aoi.zoom },
                "gfw_enabled": meta.gfw_enabled,
                "gfw_token": meta.gfw_token,
                "simulation": meta.simulation,
                "port": meta.port,
                "recording": meta.recording,
                "alert_destinations": meta.alert_destinations,
                "auth_required": meta.auth_required,
            }
        })
    }
}
