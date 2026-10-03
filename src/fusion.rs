//! Sensor fusion engine.
//!
//! Consumes decoded sensor events and maintains the live track picture:
//!  * AIS positions update the track keyed by MMSI.
//!  * Sonar / LiDAR contacts associate to the nearest track inside the gate,
//!    or spawn a new sensor-only track.
//!  * A sensor-only track older than `dark_alert_after_s` raises a dark
//!    contact alert (a vessel that is physically present but not on AIS).
//!  * AIS tracks that fall silent raise an AIS-lost alert.
//!  * Zone entry/exit transitions raise geofence alerts.
//! A full state snapshot is broadcast every tick for the live map.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::json;
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::debug;

use crate::ais::AisBody;
use crate::config::FusionCfg;
use crate::geo;
use crate::model::*;
use crate::sources::{Event, RelativeTarget};
use crate::store::{SnapshotMeta, Store};

struct Cand {
    key: String,
    kind: &'static str,
    severity: &'static str,
    message: String,
    track_id: String,
    lat: f64,
    lon: f64,
}

fn mk_alert(
    kind: &str,
    severity: &str,
    message: String,
    track_id: Option<String>,
    lat: f64,
    lon: f64,
) -> Alert {
    Alert {
        id: uuid::Uuid::new_v4().to_string(),
        ts: Utc::now(),
        kind: kind.to_string(),
        severity: severity.to_string(),
        message,
        track_id,
        lat,
        lon,
    }
}

fn cooldown_ok(
    cooldown: &mut HashMap<String, DateTime<Utc>>,
    cooldown_s: i64,
    key: &str,
    now: DateTime<Utc>,
) -> bool {
    if let Some(last) = cooldown.get(key) {
        if (now - *last).num_seconds() < cooldown_s {
            return false;
        }
    }
    cooldown.insert(key.to_string(), now);
    true
}

pub struct Fusion {
    store: Arc<RwLock<Store>>,
    bcast: broadcast::Sender<String>,
    cfg: FusionCfg,
    meta: SnapshotMeta,
    own: Option<OwnShipFix>,
    statics: HashMap<u32, AisBody>,
    cooldown: HashMap<String, DateTime<Utc>>,
    zone_occ: HashSet<(String, String)>,
    sensor_seq: u64,
    ticks: u64,
}

impl Fusion {
    pub fn new(
        store: Arc<RwLock<Store>>,
        bcast: broadcast::Sender<String>,
        cfg: FusionCfg,
        meta: SnapshotMeta,
    ) -> Self {
        Fusion {
            store,
            bcast,
            cfg,
            meta,
            own: None,
            statics: HashMap::new(),
            cooldown: HashMap::new(),
            zone_occ: HashSet::new(),
            sensor_seq: 0,
            ticks: 0,
        }
    }

    pub async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Event>) {
        let mut ticker = tokio::time::interval(Duration::from_millis(self.cfg.tick_ms.max(200)));
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Some(e) => self.handle(e).await,
                    None => break,
                },
                _ = ticker.tick() => self.tick().await,
            }
        }
    }

    async fn handle(&mut self, ev: Event) {
        match ev {
            Event::OwnShip(f) => {
                self.own = Some(f.clone());
                let mut st = self.store.write().await;
                st.own_ship = Some(f);
            }
            Event::Ais(body) => self.on_ais(body).await,
            Event::Contact(c) => self.on_contact(c).await,
            Event::RelativeTarget(rt) => self.on_relative(rt).await,
            Event::Feed(fs) => {
                let mut st = self.store.write().await;
                st.feeds.insert(fs.name.clone(), fs);
            }
        }
    }

    async fn flush_alerts(&self, alerts: Vec<Alert>) {
        if alerts.is_empty() {
            return;
        }
        let mut st = self.store.write().await;
        for a in &alerts {
            st.push_alert(a.clone());
            let msg = json!({"type": "alert", "alert": a}).to_string();
            let _ = self.bcast.send(msg);
        }
    }

    async fn on_ais(&mut self, body: AisBody) {
        let ts = Utc::now();
        let mmsi = match body.mmsi() {
            Some(m) => m,
            None => return,
        };
        let mut pending: Vec<Alert> = Vec::new();
        {
            let mut st = self.store.write().await;
            match &body {
                AisBody::Position {
                    lat: Some(lat),
                    lon: Some(lon),
                    sog,
                    cog,
                    heading,
                    nav_status,
                    ..
                } => {
                    let (lat, lon) = (*lat, *lon);
                    let id = st.ensure_mmsi_track(mmsi, lat, lon, ts, self.cfg.gate_m);
                    let mut newly_corroborated = false;
                    if let Some(t) = st.tracks.get_mut(&id) {
                        t.lat = lat;
                        t.lon = lon;
                        if sog.is_some() {
                            t.sog = *sog;
                        }
                        if cog.is_some() {
                            t.cog = *cog;
                        }
                        if heading.is_some() {
                            t.heading = *heading;
                        }
                        t.nav_status = Some(*nav_status);
                        let had_sensor = !t.sensor_sources().is_empty();
                        t.add_source(SensorKind::Ais);
                        t.dark = false;
                        t.last_seen = ts;
                        t.last_ais = Some(ts);
                        t.confidence = (t.confidence + 0.15).min(1.0);
                        if had_sensor && !t.corroborated {
                            t.corroborated = true;
                            newly_corroborated = true;
                        }
                    }
                    st.push_trail(&id, lat, lon, ts);
                    if let Some(s) = self.statics.get(&mmsi).cloned() {
                        st.apply_static_body(&s);
                    }
                    if newly_corroborated
                        && cooldown_ok(
                            &mut self.cooldown,
                            self.cfg.alert_cooldown_s,
                            &format!("corrob:{id}"),
                            ts,
                        )
                    {
                        let label = st
                            .tracks
                            .get(&id)
                            .and_then(|t| t.name.clone())
                            .unwrap_or_else(|| format!("MMSI {mmsi}"));
                        pending.push(mk_alert(
                            "sensor_corroborated",
                            "info",
                            format!("{label}: AIS position confirmed by onboard sensor contact"),
                            Some(id.clone()),
                            lat,
                            lon,
                        ));
                    }
                }
                _ => {
                    st.apply_static_body(&body);
                    self.statics.insert(mmsi, body.clone());
                }
            }
        }
        self.flush_alerts(pending).await;
    }

    async fn on_contact(&mut self, c: Contact) {
        let mut pending: Vec<Alert> = Vec::new();
        {
            let mut st = self.store.write().await;
            let id = if let Some(mmsi) = c.mmsi {
                st.ensure_mmsi_track(mmsi, c.lat, c.lon, c.ts, self.cfg.gate_m)
            } else {
                match st.nearest(c.lat, c.lon, self.cfg.gate_m, |t| !t.has_ais()) {
                    Some(id) => id,
                    None => match st.nearest(c.lat, c.lon, 400.0, |_| true) {
                        Some(id) => id,
                        None => st.create_sensor_track(&c, &mut self.sensor_seq),
                    },
                }
            };
            let mut newly_corroborated = false;
            if let Some(t) = st.tracks.get_mut(&id) {
                let age = (c.ts - t.last_seen).num_seconds();
                if t.has_ais() {
                    // AIS position is authoritative: a sensor hit confirms presence
                    if !t.corroborated && geo::haversine_m(t.lat, t.lon, c.lat, c.lon) < 1500.0 {
                        t.corroborated = true;
                        newly_corroborated = true;
                    }
                } else {
                    let w = if age > 60 {
                        1.0
                    } else {
                        0.6 * (c.confidence as f64).clamp(0.2, 1.0)
                    };
                    t.lat += (c.lat - t.lat) * w;
                    t.lon += (c.lon - t.lon) * w;
                    // Trackers that report target motion (NMEA TTM) give these
                    // directly: a dark contact gets speed and course too.
                    if c.sog_kn.is_some() {
                        t.sog = c.sog_kn;
                    }
                    if c.cog_deg.is_some() {
                        t.cog = c.cog_deg;
                        t.heading = None;
                    }
                }
                t.add_source(c.source);
                t.last_sensor_contact = Some(c.ts);
                t.last_seen = c.ts;
                t.dark = !t.has_ais();
                t.confidence = (t.confidence + 0.05).min(1.0);
                if t.name.is_none() {
                    if let Some(lbl) = &c.label {
                        t.name = Some(lbl.clone());
                    }
                }
            }
            st.push_trail(&id, c.lat, c.lon, c.ts);
            debug!(
                source = c.source.as_str(),
                label = c.label.as_deref().unwrap_or("-"),
                lat = c.lat,
                lon = c.lon,
                track = %id,
                "sensor contact associated"
            );
            if newly_corroborated
                && cooldown_ok(
                    &mut self.cooldown,
                    self.cfg.alert_cooldown_s,
                    &format!("corrob:{id}"),
                    c.ts,
                )
            {
                let label = st
                    .tracks
                    .get(&id)
                    .and_then(|t| t.name.clone())
                    .unwrap_or_else(|| id.clone());
                pending.push(mk_alert(
                    "sensor_corroborated",
                    "info",
                    format!(
                        "{label}: AIS position confirmed by {} contact",
                        c.source.as_str()
                    ),
                    Some(id.clone()),
                    c.lat,
                    c.lon,
                ));
            }
        }
        self.flush_alerts(pending).await;
    }

    async fn on_relative(&mut self, rt: RelativeTarget) {
        let own = match self.own.clone() {
            Some(o) => o,
            None => return,
        };
        let brg = if rt.true_bearing {
            rt.bearing_deg
        } else {
            rt.bearing_deg + own.heading.unwrap_or(0.0) as f64
        };
        let (lat, lon) = geo::destination_point(own.lat, own.lon, brg, rt.dist_m);
        let c = Contact {
            source: rt.source,
            lat,
            lon,
            mmsi: None,
            label: rt.name.clone().or_else(|| Some(rt.target.clone())),
            range_m: Some(rt.dist_m),
            bearing_deg: Some(rt.bearing_deg),
            sog_kn: rt.speed_kn.map(|v| v as f32),
            cog_deg: rt.course_deg.map(|v| v as f32),
            confidence: 0.75,
            ts: rt.ts,
        };
        self.on_contact(c).await;
    }

    async fn tick(&mut self) {
        let now = Utc::now();
        let mut pending: Vec<Alert> = Vec::new();
        let mut occupancy: HashSet<(String, String)> = HashSet::new();
        {
            let mut st = self.store.write().await;

            // 1. forget tracks that have been silent too long
            let drop_after = self.cfg.drop_after_s;
            let dead: Vec<String> = st
                .tracks
                .iter()
                .filter(|(_, t)| (now - t.last_seen).num_seconds() >= drop_after)
                .map(|(id, _)| id.clone())
                .collect();
            for id in dead {
                st.tracks.remove(&id);
            }
            let live: HashSet<String> = st.tracks.keys().cloned().collect();
            st.mmsi_index.retain(|_, v| live.contains(v));

            // 1b. enforce the track cap (a global AIS feed can carry tens of
            // thousands of vessels): evict the least recently seen.
            let max_tracks = self.cfg.max_tracks.max(50);
            if st.tracks.len() > max_tracks {
                let mut ages: Vec<(String, i64)> = st
                    .tracks
                    .iter()
                    .map(|(id, t)| (id.clone(), (now - t.last_seen).num_seconds()))
                    .collect();
                ages.sort_by(|a, b| b.1.cmp(&a.1));
                for (id, _) in ages.iter().take(st.tracks.len() - max_tracks) {
                    st.tracks.remove(id);
                }
                let live: HashSet<String> = st.tracks.keys().cloned().collect();
                st.mmsi_index.retain(|_, v| live.contains(v));
            }

            // 2. derive alerts from track state
            let dark_after = self.cfg.dark_alert_after_s;
            let ais_lost_after = self.cfg.ais_lost_after_s;
            let stale_after = self.cfg.stale_after_s;
            let cooldown_s = self.cfg.alert_cooldown_s;
            let mut cands: Vec<Cand> = Vec::new();
            for t in st.tracks.values() {
                let since_seen = (now - t.last_seen).num_seconds();
                let age = (now - t.first_seen).num_seconds();
                let ais_age = t
                    .last_ais
                    .map(|a| (now - a).num_seconds())
                    .unwrap_or(i64::MAX);
                let label = t
                    .name
                    .clone()
                    .unwrap_or_else(|| match t.mmsi {
                        Some(m) => format!("MMSI {m}"),
                        None => t.id.clone(),
                    });
                if t.dark && age >= dark_after && since_seen < stale_after {
                    let sensors: Vec<&str> =
                        t.sensor_sources().iter().map(|s| s.as_str()).collect();
                    cands.push(Cand {
                        key: format!("dark:{}", t.id),
                        kind: "dark_contact",
                        severity: "high",
                        message: format!(
                            "DARK CONTACT {label}: tracked by {} only, no AIS — {:.4}, {:.4}",
                            sensors.join("+"),
                            t.lat,
                            t.lon
                        ),
                        track_id: t.id.clone(),
                        lat: t.lat,
                        lon: t.lon,
                    });
                }
                if t.has_ais() && ais_age >= ais_lost_after {
                    // A track still held by a physical sensor while its AIS is
                    // silent is the strongest dark-vessel signal there is, so it
                    // escalates rather than being suppressed.
                    let sensor_fresh = t
                        .last_sensor_contact
                        .map(|s| (now - s).num_seconds() < stale_after)
                        .unwrap_or(false);
                    let sensors: Vec<&str> =
                        t.sensor_sources().iter().map(|s| s.as_str()).collect();
                    let (severity, message, variant) = if sensor_fresh && !sensors.is_empty() {
                        (
                            "high",
                            format!(
                                "AIS LOST {label}: AIS silent for {ais_age}s but STILL VISIBLE on {} — possible AIS-off at {:.4}, {:.4}",
                                sensors.join("+"),
                                t.lat,
                                t.lon
                            ),
                            "visible",
                        )
                    } else {
                        (
                            "medium",
                            format!(
                                "AIS LOST {label}: AIS silent for {ais_age}s, no current sensor contact — last seen {:.4}, {:.4}",
                                t.lat, t.lon
                            ),
                            "blind",
                        )
                    };
                    cands.push(Cand {
                        key: format!("aislost:{}:{variant}", t.id),
                        kind: "ais_lost",
                        severity,
                        message,
                        track_id: t.id.clone(),
                        lat: t.lat,
                        lon: t.lon,
                    });
                }
            }
            for c in cands {
                if cooldown_ok(&mut self.cooldown, cooldown_s, &c.key, now) {
                    pending.push(mk_alert(
                        c.kind,
                        c.severity,
                        c.message,
                        Some(c.track_id),
                        c.lat,
                        c.lon,
                    ));
                }
            }

            // 3. geofence zones
            if !st.zones.is_empty() {
                let zones = st.zones.clone();
                let positions: Vec<(String, f64, f64)> = st
                    .tracks
                    .values()
                    .map(|t| (t.id.clone(), t.lat, t.lon))
                    .collect();
                for z in &zones {
                    for (id, lat, lon) in &positions {
                        if geo::point_in_poly(*lat, *lon, &z.polygon) {
                            occupancy.insert((z.id.clone(), id.clone()));
                            if !self.zone_occ.contains(&(z.id.clone(), id.clone()))
                                && cooldown_ok(
                                    &mut self.cooldown,
                                    cooldown_s,
                                    &format!("zone:{}:{}", z.id, id),
                                    now,
                                )
                            {
                                let label = st
                                    .tracks
                                    .get(id)
                                    .and_then(|t| t.name.clone())
                                    .unwrap_or_else(|| id.clone());
                                pending.push(mk_alert(
                                    "zone_entry",
                                    "medium",
                                    format!("{label} entered zone \"{}\"", z.name),
                                    Some(id.clone()),
                                    *lat,
                                    *lon,
                                ));
                            }
                        }
                    }
                }
            }

            // 4. broadcast the state snapshot (trails shrink and the cadence
            // slows as the picture grows, so a worldwide view stays workable)
            let n = st.tracks.len();
            self.meta.trail_limit = if n > 1500 {
                0
            } else if n > 400 {
                20
            } else {
                self.cfg.snapshot_trail
            };
            self.ticks = self.ticks.wrapping_add(1);
            let stride = if n > 1500 {
                3
            } else if n > 600 {
                2
            } else {
                1
            };
            if self.ticks % stride == 0 {
                let snap = st.snapshot(&self.meta).to_string();
                let _ = self.bcast.send(snap);
            }
        }
        self.zone_occ = occupancy;
        self.flush_alerts(pending).await;
    }
}
