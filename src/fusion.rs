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
//!
//! A full state snapshot is broadcast every tick for the live map.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::debug;

use crate::ais::AisBody;
use crate::config::{FusionCfg, StorageCfg};
use crate::geo;
use crate::model::*;
use crate::persist;
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

/// Long-lived handles the fusion engine needs.
pub struct FusionDeps {
    pub store: Arc<RwLock<Store>>,
    pub bcast: broadcast::Sender<crate::server::ServerMsg>,
    /// Alerts are also handed here for out-of-band delivery + the alert log.
    pub notify: mpsc::UnboundedSender<Alert>,
    pub paths: persist::Paths,
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
    bcast: broadcast::Sender<crate::server::ServerMsg>,
    cfg: FusionCfg,
    meta: SnapshotMeta,
    own: Option<OwnShipFix>,
    statics: HashMap<u32, AisBody>,
    cooldown: HashMap<String, DateTime<Utc>>,
    zone_occ: HashSet<(String, String)>,
    sensor_seq: u64,
    ticks: u64,
    notify: mpsc::UnboundedSender<Alert>,
    /// "entry_id:track_id" pairs already alerted, so a watchlist hit fires once.
    watch_hit: HashSet<String>,
    /// Feeds currently known to be silent.
    stalled: HashSet<String>,
    paths: persist::Paths,
    record: bool,
    record_interval_s: u64,
    retention_days: u32,
    last_record: DateTime<Utc>,
    last_prune_day: String,
}

impl Fusion {
    pub fn new(deps: FusionDeps, cfg: FusionCfg, meta: SnapshotMeta, storage: &StorageCfg) -> Self {
        Fusion {
            store: deps.store,
            bcast: deps.bcast,
            cfg,
            meta,
            own: None,
            statics: HashMap::new(),
            cooldown: HashMap::new(),
            zone_occ: HashSet::new(),
            sensor_seq: 0,
            ticks: 0,
            notify: deps.notify,
            watch_hit: HashSet::new(),
            stalled: HashSet::new(),
            paths: deps.paths,
            record: storage.record_tracks,
            record_interval_s: storage.record_interval_s.max(10),
            retention_days: storage.retention_days.max(1),
            last_record: Utc::now(),
            last_prune_day: String::new(),
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
            let _ = self.bcast.send(crate::server::ServerMsg::Alert(
                serde_json::to_value(a).unwrap_or(Value::Null),
            ));
            // out-of-band delivery + on-disk alert log (the notify task owns both)
            let _ = self.notify.send(a.clone());
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
                    let mut newly_carrier = false;
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
                    st.classify_identity(&id);
                    // One alert per track, and only where a position exists to
                    // report: a static-only message carries no coordinates.
                    if let Some(t) = st.tracks.get_mut(&id) {
                        if t.carrier && !t.carrier_alerted {
                            t.carrier_alerted = true;
                            newly_carrier = true;
                        }
                    }
                    if newly_carrier
                        && cooldown_ok(
                            &mut self.cooldown,
                            self.cfg.alert_cooldown_s,
                            &format!("carrier:{id}"),
                            ts,
                        )
                    {
                        let (label, sog, cog) = st
                            .tracks
                            .get(&id)
                            .map(|t| {
                                (
                                    t.name.clone().unwrap_or_else(|| format!("MMSI {mmsi}")),
                                    t.sog,
                                    t.cog,
                                )
                            })
                            .unwrap_or((format!("MMSI {mmsi}"), None, None));
                        let course = match (sog, cog) {
                            (Some(s), Some(c)) => {
                                format!(" making {s:.0} kn on {c:.0}°")
                            }
                            _ => String::new(),
                        };
                        pending.push(mk_alert(
                            "carrier_contact",
                            "high",
                            format!(
                                "AIRCRAFT CARRIER — {label} identified near {lat:.3}, {lon:.3}{course}. Carriers often stop transmitting AIS at sea."
                            ),
                            Some(id.clone()),
                            lat,
                            lon,
                        ));
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
                    // No position in this message: classify, and let the next
                    // position report raise the alert.
                    if let Some(id) = st.mmsi_index.get(&mmsi).cloned() {
                        st.classify_identity(&id);
                    }
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
                    // Closest-approach data only arrives from target trackers.
                    if c.cpa_m.is_some() {
                        t.cpa_m = c.cpa_m;
                    }
                    if c.tcpa_min.is_some() {
                        t.tcpa_min = c.tcpa_min;
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
            // A sensor label can name a carrier too ("USS NIMITZ" off a radar
            // tracker), so classification runs for every feed, not just AIS.
            // Classify (the label may have just named it), then decide from the
            // per-track flag rather than from classify's return value: creating
            // the track already classified it, which would swallow the "newly"
            // signal and with it the alert.
            st.classify_identity(&id);
            let newly_carrier = st
                .tracks
                .get_mut(&id)
                .map(|t| {
                    if t.carrier && !t.carrier_alerted {
                        t.carrier_alerted = true;
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if newly_carrier {
                let label = st
                    .tracks
                    .get(&id)
                    .and_then(|t| t.name.clone())
                    .unwrap_or_else(|| c.label.clone().unwrap_or_else(|| id.clone()));
                pending.push(mk_alert(
                    "carrier_contact",
                    "high",
                    format!(
                        "AIRCRAFT CARRIER — {label} identified near {:.3}, {:.3} ({}). Carriers often stop transmitting AIS at sea.",
                        c.lat,
                        c.lon,
                        c.source.as_str()
                    ),
                    Some(id.clone()),
                    c.lat,
                    c.lon,
                ));
            }
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
            cpa_m: rt.cpa_m,
            tcpa_min: rt.tcpa_min,
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
                ages.sort_by_key(|a| std::cmp::Reverse(a.1));
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
                let label = t.name.clone().unwrap_or_else(|| match t.mmsi {
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
            // 3b. watchlist hits — the whole point of a watchlist is not having
            // to stare at the map, so these are high severity.
            let entries = st.watchlist.clone();
            if !entries.is_empty() {
                let mut hits: Vec<(String, String, String, f64, f64, Option<String>)> = Vec::new();
                for t in st.tracks.values() {
                    for e in &entries {
                        if e.matches_track(t) {
                            hits.push((
                                e.id.clone(),
                                e.label(),
                                t.id.clone(),
                                t.lat,
                                t.lon,
                                e.note.clone(),
                            ));
                        }
                    }
                }
                for (eid, label, tid, lat, lon, note) in hits {
                    if self.watch_hit.insert(format!("{eid}:{tid}")) {
                        let shown = st
                            .tracks
                            .get(&tid)
                            .and_then(|t| t.name.clone())
                            .unwrap_or_else(|| tid.clone());
                        let msg = match note.filter(|n| !n.trim().is_empty()) {
                            Some(n) => format!(
                                "WATCHLIST HIT — {label} ({n}) is on the map as \"{shown}\" at {lat:.4}, {lon:.4}"
                            ),
                            None => format!(
                                "WATCHLIST HIT — {label} is on the map as \"{shown}\" at {lat:.4}, {lon:.4}"
                            ),
                        };
                        cands.push(Cand {
                            key: format!("watch:{eid}:{tid}"),
                            kind: "watchlist_hit",
                            severity: "high",
                            message: msg,
                            track_id: tid,
                            lat,
                            lon,
                        });
                    }
                }
                // forget hits for deleted entries so re-adding arms them again
                let live: HashSet<String> = entries.iter().map(|e| e.id.clone()).collect();
                self.watch_hit.retain(|k| {
                    k.split(':')
                        .next()
                        .map(|eid| live.contains(eid))
                        .unwrap_or(false)
                });
            }

            // 3c. collision risk from target-tracker CPA/TCPA (only fresh data)
            for t in st.tracks.values() {
                let (Some(cpa), Some(tcpa)) = (t.cpa_m, t.tcpa_min) else {
                    continue;
                };
                let fresh = t
                    .last_sensor_contact
                    .map(|s| (now - s).num_seconds() <= 60)
                    .unwrap_or(false);
                if !fresh {
                    continue;
                }
                if cpa <= self.cfg.collision_cpa_m
                    && (0.0..=self.cfg.collision_tcpa_min).contains(&tcpa)
                {
                    let label = t.name.clone().unwrap_or_else(|| t.id.clone());
                    cands.push(Cand {
                        key: format!("collision:{}", t.id),
                        kind: "collision_risk",
                        severity: "high",
                        message: format!(
                            "COLLISION RISK {label}: closest approach {cpa:.0} m in {tcpa:.1} min at {:.4}, {:.4}",
                            t.lat, t.lon
                        ),
                        track_id: t.id.clone(),
                        lat: t.lat,
                        lon: t.lon,
                    });
                }
            }

            // 3d. feeds that are connected but have gone quiet (a stale map
            // looks identical to a quiet ocean, so this matters)
            let stall_after = self.cfg.feed_stall_after_s;
            let mut stalled_now: HashSet<String> = HashSet::new();
            for (name, f) in &st.feeds {
                if f.state != "connected" {
                    continue;
                }
                let age = f.last_line.map(|l| (now - l).num_seconds()).unwrap_or(0);
                if age >= stall_after {
                    stalled_now.insert(name.clone());
                }
            }
            let (aoi_lat, aoi_lon) = (self.meta.aoi.center_lat, self.meta.aoi.center_lon);
            let mut feed_events: Vec<(String, bool, i64)> = Vec::new();
            for name in &stalled_now {
                if !self.stalled.contains(name) {
                    let age = st
                        .feeds
                        .get(name)
                        .and_then(|f| f.last_line)
                        .map(|l| (now - l).num_seconds())
                        .unwrap_or(0);
                    feed_events.push((name.clone(), true, age));
                }
            }
            for name in self.stalled.iter() {
                if !stalled_now.contains(name) {
                    feed_events.push((name.clone(), false, 0));
                }
            }
            self.stalled = stalled_now;
            for (name, down, age) in feed_events {
                if down {
                    cands.push(Cand {
                        key: format!("feeddown:{name}"),
                        kind: "feed_stalled",
                        severity: "medium",
                        message: format!(
                            "FEED SILENT — {name} is connected but sent nothing for {age}s; the picture may be stale"
                        ),
                        track_id: String::new(),
                        lat: aoi_lat,
                        lon: aoi_lon,
                    });
                } else {
                    cands.push(Cand {
                        key: format!("feedup:{name}"),
                        kind: "feed_recovered",
                        severity: "info",
                        message: format!("feed recovered — {name} is streaming again"),
                        track_id: String::new(),
                        lat: aoi_lat,
                        lon: aoi_lon,
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

            // 5. optional position history (enables the replay scrubber)
            if self.record
                && (now - self.last_record).num_seconds() >= self.record_interval_s as i64
            {
                self.last_record = now;
                let day = persist::day_key(now);
                if day != self.last_prune_day {
                    self.last_prune_day = day.clone();
                    persist::prune_history(&self.paths.history, self.retention_days);
                }
                let mut body = String::new();
                for t in st.tracks.values() {
                    let p = persist::HistoryPoint {
                        ts: now,
                        id: t.id.clone(),
                        mmsi: t.mmsi,
                        name: t.name.clone(),
                        lat: t.lat,
                        lon: t.lon,
                        sog: t.sog,
                        cog: t.cog,
                        dark: t.dark,
                        class: t.classification.clone(),
                    };
                    if let Ok(line) = serde_json::to_string(&p) {
                        body.push_str(&line);
                        body.push('\n');
                    }
                }
                if !body.is_empty() {
                    persist::append_raw(&self.paths.history_day(&day), &body);
                }
            }

            // 4. broadcast the state snapshot (trails shrink and the cadence
            // slows as the picture grows, so a worldwide view stays workable)
            // Trails cost payload (points x tracks) every tick, so the budget
            // shrinks as the picture grows — but never to zero: a zoomed-in
            // client still wants a track's recent history, and per-connection
            // filtering drops the trails entirely for whole-world views.
            let n = st.tracks.len();
            self.meta.trail_limit = if n > 1500 {
                30
            } else if n > 400 {
                45
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
            if self.ticks.is_multiple_of(stride) {
                let snap = Arc::new(st.snapshot(&self.meta));
                let _ = self.bcast.send(crate::server::ServerMsg::State(snap));
            }
        }
        self.zone_occ = occupancy;
        self.flush_alerts(pending).await;
    }
}
