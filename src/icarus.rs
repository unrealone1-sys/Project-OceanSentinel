//! Project Icarus — the aerospace domain: aircraft store, poller and alerts.
//!
//! This is deliberately a *sibling* of the maritime stack rather than a fork of
//! it. Ships and aircraft share almost no physics (a vessel reports every few
//! seconds and drifts; an airliner crosses a viewport in a minute) and the two
//! maps are separate pages, so the aircraft side keeps its own store, its own
//! broadcast channel and its own watchlist file. What it borrows is everything
//! that is genuinely domain-neutral: the `Alert` type and its delivery
//! pipeline (webhook / Telegram / the shared alert log), `TrailPoint`, the
//! atomic file persistence, and the geodesy helpers.
//!
//! Two detection ideas carried over from the maritime side, because they are
//! the ones that matter for an OSINT air picture:
//!
//! * **Emergency squawks** — 7500/7600/7700 and the ADS-B emergency field are
//!   broadcast in the clear, and almost nobody is looking at them at scale.
//! * **Lost contact** — an airborne aircraft that stops being heard. Like a
//!   dark vessel this is *not* proof of anything (receiver coverage is patchy
//!   and transponder range is line-of-sight), so the alert says so.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{debug, info, warn};

use crate::adsb::{self, AdsbClient, Aircraft};
use crate::config::IcarusCfg;
use crate::geo;
use crate::model::{severity_rank, Alert, TrailPoint};
use crate::persist;

/// What the Icarus WebSocket fans out.
#[derive(Clone)]
pub enum IcarusMsg {
    State(Arc<Value>),
    Alert(Value),
    Watch,
}

/// A viewport the browser is looking at (or the home box when nobody is).
#[derive(Debug, Clone, Copy)]
pub struct Area {
    pub w: f64,
    pub s: f64,
    pub e: f64,
    pub n: f64,
}

/// One watchlist entry, matched against live aircraft by transponder address,
/// callsign or tail number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IcarusWatch {
    #[serde(default)]
    pub id: String,
    pub hex: Option<String>,
    pub callsign: Option<String>,
    pub registration: Option<String>,
    pub note: Option<String>,
    #[serde(default = "Utc::now")]
    pub added: DateTime<Utc>,
}

impl IcarusWatch {
    pub fn label(&self) -> String {
        self.callsign
            .clone()
            .or_else(|| self.registration.clone())
            .or_else(|| self.hex.as_ref().map(|h| h.to_uppercase()))
            .unwrap_or_else(|| self.id.clone())
    }

    pub fn matches(&self, t: &AircraftTrack) -> bool {
        let eq = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
        if let Some(h) = self.hex.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
            if eq(h, &t.ac.hex) {
                return true;
            }
        }
        if let Some(c) = self
            .callsign
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            if let Some(tc) = t.ac.callsign.as_deref() {
                if eq(c, tc) {
                    return true;
                }
            }
        }
        if let Some(r) = self
            .registration
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
        {
            if let Some(tr) = t.ac.registration.as_deref() {
                if eq(r, tr) {
                    return true;
                }
            }
        }
        false
    }
}

/// A live aircraft track: the last state vector plus everything we have
/// accumulated about it (history trail, watch flag, detection flags).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AircraftTrack {
    pub id: String,
    #[serde(flatten)]
    pub ac: Aircraft,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// Seconds since the *network* last heard this aircraft. Keeps growing
    /// while the track is not refreshed, which is what detects a lost contact.
    pub age_s: f64,
    pub trail: Vec<TrailPoint>,
    /// Watchlist label this aircraft matched, if any.
    pub watch: Option<String>,
    /// Detection flags for the map: emergency | watchlist | military | lost.
    pub flags: Vec<String>,
    pub lost: bool,
}

impl AircraftTrack {
    fn has_flag(&self, f: &str) -> bool {
        self.flags.iter().any(|x| x == f)
    }
}

/// What the snapshot needs that isn't in the store.
#[derive(Debug, Clone)]
pub struct IcarusMeta {
    pub version: String,
    pub provider: String,
    pub opensky: bool,
    pub recording: bool,
    pub home: (f64, f64, f64),
    pub radius_nm: u32,
    pub global_mil: bool,
    pub trail_limit: usize,
    pub auth_required: bool,
}

pub struct IcarusStore {
    pub tracks: HashMap<String, AircraftTrack>,
    pub alerts: VecDeque<Alert>,
    pub watchlist: Vec<IcarusWatch>,
    /// Every transponder address seen in the global military sweep.
    pub mil_hexes: HashSet<String>,
    pub started: DateTime<Utc>,
    pub total_alerts: u64,
    pub alert_cap: usize,
    pub paths: persist::Paths,
    /// Query plan currently in force (for the UI's coverage display).
    pub circles: Vec<adsb::Circle>,
    pub mode: String,
    pub degraded: bool,
    pub queries: u64,
    pub last_fetch: Option<DateTime<Utc>>,
    pub last_fetch_count: usize,
    /// Effective lost-contact threshold: the configured value, or longer when
    /// the request budget stretches the sweep beyond it.
    pub lost_after_s: f64,
    pub client_stats: adsb::ClientStats,
    /// Last alert time and severity per `kind:id` key, so a *more severe*
    /// event can break through the cooldown while a repeat cannot.
    cooldowns: HashMap<String, (DateTime<Utc>, u8)>,
    last_record: Option<DateTime<Utc>>,
}

impl IcarusStore {
    pub fn new(
        cfg: &IcarusCfg,
        paths: persist::Paths,
        watch_from_config: Vec<IcarusWatch>,
    ) -> Self {
        let mut store = IcarusStore {
            tracks: HashMap::new(),
            alerts: VecDeque::new(),
            watchlist: watch_from_config,
            mil_hexes: HashSet::new(),
            started: Utc::now(),
            total_alerts: 0,
            alert_cap: 500,
            paths,
            circles: Vec::new(),
            mode: "regional".to_string(),
            degraded: false,
            queries: 0,
            last_fetch: None,
            last_fetch_count: 0,
            lost_after_s: cfg.lost_contact_s as f64,
            client_stats: adsb::ClientStats::default(),
            cooldowns: HashMap::new(),
            last_record: None,
        };
        if let Some(saved) = persist::load_json::<Vec<IcarusWatch>>(&store.paths.icarus_watchlist())
        {
            for e in saved {
                if !store.watchlist.iter().any(|w| w.id == e.id) {
                    store.watchlist.push(e);
                }
            }
        }
        for e in store.watchlist.iter_mut() {
            if e.id.trim().is_empty() {
                e.id = format!("cfg-{}", &uuid::Uuid::new_v4().to_string()[..8]);
            }
        }
        if !store.watchlist.is_empty() {
            info!(
                "Project Icarus watchlist holds {} entries",
                store.watchlist.len()
            );
        }
        // The alert log is shared with the maritime side, so restore only the
        // aircraft kinds — a restart should not show ship alerts in the air feed.
        let lines = persist::load_lines(&store.paths.alerts());
        let tail = lines.len().saturating_sub(200);
        for line in &lines[tail..] {
            if let Ok(a) = serde_json::from_str::<Alert>(line) {
                if a.kind.starts_with("aircraft_") {
                    store.alerts.push_back(a);
                    store.total_alerts += 1;
                }
            }
        }
        store
    }

    pub fn save_watchlist(&self) {
        if let Err(e) = persist::save_json(&self.paths.icarus_watchlist(), &self.watchlist) {
            warn!("could not persist the aircraft watchlist: {e}");
        }
    }

    fn push_alert(&mut self, spec: AlertSpec, now: DateTime<Utc>) -> Option<Alert> {
        let key = format!("{}:{}", spec.kind, spec.id);
        let rank = severity_rank(spec.severity);
        if spec.cooldown_s > 0 {
            if let Some((last, last_rank)) = self.cooldowns.get(&key) {
                // A repeat of the same or a lesser severity stays quiet; an
                // escalation (lifeguard -> squawk 7700) must get through.
                if (now - *last).num_seconds() < spec.cooldown_s && rank <= *last_rank {
                    return None;
                }
            }
        }
        if self.cooldowns.len() > 4000 {
            let cutoff = now - chrono::Duration::seconds(spec.cooldown_s.max(60) * 4);
            self.cooldowns.retain(|_, (t, _)| *t > cutoff);
        }
        self.cooldowns.insert(key, (now, rank));
        let alert = Alert {
            id: uuid::Uuid::new_v4().to_string(),
            ts: now,
            kind: spec.kind.to_string(),
            severity: spec.severity.to_string(),
            message: spec.message,
            track_id: Some(spec.id),
            lat: spec.lat,
            lon: spec.lon,
        };
        self.total_alerts += 1;
        self.alerts.push_back(alert.clone());
        while self.alerts.len() > self.alert_cap {
            self.alerts.pop_front();
        }
        Some(alert)
    }

    /// Insert or refresh one aircraft. Returns any alert it raised.
    pub fn upsert(&mut self, mut a: Aircraft, cfg: &IcarusCfg, now: DateTime<Utc>) -> Vec<Alert> {
        // The global military sweep tells us identity that a regional query
        // cannot: military aircraft are not flagged in point responses.
        if self.mil_hexes.contains(&a.hex) {
            a.military = true;
        }
        let id = a.hex.clone();
        let label = a.label();
        let emergency = a.emergency.clone();
        let severity = adsb::emergency_severity(&emergency);
        let is_mil = a.military;
        let trail_cap = cfg.trail_points.max(2);

        // Scoped so the mutable borrow of the track map ends before any alert
        // is pushed (push_alert borrows the whole store).
        let (fire_emergency, fire_military, lat, lon) = match self.tracks.get_mut(&id) {
            Some(t) => {
                let prev_emergency = t.ac.emergency.clone();
                let was_military = t.has_flag("military");
                // hearing it again clears a lost-contact flag
                if t.lost && a.seen_s < cfg.lost_contact_s as f64 {
                    t.lost = false;
                    t.flags.retain(|f| f != "lost");
                }
                let last = t.trail.last().map(|p| (p.lat, p.lon, p.ts));
                let moved = match last {
                    Some((la, lo, ts)) => {
                        geo::haversine_m(la, lo, a.lat, a.lon) > 900.0
                            || (now - ts).num_seconds() >= 15
                    }
                    None => true,
                };
                t.ac = a;
                t.last_seen = now;
                t.age_s = t.ac.seen_s;
                if is_mil {
                    if !t.has_flag("military") {
                        t.flags.push("military".to_string());
                    }
                } else {
                    t.flags.retain(|f| f != "military");
                }
                if moved {
                    t.trail.push(TrailPoint {
                        lat: t.ac.lat,
                        lon: t.ac.lon,
                        ts: now,
                    });
                    if t.trail.len() > trail_cap {
                        let excess = t.trail.len() - trail_cap;
                        t.trail.drain(0..excess);
                    }
                }
                let fire = !emergency.is_empty() && emergency != prev_emergency;
                if fire && !t.has_flag("emergency") {
                    t.flags.push("emergency".to_string());
                }
                (fire, is_mil && !was_military, t.ac.lat, t.ac.lon)
            }
            None => {
                let mut t = AircraftTrack {
                    id: id.clone(),
                    first_seen: now,
                    last_seen: now,
                    age_s: a.seen_s,
                    trail: vec![TrailPoint {
                        lat: a.lat,
                        lon: a.lon,
                        ts: now,
                    }],
                    watch: None,
                    flags: Vec::new(),
                    lost: false,
                    ac: a,
                };
                if t.ac.military {
                    t.flags.push("military".to_string());
                }
                if t.ac.is_emergency() {
                    t.flags.push("emergency".to_string());
                }
                let (lat, lon) = (t.ac.lat, t.ac.lon);
                self.tracks.insert(id.clone(), t);
                (!emergency.is_empty(), is_mil, lat, lon)
            }
        };

        let mut out = Vec::new();
        if cfg.alert_emergency && fire_emergency {
            if let Some(al) = self.push_alert(
                AlertSpec {
                    kind: "aircraft_emergency",
                    severity,
                    message: format!(
                        "AIRCRAFT EMERGENCY — {label} ({}) {} near {lat:.3}, {lon:.3}",
                        id.to_uppercase(),
                        adsb::emergency_text(&emergency)
                    ),
                    id: id.clone(),
                    lat,
                    lon,
                    cooldown_s: cfg.alert_cooldown_s,
                },
                now,
            ) {
                out.push(al);
            }
        }
        if cfg.alert_military && fire_military {
            let ty = self
                .tracks
                .get(&id)
                .and_then(|t| t.ac.type_code.clone())
                .unwrap_or_else(|| "type unknown".to_string());
            if let Some(al) = self.push_alert(
                AlertSpec {
                    kind: "aircraft_military",
                    severity: "info",
                    message: format!(
                        "MILITARY / GOVERNMENT AIRBORNE — {label} ({}) {ty} near {lat:.3}, {lon:.3}",
                        id.to_uppercase()
                    ),
                    id: id.clone(),
                    lat,
                    lon,
                    cooldown_s: cfg.alert_cooldown_s,
                },
                now,
            ) {
                out.push(al);
            }
        }

        // Hard cap: a country-sized view can pull in thousands of aircraft, so
        // the least recently heard are dropped.
        let cap = cfg.max_tracks.max(10);
        if self.tracks.len() > cap {
            let mut by_age: Vec<(String, DateTime<Utc>)> = self
                .tracks
                .iter()
                .map(|(k, t)| (k.clone(), t.last_seen))
                .collect();
            by_age.sort_by_key(|(_, ts)| *ts);
            let excess = self.tracks.len() - cap;
            for (k, _) in by_age.into_iter().take(excess) {
                self.tracks.remove(&k);
            }
        }
        out
    }

    /// Age every track, raise lost-contact alerts, and evict what has gone.
    pub fn expire(&mut self, cfg: &IcarusCfg, now: DateTime<Utc>) -> Vec<Alert> {
        let mut pending: Vec<Pending> = Vec::new();
        let mut drop_ids: Vec<String> = Vec::new();
        for t in self.tracks.values_mut() {
            let since_update = (now - t.last_seen).num_milliseconds() as f64 / 1000.0;
            t.age_s = t.ac.seen_s + since_update;
            let airborne = !t.ac.on_ground && t.ac.alt_ft.unwrap_or(0) > 0;
            if cfg.alert_lost
                && !t.lost
                && airborne
                && t.age_s > self.lost_after_s
                && t.age_s < cfg.drop_after_s as f64
            {
                t.lost = true;
                if !t.has_flag("lost") {
                    t.flags.push("lost".to_string());
                }
                pending.push(Pending {
                    id: t.id.clone(),
                    lat: t.ac.lat,
                    lon: t.ac.lon,
                    message: format!(
                        "AIRCRAFT LOST CONTACT — {} ({}) unheard for {:.0}s, last at {:.3}, {:.3} ({}) — transponder off, or out of receiver range",
                        t.ac.label(),
                        t.ac.hex.to_uppercase(),
                        t.age_s,
                        t.ac.lat,
                        t.ac.lon,
                        t.ac
                            .alt_ft
                            .map(|a| format!("{a} ft"))
                            .unwrap_or_else(|| "altitude unknown".to_string()),
                    ),
                });
            }
            if since_update > cfg.drop_after_s as f64 {
                drop_ids.push(t.id.clone());
            }
        }
        let mut out = Vec::new();
        for p in pending {
            if let Some(al) = self.push_alert(
                AlertSpec {
                    kind: "aircraft_lost",
                    severity: "medium",
                    message: p.message,
                    id: p.id,
                    lat: p.lat,
                    lon: p.lon,
                    cooldown_s: cfg.alert_cooldown_s,
                },
                now,
            ) {
                out.push(al);
            }
        }
        for id in drop_ids {
            self.tracks.remove(&id);
        }
        out
    }

    /// Flag (and on first match, alert) every watched aircraft currently tracked.
    pub fn watch_hits(&mut self, cfg: &IcarusCfg, now: DateTime<Utc>) -> Vec<Alert> {
        let watches = self.watchlist.clone();
        if watches.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let ids: Vec<String> = self.tracks.keys().cloned().collect();
        for id in ids {
            let Some(hit) = watches
                .iter()
                .find(|w| self.tracks.get(&id).map(|t| w.matches(t)).unwrap_or(false))
            else {
                continue;
            };
            let label = hit.label();
            let first = self
                .tracks
                .get(&id)
                .map(|t| !t.has_flag("watchlist"))
                .unwrap_or(false);
            let (lat, lon, ac_label, hex) = match self.tracks.get_mut(&id) {
                Some(t) => {
                    t.watch = Some(label.clone());
                    if first {
                        t.flags.push("watchlist".to_string());
                    }
                    (t.ac.lat, t.ac.lon, t.ac.label(), t.ac.hex.to_uppercase())
                }
                None => continue,
            };
            if first && cfg.alert_watchlist {
                if let Some(al) = self.push_alert(
                    AlertSpec {
                        kind: "aircraft_watchlist",
                        severity: "high",
                        message: format!(
                            "WATCHLIST AIRCRAFT — {ac_label} ({hex}) is airborne near {lat:.3}, {lon:.3} [watching for {label}]"
                        ),
                        id: id.clone(),
                        lat,
                        lon,
                        cooldown_s: cfg.alert_cooldown_s,
                    },
                    now,
                ) {
                    out.push(al);
                }
            }
        }
        out
    }

    /// Archive the current picture to JSONL every `interval_s`.
    pub fn record(&mut self, paths: &persist::Paths, now: DateTime<Utc>, interval_s: u64) {
        if let Some(last) = self.last_record {
            if (now - last).num_seconds() < interval_s.max(30) as i64 {
                return;
            }
        }
        self.last_record = Some(now);
        let mut body = String::new();
        for t in self.tracks.values() {
            let line = json!({
                "ts": now,
                "hex": t.ac.hex,
                "callsign": t.ac.callsign,
                "registration": t.ac.registration,
                "type": t.ac.type_code,
                "lat": t.ac.lat,
                "lon": t.ac.lon,
                "alt_ft": t.ac.alt_ft,
                "gs_kt": t.ac.gs_kt,
                "track_deg": t.ac.track_deg,
                "squawk": t.ac.squawk,
                "military": t.ac.military,
                "on_ground": t.ac.on_ground,
                "country": t.ac.country,
            });
            if let Ok(s) = serde_json::to_string(&line) {
                body.push_str(&s);
                body.push('\n');
            }
        }
        if !body.is_empty() {
            persist::append_raw(&paths.icarus_history_day(&persist::day_key(now)), &body);
        }
    }

    pub fn snapshot(&self, meta: &IcarusMeta) -> Value {
        let now = Utc::now();
        let mut aircraft: Vec<Value> = self
            .tracks
            .values()
            .map(|t| {
                let mut v = serde_json::to_value(t).unwrap_or(Value::Null);
                if let Some(obj) = v.as_object_mut() {
                    // publish the icon class once, here, so the map does not
                    // re-implement the category rules in JavaScript
                    obj.insert("shape".to_string(), json!(t.ac.shape()));
                }
                if let Some(arr) = v.get_mut("trail").and_then(|x| x.as_array_mut()) {
                    if arr.len() > meta.trail_limit {
                        let skip = arr.len() - meta.trail_limit;
                        arr.drain(0..skip);
                    }
                }
                v
            })
            .collect();
        // Emergency, then watched, then military, then most recently heard —
        // the order an operator would want the list in.
        let rank = |v: &Value| {
            let flags = v.get("flags").and_then(|f| f.as_array());
            let has = |name: &str| {
                flags
                    .map(|f| f.iter().any(|x| x.as_str() == Some(name)))
                    .unwrap_or(false)
            };
            let mut r = 0;
            if has("emergency") {
                r += 100;
            }
            if has("watchlist") {
                r += 50;
            }
            if has("lost") {
                r += 25;
            }
            if has("military") {
                r += 10;
            }
            r
        };
        aircraft.sort_by(|a, b| {
            rank(b).cmp(&rank(a)).then_with(|| {
                b.get("last_seen")
                    .and_then(|x| x.as_str())
                    .cmp(&a.get("last_seen").and_then(|x| x.as_str()))
            })
        });

        let alerts: Vec<Value> = self
            .alerts
            .iter()
            .rev()
            .take(150)
            .map(|a| serde_json::to_value(a).unwrap_or(Value::Null))
            .collect();

        let airborne = self.tracks.values().filter(|t| !t.ac.on_ground).count();
        let emergency = self.tracks.values().filter(|t| t.ac.is_emergency()).count();
        let military = self.tracks.values().filter(|t| t.ac.military).count();
        let lost = self.tracks.values().filter(|t| t.lost).count();
        let watched = self
            .tracks
            .values()
            .filter(|t| t.has_flag("watchlist"))
            .count();

        // "throttled" is its own state: it is not an outage, it is the client
        // staying politely inside a small request budget.
        let feed_state = if self.client_stats.backoff_s > 0 {
            "throttled"
        } else if self.client_stats.failures > 0 && !self.client_stats.ok {
            "error"
        } else if self.client_stats.ok {
            "ok"
        } else {
            "connecting"
        };
        let feed_detail = match (&self.client_stats.last_error, feed_state) {
            (_, "throttled") => format!(
                "rate limited upstream — holding off {}s (the public feed grants ~1 request per 15s); aircraft stay on screen and age while the sweep rests",
                self.client_stats.backoff_s
            ),
            (Some(e), "error") => e.clone(),
            _ => format!(
                "{} aircraft on screen · {} circles · {} query(s) so far",
                self.tracks.len(),
                self.circles.len(),
                self.queries
            ),
        };

        json!({
            "type": "state",
            "domain": "air",
            "ts": now,
            "aircraft": aircraft,
            // the receiver queries the picture is built from, so the map can
            // show its own coverage instead of hiding the gaps
            "query_circles": self.circles,
            "alerts": alerts,
            "watchlist": self.watchlist,
            "feeds": [{
                "name": meta.provider,
                "kind": "adsb-http",
                "state": feed_state,
                "detail": feed_detail,
                "lines": self.client_stats.requests,
                "pps": 0.0,
                "last_line": self.client_stats.last_ok,
            }],
            "stats": {
                "aircraft": self.tracks.len(),
                "airborne": airborne,
                "on_ground": self.tracks.len() - airborne,
                "military": military,
                "emergency": emergency,
                "lost": lost,
                "watchlist": watched,
                "alerts": self.total_alerts,
                "queries": self.queries,
                "updated": self.last_fetch_count,
                "uptime_s": (now - self.started).num_seconds(),
            },
            "app": {
                "version": meta.version,
                "domain": "air",
                "project": "Icarus",
                "provider": meta.provider,
                "opensky": meta.opensky,
                "mode": self.mode,
                "degraded": self.degraded,
                "circles": self.circles.len(),
                "radius_nm": meta.radius_nm,
                "lost_after_s": self.lost_after_s.round() as i64,
                "global_mil": meta.global_mil,
                "home": { "lat": meta.home.0, "lon": meta.home.1, "zoom": meta.home.2 },
                "recording": meta.recording,
                "auth_required": meta.auth_required,
                "last_fetch": self.last_fetch,
            }
        })
    }
}

/// One alert to raise: the fields that differ per detection, bundled so
/// `push_alert` stays readable.
struct AlertSpec {
    kind: &'static str,
    severity: &'static str,
    message: String,
    id: String,
    lat: f64,
    lon: f64,
    cooldown_s: i64,
}

/// An alert raised while iterating the track map, pushed afterwards so the
/// borrow of the map ends first.
struct Pending {
    id: String,
    message: String,
    lat: f64,
    lon: f64,
}

/* ------------------------------------------------------------------ poller */

pub struct Icarus {
    pub cfg: IcarusCfg,
    pub store: Arc<RwLock<IcarusStore>>,
    pub client: Arc<AdsbClient>,
    bcast: broadcast::Sender<IcarusMsg>,
    notify: mpsc::UnboundedSender<Alert>,
    demand: Arc<RwLock<Option<(Area, Instant)>>>,
    paths: persist::Paths,
    retention_days: u32,
    meta: IcarusMeta,
}

impl Icarus {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: IcarusCfg,
        store: Arc<RwLock<IcarusStore>>,
        client: Arc<AdsbClient>,
        bcast: broadcast::Sender<IcarusMsg>,
        notify: mpsc::UnboundedSender<Alert>,
        demand: Arc<RwLock<Option<(Area, Instant)>>>,
        paths: persist::Paths,
        retention_days: u32,
        auth_required: bool,
    ) -> Self {
        let meta = IcarusMeta {
            version: env!("CARGO_PKG_VERSION").to_string(),
            provider: provider_name(&cfg),
            opensky: client.opensky_enabled(),
            recording: cfg.record,
            home: cfg.home(),
            radius_nm: cfg.radius_nm,
            global_mil: cfg.global_mil,
            trail_limit: cfg.trail_points,
            auth_required,
        };
        Icarus {
            cfg,
            store,
            client,
            bcast,
            notify,
            demand,
            paths,
            retention_days,
            meta,
        }
    }

    pub async fn run(self) {
        let poll = self.cfg.poll_ms.clamp(1000, 300_000);
        info!(
            "Project Icarus (aerospace, ADS-B): ENABLED — provider {}, {} nm circles, {} per tick",
            self.meta.provider, self.cfg.radius_nm, self.cfg.queries_per_tick
        );
        if !self.cfg.global_mil {
            info!("global military sweep: OFF ([icarus] global_mil = false)");
        }
        if self.meta.recording {
            info!(
                "aircraft archive: ON every {}s into data/icarus/history/",
                self.cfg.record_interval_s
            );
        }
        let mut ticker = tokio::time::interval(Duration::from_millis(poll));
        let mut cursor = 0usize;
        let mut last_mil = Instant::now() - Duration::from_secs(86_400);
        let mut last_opensky = Instant::now() - Duration::from_secs(86_400);
        let mut last_prune: Option<DateTime<Utc>> = None;

        loop {
            ticker.tick().await;
            let now = Utc::now();

            // A viewport message from a browser is a *request for coverage*;
            // with nobody watching we sweep a home box instead, so alerts keep
            // flowing on an unattended install.
            let demand = *self.demand.read().await;
            let fresh = demand.filter(|(_, at)| at.elapsed() < Duration::from_secs(300));
            let (plan, watched) = match fresh {
                Some((area, _)) => (
                    adsb::plan(
                        area.w,
                        area.s,
                        area.e,
                        area.n,
                        self.cfg.radius_nm as f64,
                        self.cfg.max_circles,
                    ),
                    true,
                ),
                None => (
                    adsb::home_plan(
                        self.cfg.home_lat,
                        self.cfg.home_lon,
                        self.cfg.radius_nm as f64,
                        self.cfg.max_circles,
                    ),
                    false,
                ),
            };

            let mut got: HashMap<String, Aircraft> = HashMap::new();
            let n = plan.circles.len();
            let per_tick = self.cfg.queries_per_tick.clamp(1, 8);
            let mut issued = 0u64;
            // The public feed grants a small request budget, so a tick that
            // cannot afford a query is skipped outright: tracks age (and may
            // raise lost-contact alerts) rather than the poller blocking.
            let tick_s = poll as f64 / 1000.0;
            let wait = self.client.budget_wait_s().await;
            let affordable = wait <= tick_s * 0.75;
            if !affordable {
                debug!("air sweep resting {wait:.0}s to stay inside the feed's request budget");
            }
            if n > 0 && affordable {
                for k in 0..per_tick {
                    let c = plan.circles[(cursor + k) % n];
                    match self.client.point(c.lat, c.lon, c.radius_nm).await {
                        Ok(list) => {
                            issued += 1;
                            for a in list {
                                got.insert(a.hex.clone(), a);
                            }
                        }
                        Err(e) => debug!(
                            "aircraft query around {:.2},{:.2} failed: {e:#}",
                            c.lat, c.lon
                        ),
                    }
                }
                cursor = (cursor + per_tick) % n;
            }

            // The one query that reaches the whole planet.
            if self.cfg.global_mil
                && (last_mil.elapsed()
                    >= Duration::from_secs(self.cfg.mil_interval_s.clamp(10, 3600))
                    || plan.global)
            {
                last_mil = Instant::now();
                match self.client.military().await {
                    Ok(list) => {
                        issued += 1;
                        let mut hexes = HashSet::with_capacity(list.len());
                        for a in &list {
                            hexes.insert(a.hex.clone());
                        }
                        let count = hexes.len();
                        {
                            let mut store = self.store.write().await;
                            store.mil_hexes = hexes;
                        }
                        for a in list {
                            got.insert(a.hex.clone(), a);
                        }
                        debug!("global military sweep: {count} aircraft");
                    }
                    Err(e) => debug!("global military sweep failed: {e:#}"),
                }
            }

            // Optional global civil coverage, only when the view is wide and
            // only on a slow cadence (OpenSky limits are tight).
            if self.client.opensky_enabled()
                && plan.global
                && last_opensky.elapsed() >= Duration::from_secs(60)
            {
                last_opensky = Instant::now();
                match self
                    .client
                    .opensky_bbox(
                        fresh.map(|(a, _)| a.w).unwrap_or(-180.0),
                        fresh.map(|(a, _)| a.s).unwrap_or(-85.0),
                        fresh.map(|(a, _)| a.e).unwrap_or(180.0),
                        fresh.map(|(a, _)| a.n).unwrap_or(85.0),
                    )
                    .await
                {
                    Ok(list) => {
                        issued += 1;
                        for a in list {
                            got.insert(a.hex.clone(), a);
                        }
                    }
                    Err(e) => debug!("OpenSky sweep failed: {e:#}"),
                }
            }

            let meta = self.meta.clone();
            let mut store = self.store.write().await;
            store.mode = if plan.global {
                "global".into()
            } else {
                "regional".into()
            };
            store.degraded = plan.degraded;
            store.circles = plan.circles.clone();
            store.queries += issued;
            store.client_stats = self.client.stats().await;
            store.last_fetch_count = got.len();
            store.last_fetch = Some(now);

            let mut alerts: Vec<Alert> = Vec::new();
            for (_, a) in got.drain() {
                alerts.extend(store.upsert(a, &self.cfg, now));
            }
            alerts.extend(store.expire(&self.cfg, now));
            alerts.extend(store.watch_hits(&self.cfg, now));

            if self.cfg.record {
                store.record(&self.paths, now, self.cfg.record_interval_s);
            }
            if last_prune
                .map(|t| (now - t).num_hours() >= 6)
                .unwrap_or(true)
            {
                last_prune = Some(now);
                persist::prune_history(
                    &self.paths.icarus_dir().join("history"),
                    self.retention_days,
                );
            }

            let snap = Arc::new(store.snapshot(&meta));
            drop(store);

            for a in &alerts {
                // one pipeline for delivery (webhook/Telegram) and the shared log
                let _ = self.notify.send(a.clone());
                if let Ok(v) = serde_json::to_value(a) {
                    let _ = self.bcast.send(IcarusMsg::Alert(v));
                }
            }
            if !watched {
                debug!("no browser watching the air map — sweeping the home box");
            }
            let _ = self.bcast.send(IcarusMsg::State(snap));
        }
    }
}

/// Provider label for logs and the UI (derived from the configured base URL).
pub fn provider_name(cfg: &IcarusCfg) -> String {
    let base = cfg.base_url.trim();
    if base.is_empty() {
        return "adsb.lol".to_string();
    }
    base.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IcarusCfg;

    fn paths() -> persist::Paths {
        let dir = std::env::temp_dir().join(format!("os-icarus-{}", uuid::Uuid::new_v4()));
        persist::Paths::new(dir.to_str().unwrap())
    }

    fn ac(hex: &str, lat: f64, lon: f64) -> Aircraft {
        Aircraft {
            hex: hex.to_string(),
            callsign: Some("TEST123".to_string()),
            registration: Some("G-TEST".to_string()),
            type_code: Some("A320".to_string()),
            lat,
            lon,
            alt_ft: Some(31000),
            seen_s: 0.5,
            ..Default::default()
        }
    }

    fn setup() -> (IcarusStore, IcarusCfg) {
        let cfg = IcarusCfg::default();
        (IcarusStore::new(&cfg, paths(), Vec::new()), cfg)
    }

    #[test]
    fn upsert_inserts_then_refreshes_without_duplicating() {
        let (mut s, cfg) = setup();
        let now = Utc::now();
        s.upsert(ac("abc123", 51.0, 0.0), &cfg, now);
        assert_eq!(s.tracks.len(), 1);
        let later = now + chrono::Duration::seconds(5);
        s.upsert(ac("abc123", 51.05, 0.02), &cfg, later);
        assert_eq!(s.tracks.len(), 1, "same transponder = same track");
        let t = s.tracks.get("abc123").unwrap();
        assert!((t.ac.lat - 51.05).abs() < 1e-9);
        assert!(t.first_seen == now && t.last_seen == later);
        assert!(t.trail.len() >= 2, "movement should extend the trail");
    }

    #[test]
    fn trail_is_capped_and_does_not_grow_on_a_stationary_contact() {
        let (mut s, mut cfg) = setup();
        cfg.trail_points = 5;
        let now = Utc::now();
        for i in 0..30 {
            s.upsert(
                ac("abc123", 51.0, 0.0),
                &cfg,
                now + chrono::Duration::seconds(i),
            );
        }
        let t = s.tracks.get("abc123").unwrap();
        assert!(
            t.trail.len() <= 5,
            "trail must stay capped, got {}",
            t.trail.len()
        );
    }

    #[test]
    fn emergency_raises_one_alert_and_does_not_repeat() {
        let (mut s, cfg) = setup();
        let now = Utc::now();
        let mut a = ac("abc123", 51.0, 0.0);
        a.emergency = "unlawful".to_string();
        a.squawk = Some("7500".to_string());
        let alerts = s.upsert(a.clone(), &cfg, now);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].kind, "aircraft_emergency");
        assert_eq!(alerts[0].severity, "high");
        assert!(alerts[0].message.contains("7500"));
        // the same state on the next sweep is not a new event
        let again = s.upsert(a, &cfg, now + chrono::Duration::seconds(4));
        assert!(again.is_empty(), "an unchanged emergency must not re-alert");
        assert!(s.tracks.get("abc123").unwrap().has_flag("emergency"));
    }

    #[test]
    fn emergency_escalation_alerts_again_when_the_code_changes() {
        let (mut s, cfg) = setup();
        let now = Utc::now();
        let mut a = ac("abc123", 51.0, 0.0);
        a.emergency = "lifeguard".to_string();
        s.upsert(a.clone(), &cfg, now);
        a.emergency = "general".to_string();
        // an escalation inside the cooldown window must still get through
        let alerts = s.upsert(a, &cfg, now + chrono::Duration::seconds(5));
        assert_eq!(alerts.len(), 1, "escalation must break the cooldown");
        assert_eq!(alerts[0].severity, "medium");
        // ...but a de-escalation to the same code does not
        assert!(s
            .upsert(
                {
                    let mut b = ac("abc123", 51.0, 0.0);
                    b.emergency = "lifeguard".to_string();
                    b
                },
                &cfg,
                now + chrono::Duration::seconds(6)
            )
            .is_empty());
    }

    #[test]
    fn military_flag_propagates_from_the_global_sweep() {
        let (mut s, cfg) = setup();
        let now = Utc::now();
        s.mil_hexes.insert("abc123".to_string());
        s.upsert(ac("abc123", 51.0, 0.0), &cfg, now);
        let t = s.tracks.get("abc123").unwrap();
        assert!(t.ac.military);
        assert!(t.has_flag("military"));
    }

    #[test]
    fn lost_contact_fires_for_airborne_but_not_for_ground_or_landed_tracks() {
        let (mut s, cfg) = setup();
        let now = Utc::now();
        s.upsert(ac("air001", 51.0, 0.0), &cfg, now);
        let mut g = ac("gnd001", 51.0, 0.0);
        g.on_ground = true;
        g.alt_ft = None;
        s.upsert(g, &cfg, now);

        // not yet: still inside the silence window
        assert!(s
            .expire(&cfg, now + chrono::Duration::seconds(30))
            .is_empty());

        let alerts = s.expire(
            &cfg,
            now + chrono::Duration::seconds(cfg.lost_contact_s + 10),
        );
        assert_eq!(alerts.len(), 1, "only the airborne contact is lost");
        assert_eq!(alerts[0].kind, "aircraft_lost");
        assert!(alerts[0].message.contains("LOST CONTACT"));
        assert!(s.tracks.get("air001").unwrap().lost);
        assert!(!s.tracks.get("gnd001").unwrap().lost);
        // and it does not repeat on every subsequent sweep
        assert!(s
            .expire(
                &cfg,
                now + chrono::Duration::seconds(cfg.lost_contact_s + 20)
            )
            .is_empty());
    }

    #[test]
    fn a_stretched_threshold_suppresses_rotation_false_positives() {
        // With a tight request budget a circle can wait minutes for its turn;
        // an aircraft that is merely un-refreshed must not be called lost.
        let (mut s, cfg) = setup();
        let now = Utc::now();
        s.upsert(ac("air001", 51.0, 0.0), &cfg, now);

        let quiet = now + chrono::Duration::seconds(200);
        assert!(
            s.expire(&cfg, quiet).is_empty(),
            "inside the base threshold nothing fires"
        );

        // the poller decides the threshold is 600 s because the sweep is slow
        s.lost_after_s = 600.0;
        let still_quiet = now + chrono::Duration::seconds(400);
        assert!(
            s.expire(&cfg, still_quiet).is_empty(),
            "a slow sweep must not look like a disappearing aircraft"
        );

        let finally = now + chrono::Duration::seconds(700);
        let alerts = s.expire(&cfg, finally);
        assert_eq!(alerts.len(), 1, "beyond the stretched threshold it fires");
        assert_eq!(alerts[0].kind, "aircraft_lost");
    }

    #[test]
    fn tracks_are_evicted_once_they_are_long_gone() {
        let (mut s, cfg) = setup();
        let now = Utc::now();
        s.upsert(ac("abc123", 51.0, 0.0), &cfg, now);
        s.expire(&cfg, now + chrono::Duration::seconds(cfg.drop_after_s + 5));
        assert!(s.tracks.is_empty(), "a long-silent track should be dropped");
    }

    #[test]
    fn max_tracks_evicts_the_least_recently_heard() {
        let (mut s, mut cfg) = setup();
        cfg.max_tracks = 10;
        let now = Utc::now();
        for i in 0..25 {
            let mut a = ac(&format!("{:06x}", i), 51.0 + i as f64 * 0.01, 0.0);
            a.seen_s = 0.0;
            s.upsert(a, &cfg, now + chrono::Duration::milliseconds(i as i64));
        }
        assert!(s.tracks.len() <= 10, "cap not enforced: {}", s.tracks.len());
        assert!(s.tracks.contains_key("000018"), "newest track was evicted");
    }

    #[test]
    fn watchlist_matches_hex_callsign_or_tail_and_alerts_once() {
        let (mut s, cfg) = setup();
        let now = Utc::now();
        s.watchlist = vec![
            IcarusWatch {
                id: "w1".into(),
                hex: Some("ABC123".into()),
                callsign: None,
                registration: None,
                note: Some("test".into()),
                added: now,
            },
            IcarusWatch {
                id: "w2".into(),
                hex: None,
                callsign: Some("other1".into()),
                registration: None,
                note: None,
                added: now,
            },
            IcarusWatch {
                id: "w3".into(),
                hex: None,
                callsign: None,
                registration: Some("g-test".into()),
                note: None,
                added: now,
            },
        ];
        s.upsert(ac("abc123", 51.0, 0.0), &cfg, now);
        let hits = s.watch_hits(&cfg, now);
        // all three entries describe the same aircraft: one hit, one alert
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "aircraft_watchlist");
        assert_eq!(hits[0].severity, "high");
        assert!(s.tracks.get("abc123").unwrap().has_flag("watchlist"));
        assert!(s
            .watch_hits(&cfg, now + chrono::Duration::seconds(2))
            .is_empty());
    }

    #[test]
    fn snapshot_is_flat_and_ranked() {
        let (mut s, cfg) = setup();
        let now = Utc::now();
        s.upsert(ac("aaa111", 51.0, 0.0), &cfg, now);
        let mut e = ac("bbb222", 51.1, 0.1);
        e.emergency = "general".to_string();
        s.upsert(e, &cfg, now);
        let mut m = ac("ccc333", 51.2, 0.2);
        m.military = true;
        s.upsert(m, &cfg, now);

        let meta = IcarusMeta {
            version: "0.1.0".into(),
            provider: "adsb.lol".into(),
            opensky: false,
            recording: false,
            home: (51.47, -0.45, 8.0),
            radius_nm: 180,
            global_mil: true,
            trail_limit: 30,
            auth_required: false,
        };
        let snap = s.snapshot(&meta);
        let arr = snap.get("aircraft").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 3);
        // flattened fields must be at the top level, not nested under "ac"
        assert!(arr[0].get("hex").is_some());
        assert!(arr[0].get("ac").is_none());
        assert_eq!(arr[0].get("shape").unwrap().as_str().unwrap(), "air");
        assert_eq!(arr[0].get("hex").unwrap().as_str().unwrap(), "bbb222");
        assert_eq!(
            snap.pointer("/stats/aircraft").unwrap().as_u64().unwrap(),
            3
        );
        assert_eq!(
            snap.pointer("/stats/emergency").unwrap().as_u64().unwrap(),
            1
        );
        assert_eq!(
            snap.pointer("/stats/military").unwrap().as_u64().unwrap(),
            1
        );
        assert_eq!(
            snap.pointer("/app/project").unwrap().as_str().unwrap(),
            "Icarus"
        );
        assert_eq!(
            snap.pointer("/app/domain").unwrap().as_str().unwrap(),
            "air"
        );
        assert!(snap.get("feeds").unwrap().as_array().unwrap()[0]
            .get("name")
            .is_some());
    }

    #[test]
    fn watchlist_survives_a_restart() {
        let cfg = IcarusCfg::default();
        let p = paths();
        let mut s = IcarusStore::new(&cfg, p.clone(), Vec::new());
        s.watchlist.push(IcarusWatch {
            id: "w1".into(),
            hex: Some("abc123".into()),
            callsign: None,
            registration: None,
            note: Some("note".into()),
            added: Utc::now(),
        });
        s.save_watchlist();
        let reloaded = IcarusStore::new(&IcarusCfg::default(), p, Vec::new());
        assert_eq!(reloaded.watchlist.len(), 1);
        assert_eq!(reloaded.watchlist[0].hex.as_deref(), Some("abc123"));
    }

    #[test]
    fn recording_writes_a_usable_jsonl_archive() {
        let (mut s, cfg) = setup();
        let p = paths();
        let now = Utc::now();
        s.upsert(ac("abc123", 51.0, 0.0), &cfg, now);
        s.record(&p, now, 300);
        let file = p.icarus_history_day(&persist::day_key(now));
        let lines = persist::load_lines(&file);
        assert_eq!(lines.len(), 1);
        let v: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v.get("hex").unwrap().as_str().unwrap(), "abc123");
        // the interval gate keeps a second call from writing again
        s.record(&p, now + chrono::Duration::seconds(5), 300);
        assert_eq!(persist::load_lines(&file).len(), 1);
    }

    #[test]
    fn provider_name_is_derived_from_the_base_url() {
        let mut cfg = IcarusCfg::default();
        assert_eq!(provider_name(&cfg), "api.adsb.lol");
        cfg.base_url = "https://adsb.example.org/".into();
        assert_eq!(provider_name(&cfg), "adsb.example.org");
    }
}
