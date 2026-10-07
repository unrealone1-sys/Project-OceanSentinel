//! ADS-B ingestion for Project Icarus — the aerospace half of the platform.
//!
//! Aircraft broadcast identity, position, altitude, heading and *intent* in the
//! clear on 1090 MHz (ADS-B Out), and a volunteer receiver network re-publishes
//! that picture to the internet. This module talks to **adsb.lol**, a keyless
//! community aggregator that serves the standard ADSBexchange v2 JSON schema —
//! the aviation mirror of the AISStream feed used for ships.
//!
//! Two properties of that feed shape the whole design:
//!
//! * **Coverage is receiver-bound**, exactly like AIS: dense over Europe and
//!   North America, thin over Africa and South Asia. Regional queries return
//!   whatever the nearest volunteers can hear.
//! * **`/v2/mil` is global.** Military and government aircraft worldwide come
//!   back in a single request, which is what makes a world-zoom air picture
//!   possible without a paid feed.
//!
//! OpenSky Network is wired in as an optional second provider (client
//! credentials in `OPENSKY_CLIENT_ID` / `OPENSKY_CLIENT_SECRET`): it is the
//! only source here that answers a *global civil* query, at the price of
//! aggressive rate limiting. It is off by default.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::config::IcarusCfg;

const M_PER_FT: f64 = 0.3048;
const KT_PER_MPS: f64 = 1.943_844_5;
const FPM_PER_MPS: f64 = 196.850_394;

/* ------------------------------------------------------------------ model */

/// One normalized aircraft state vector, independent of which provider
/// produced it. Field names follow the map/UI (snake_case like the vessel
/// model so both UIs read the same way).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Aircraft {
    /// ICAO 24-bit transponder address, lowercase hex — the "MMSI of the sky".
    pub hex: String,
    pub callsign: Option<String>,
    /// Tail number ("r" in the feed), e.g. G-EZUC.
    pub registration: Option<String>,
    /// ICAO type designator, e.g. A320, B738, H60.
    pub type_code: Option<String>,
    pub lat: f64,
    pub lon: f64,
    /// Barometric altitude in feet (None while on the ground).
    pub alt_ft: Option<i32>,
    pub alt_geom_ft: Option<i32>,
    /// Altitude selected on the autopilot — where the crew *intends* to go.
    pub nav_alt_ft: Option<i32>,
    pub on_ground: bool,
    pub gs_kt: Option<f64>,
    pub ias_kt: Option<f64>,
    pub tas_kt: Option<f64>,
    pub mach: Option<f64>,
    pub track_deg: Option<f64>,
    pub mag_heading: Option<f64>,
    pub true_heading: Option<f64>,
    /// Vertical rate, feet per minute (positive = climbing).
    pub vert_rate_fpm: Option<i32>,
    pub roll_deg: Option<f64>,
    pub squawk: Option<String>,
    /// Normalized emergency token: "", general, lifeguard, minfuel, nordo,
    /// unlawful, downed, reserved.
    pub emergency: String,
    /// ADS-B emitter category (A3 = large airplane, C2 = light rotorcraft …).
    pub category: Option<String>,
    pub military: bool,
    pub interesting: bool,
    /// Privacy ICAO address / Limiting Aircraft Data Displayed.
    pub pia: bool,
    pub ladd: bool,
    /// Special position identification (pilot pressed "ident").
    pub spi: bool,
    /// Feed provenance: adsb_icao, adsb_other, mlat, tisb_*, opensky …
    pub source: String,
    /// Seconds since the network last heard *any* message from this aircraft.
    pub seen_s: f64,
    pub seen_pos_s: Option<f64>,
    pub messages: u64,
    pub rssi: Option<f64>,
    /// Distance/bearing from the receiver that heard it, when the feed says.
    pub dst_nm: Option<f64>,
    pub dir_deg: Option<f64>,
    pub country: Option<String>,
}

impl Aircraft {
    pub fn label(&self) -> String {
        self.callsign
            .clone()
            .or_else(|| self.registration.clone())
            .unwrap_or_else(|| self.hex.to_uppercase())
    }

    /// Coarse airframe class used to pick an icon.
    pub fn shape(&self) -> &'static str {
        if self.on_ground {
            return "ground";
        }
        match self.category.as_deref().unwrap_or("") {
            c if c.starts_with('C') => "heli",
            "B1" => "glider",
            "B2" => "balloon",
            _ => "air",
        }
    }

    pub fn is_emergency(&self) -> bool {
        !self.emergency.is_empty()
    }
}

/// Readable name for an emergency token (used in alert text and the drawer).
pub fn emergency_text(code: &str) -> &'static str {
    match code {
        "unlawful" => "UNLAWFUL INTERFERENCE (squawk 7500)",
        "nordo" => "RADIO FAILURE (squawk 7600)",
        "general" => "GENERAL EMERGENCY (squawk 7700)",
        "lifeguard" => "LIFEGUARD / MEDICAL",
        "minfuel" => "MINIMUM FUEL",
        "downed" => "AIRCRAFT DOWN",
        "reserved" => "RESERVED EMERGENCY CODE",
        _ => "",
    }
}

pub fn emergency_severity(code: &str) -> &'static str {
    match code {
        "unlawful" | "downed" => "high",
        "nordo" | "general" | "reserved" => "medium",
        "lifeguard" | "minfuel" => "info",
        _ => "info",
    }
}

/// Merge the explicit `emergency` field with the transponder codes that
/// override it, since a 7500/7600/7700 squawk is authoritative.
pub fn normalize_emergency(field: Option<&str>, squawk: Option<&str>) -> String {
    let sq = squawk.map(str::trim).unwrap_or("");
    let from_squawk = match sq {
        "7500" => Some("unlawful"),
        "7600" => Some("nordo"),
        "7700" => Some("general"),
        _ => None,
    };
    let from_field = match field.map(str::trim).unwrap_or("") {
        "" | "none" | "no" => None,
        other => Some(other),
    };
    match (from_squawk, from_field) {
        (Some(s), _) => s.to_string(),
        (None, Some(f)) => f.to_ascii_lowercase(),
        (None, None) => String::new(),
    }
}

/* -------------------------------------------------------------- providers */

/// One query circle: a receiver-independent "look here" request.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Circle {
    pub lat: f64,
    pub lon: f64,
    pub radius_nm: f64,
}

/// How to cover the current view, and whether the global military sweep is
/// needed because the regional grid cannot reach that far.
#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub circles: Vec<Circle>,
    /// True when the grid had to be coarsened (fewer circles than the span
    /// wants) or the view is wide enough that the global sweep is the story.
    pub degraded: bool,
    pub global: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ClientStats {
    pub requests: u64,
    pub failures: u64,
    pub rate_limited: u64,
    pub ok: bool,
    pub last_ok: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    /// Seconds until upstream is tried again after a 429 (0 = ready).
    pub backoff_s: u64,
}

/// HTTP client for the community ADS-B network, with a polite request cadence.
/// The public instances ask for at most ~1 request per second per client, so
/// every call goes through a single serialized gate.
pub struct AdsbClient {
    http: reqwest::Client,
    base: String,
    budget: Mutex<Budget>,
    stats: Mutex<ClientStats>,
    opensky: Option<OpenSky>,
    /// Airframe records change rarely: cached so clicking around the map does
    /// not spend request budget the sweep needs more.
    detail_cache: Mutex<HashMap<String, (Instant, Value)>>,
}

/// Token bucket for the upstream request budget.
///
/// The public community feed grants a small burst and then roughly one request
/// per 15 s (measured, see the module notes). A fixed "one request per second"
/// interval is therefore wrong in both directions: too slow to notice a block,
/// too fast to stay inside the allowance. This bucket spends a burst, then
/// refills slowly, and a 429 puts the client into a penalty that doubles on
/// every failure and halves on every success — so the feed self-calibrates to
/// whatever budget the operator is really granted.
struct Budget {
    tokens: f64,
    capacity: f64,
    refill_per_s: f64,
    /// Minimum spacing between requests, so a viewport drag cannot spend the
    /// whole bucket at once.
    min_gap: Duration,
    last: Instant,
    penalty_until: Option<Instant>,
    penalty_s: f64,
}

impl Budget {
    fn new(capacity: f64, window_s: f64, min_gap: Duration) -> Self {
        let capacity = capacity.max(1.0);
        let window_s = window_s.max(1.0);
        Budget {
            tokens: capacity,
            capacity,
            refill_per_s: capacity / window_s,
            min_gap,
            last: Instant::now() - min_gap,
            penalty_until: None,
            penalty_s: 0.0,
        }
    }

    /// Consume a token, or report how long to wait until one is available.
    fn take(&mut self) -> Option<Duration> {
        let now = Instant::now();
        if let Some(until) = self.penalty_until {
            if now < until {
                return Some(until - now);
            }
            self.penalty_until = None;
        }
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_s).min(self.capacity);
        if self.tokens < 1.0 {
            return Some(Duration::from_secs_f64(
                (1.0 - self.tokens) / self.refill_per_s,
            ));
        }
        if elapsed < self.min_gap.as_secs_f64() {
            return Some(self.min_gap - now.saturating_duration_since(self.last));
        }
        self.tokens -= 1.0;
        self.last = now;
        None
    }

    /// Called on HTTP 429. Doubles the hold-off, up to five minutes.
    fn penalize(&mut self) -> u64 {
        self.penalty_s = if self.penalty_s <= 0.0 {
            20.0
        } else {
            (self.penalty_s * 2.0).min(300.0)
        };
        self.penalty_until = Some(Instant::now() + Duration::from_secs_f64(self.penalty_s));
        // an emptied bucket makes the slowdown stick after the penalty expires
        self.tokens = 0.0;
        self.penalty_s as u64
    }

    /// Called on success: walk the penalty back down.
    fn reward(&mut self) {
        self.penalty_s *= 0.5;
        if self.penalty_s < 1.0 {
            self.penalty_s = 0.0;
        }
    }

    /// Seconds until a token would be available, without consuming one.
    fn wait_s(&self) -> f64 {
        let now = Instant::now();
        if let Some(until) = self.penalty_until {
            if now < until {
                return (until - now).as_secs_f64();
            }
        }
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        let tokens = (self.tokens + elapsed * self.refill_per_s).min(self.capacity);
        let gap_left = (self.min_gap.as_secs_f64() - elapsed).max(0.0);
        if tokens >= 1.0 {
            gap_left
        } else {
            ((1.0 - tokens) / self.refill_per_s).max(gap_left)
        }
    }

    fn backoff_s(&self) -> u64 {
        match self.penalty_until {
            Some(until) => until.saturating_duration_since(Instant::now()).as_secs() + 1,
            None => 0,
        }
    }
}

struct OpenSky {
    http: reqwest::Client,
    base: String,
    client_id: String,
    client_secret: String,
    token: Mutex<Option<(String, Instant)>>,
}

impl AdsbClient {
    pub fn new(cfg: &IcarusCfg) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            // the operators of these free feeds ask to be identifiable
            .user_agent(concat!(
                "OceanSentinel/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/unrealone1-sys/Project-OceanSentinel)"
            ))
            .build()
            .unwrap_or_default();
        let opensky = if cfg.opensky {
            match (
                std::env::var("OPENSKY_CLIENT_ID"),
                std::env::var("OPENSKY_CLIENT_SECRET"),
            ) {
                (Ok(id), Ok(secret)) if !id.trim().is_empty() && !secret.trim().is_empty() => {
                    Some(OpenSky {
                        http: http.clone(),
                        base: "https://opensky-network.org".to_string(),
                        client_id: id.trim().to_string(),
                        client_secret: secret.trim().to_string(),
                        token: Mutex::new(None),
                    })
                }
                _ => {
                    tracing::warn!(
                        "[icarus] opensky = true but OPENSKY_CLIENT_ID / OPENSKY_CLIENT_SECRET are unset — staying on the community feed"
                    );
                    None
                }
            }
        } else {
            None
        };
        AdsbClient {
            http,
            base: if cfg.base_url.trim().is_empty() {
                "https://api.adsb.lol".to_string()
            } else {
                cfg.base_url.trim_end_matches('/').to_string()
            },
            budget: Mutex::new(Budget::new(
                cfg.burst_requests as f64,
                cfg.burst_window_s as f64,
                Duration::from_millis(cfg.min_request_gap_ms.clamp(200, 30_000)),
            )),
            stats: Mutex::new(ClientStats::default()),
            opensky,
            detail_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn opensky_enabled(&self) -> bool {
        self.opensky.is_some()
    }

    pub async fn stats(&self) -> ClientStats {
        let mut s = self.stats.lock().await.clone();
        s.backoff_s = self.budget.lock().await.backoff_s();
        s
    }

    /// Seconds until one more request is affordable, without spending it. The
    /// poller uses this to skip a tick rather than block inside one.
    pub async fn budget_wait_s(&self) -> f64 {
        self.budget.lock().await.wait_s()
    }

    /// Wait until the budget allows one more upstream request.
    async fn acquire(&self) {
        loop {
            let wait = self.budget.lock().await.take();
            match wait {
                None => return,
                Some(d) => tokio::time::sleep(d).await,
            }
        }
    }

    async fn get_json(&self, url: &str) -> Result<Value> {
        let path = url.strip_prefix(&self.base).unwrap_or(url).to_string();
        self.acquire().await;
        {
            let mut s = self.stats.lock().await;
            s.requests += 1;
            s.backoff_s = 0;
        }
        tracing::debug!("ADS-B upstream request #{path}");
        let resp = match self.http.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                self.record_error(format!("{e:#}")).await;
                return Err(anyhow!(e)).context("ADS-B provider unreachable");
            }
        };
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let secs = self.budget.lock().await.penalize();
            let mut s = self.stats.lock().await;
            s.rate_limited += 1;
            s.failures += 1;
            s.ok = false;
            s.backoff_s = secs;
            s.last_error = Some(format!(
                "rate limited on {path} (HTTP 429) — holding off {secs}s"
            ));
            drop(s);
            bail!("ADS-B rate limit on {path} (HTTP 429) — holding off {secs}s");
        }
        if !status.is_success() {
            self.record_error(format!("HTTP {status}")).await;
            bail!("ADS-B provider returned HTTP {status}");
        }
        let v: Value = resp.json().await.context("parsing ADS-B JSON")?;
        if let Some(err) = v.get("error").and_then(Value::as_str) {
            self.record_error(err.to_string()).await;
            bail!("ADS-B provider error: {err}");
        }
        self.budget.lock().await.reward();
        {
            let mut s = self.stats.lock().await;
            s.ok = true;
            s.backoff_s = 0;
            s.last_ok = Some(Utc::now());
            s.last_error = None;
        }
        Ok(v)
    }

    /// The query path a 429 came from, used for the operator-facing message.
    async fn record_error(&self, msg: String) {
        let mut s = self.stats.lock().await;
        s.ok = false;
        s.failures += 1;
        s.last_error = Some(msg);
    }

    /// Regional query around one point (radius capped at the provider's 250 nm).
    pub async fn point(&self, lat: f64, lon: f64, radius_nm: f64) -> Result<Vec<Aircraft>> {
        let r = radius_nm.clamp(1.0, 250.0).round() as u32;
        let url = format!("{}/v2/point/{lat:.4}/{lon:.4}/{r}", self.base);
        let v = self.get_json(&url).await?;
        Ok(parse_ac(&v))
    }

    /// Every military / government aircraft the network can hear, worldwide.
    pub async fn military(&self) -> Result<Vec<Aircraft>> {
        let url = format!("{}/v2/mil", self.base);
        let v = self.get_json(&url).await?;
        Ok(parse_ac(&v))
    }

    /// Full record for one airframe (richer than the point query: owner,
    /// operator, model year where the network knows them).
    pub async fn detail(&self, hex: &str) -> Result<Value> {
        let hex = hex.trim().to_ascii_lowercase();
        if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("hex must be a 6-character ICAO address");
        }
        {
            let cache = self.detail_cache.lock().await;
            if let Some((at, v)) = cache.get(&hex) {
                if at.elapsed() < Duration::from_secs(900) {
                    return Ok(v.clone());
                }
            }
        }
        let url = format!("{}/v2/hex/{hex}", self.base);
        let v = self.get_json(&url).await?;
        let mut cache = self.detail_cache.lock().await;
        if cache.len() > 500 {
            cache.clear();
        }
        cache.insert(hex, (Instant::now(), v.clone()));
        Ok(v)
    }

    /// Global civil state vectors from OpenSky (optional provider).
    pub async fn opensky_bbox(&self, w: f64, s: f64, e: f64, n: f64) -> Result<Vec<Aircraft>> {
        let Some(os) = &self.opensky else {
            bail!("OpenSky is not configured");
        };
        let token = os.token().await?;
        self.acquire().await;
        let url = format!(
            "{}/api/states/all?lamin={s:.4}&lomin={w:.4}&lamax={n:.4}&lomax={e:.4}",
            os.base
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .context("OpenSky unreachable")?;
        let status = resp.status();
        let v: Value = resp.json().await.context("parsing OpenSky JSON")?;
        if !status.is_success() {
            let msg = v
                .get("message")
                .or_else(|| v.get("detail"))
                .and_then(Value::as_str)
                .unwrap_or("request rejected");
            bail!("OpenSky HTTP {status}: {msg}");
        }
        Ok(v.get("states")
            .and_then(Value::as_array)
            .map(|rows| rows.iter().filter_map(from_opensky).collect())
            .unwrap_or_default())
    }
}

impl OpenSky {
    async fn token(&self) -> Result<String> {
        {
            let cache = self.token.lock().await;
            if let Some((t, at)) = cache.as_ref() {
                if at.elapsed() < Duration::from_secs(25 * 60) {
                    return Ok(t.clone());
                }
            }
        }
        let resp = self
            .http
            .post(format!(
                "{}/auth/realms/opensky-network/protocol/openid-connect/token",
                self.base
            ))
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
            ])
            .send()
            .await
            .context("OpenSky auth unreachable")?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!(
                "OpenSky auth failed (HTTP {status}): {}",
                v.get("error_description")
                    .or_else(|| v.get("error"))
                    .and_then(Value::as_str)
                    .unwrap_or("no details")
            );
        }
        let tok = v
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("OpenSky auth returned no access_token"))?
            .to_string();
        *self.token.lock().await = Some((tok.clone(), Instant::now()));
        Ok(tok)
    }
}

/* ------------------------------------------------------------- parsing */

fn as_f64(v: &Value, k: &str) -> Option<f64> {
    match v.get(k) {
        Some(Value::Number(n)) => n.as_f64().filter(|x| x.is_finite()),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok().filter(|x| x.is_finite()),
        _ => None,
    }
}

fn as_i32(v: &Value, k: &str) -> Option<i32> {
    as_f64(v, k).map(|x| x.round() as i32)
}

fn as_text(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Callsigns arrive with the transponder's padding ("EZY21UM ") and, when a
/// crew types garbage into the FMS, as runs of filler ("@@@@@@@@" was seen
/// live). A callsign that carries no usable letters or digits is worse than
/// none, because the map labels the aircraft with it.
fn clean_callsign(raw: Option<String>) -> Option<String> {
    let s = raw?;
    let cleaned = s.trim();
    let alnum = cleaned
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .count();
    if alnum < 2 || cleaned.contains('@') || cleaned.contains('?') {
        return None;
    }
    Some(cleaned.to_string())
}

fn as_hex(v: &Value, k: &str) -> Option<String> {
    let s = as_text(v, k)?;
    let s = s.to_ascii_lowercase();
    if s.len() == 6 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(s)
    } else {
        None
    }
}

/// The feed reports "ground" in the altitude fields for taxiing aircraft.
fn ground_flag(v: &Value) -> bool {
    ["alt_baro", "alt_geom"].iter().any(|k| {
        v.get(*k)
            .and_then(Value::as_str)
            .map(|s| s.eq_ignore_ascii_case("ground"))
            .unwrap_or(false)
    })
}

fn altitude(v: &Value, k: &str) -> Option<i32> {
    // "ground" and junk values must not be read as numbers
    if v.get(k).map(|x| x.is_string()).unwrap_or(false) {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| s.parse::<f64>().is_ok())?;
    }
    as_i32(v, k)
}

/// ADSBexchange v2 / adsb.lol record -> normalized aircraft.
pub fn from_adsbx(v: &Value) -> Option<Aircraft> {
    let hex = as_hex(v, "hex")?;
    let lat = as_f64(v, "lat")?;
    let lon = as_f64(v, "lon")?;
    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
        return None;
    }
    let on_ground = ground_flag(v);
    let flags = v.get("dbFlags").and_then(Value::as_u64).unwrap_or(0);
    let mlat = v
        .get("mlat")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    let tisb = v
        .get("tisb")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    let source = as_text(v, "type").unwrap_or_else(|| {
        if mlat {
            "mlat".to_string()
        } else if tisb {
            "tisb".to_string()
        } else {
            "adsb".to_string()
        }
    });
    let squawk = as_text(v, "squawk");
    let registration = as_text(v, "r");
    let country = as_text(v, "country").or_else(|| {
        registration
            .as_deref()
            .and_then(country_from_registration)
            .map(str::to_string)
    });
    Some(Aircraft {
        callsign: clean_callsign(as_text(v, "flight")),
        registration,
        type_code: as_text(v, "t"),
        lat,
        lon,
        alt_ft: altitude(v, "alt_baro"),
        alt_geom_ft: altitude(v, "alt_geom"),
        nav_alt_ft: altitude(v, "nav_altitude_mcp"),
        on_ground,
        gs_kt: as_f64(v, "gs"),
        ias_kt: as_f64(v, "ias"),
        tas_kt: as_f64(v, "tas"),
        mach: as_f64(v, "mach"),
        track_deg: as_f64(v, "track"),
        mag_heading: as_f64(v, "mag_heading"),
        true_heading: as_f64(v, "true_heading"),
        vert_rate_fpm: as_i32(v, "baro_rate").or_else(|| as_i32(v, "geom_rate")),
        roll_deg: as_f64(v, "roll"),
        emergency: normalize_emergency(
            v.get("emergency").and_then(Value::as_str),
            squawk.as_deref(),
        ),
        category: as_text(v, "category"),
        military: flags & 0b0001 != 0,
        interesting: flags & 0b0010 != 0,
        pia: flags & 0b0100 != 0,
        ladd: flags & 0b1000 != 0,
        spi: v.get("spi").and_then(Value::as_u64).unwrap_or(0) == 1,
        source,
        seen_s: as_f64(v, "seen").unwrap_or(0.0),
        seen_pos_s: as_f64(v, "seen_pos"),
        messages: v.get("messages").and_then(Value::as_u64).unwrap_or(0),
        rssi: as_f64(v, "rssi"),
        dst_nm: as_f64(v, "dst"),
        dir_deg: as_f64(v, "dir"),
        country,
        hex,
        squawk,
    })
}

/// OpenSky `states/all` row -> normalized aircraft.
/// Row order: icao24, callsign, origin_country, time_position, last_contact,
/// longitude, latitude, baro_altitude(m), on_ground, velocity(m/s),
/// true_track, vertical_rate(m/s), sensors, geo_altitude(m), squawk, spi,
/// position_source.
pub fn from_opensky(row: &Value) -> Option<Aircraft> {
    let r = row.as_array()?;
    let get = |i: usize| r.get(i);
    let hex = get(0)?.as_str()?.trim().to_ascii_lowercase();
    if hex.len() != 6 {
        return None;
    }
    let lat = get(6)?.as_f64()?;
    let lon = get(5)?.as_f64()?;
    if !lat.is_finite() || !lon.is_finite() {
        return None;
    }
    let on_ground = get(8).and_then(Value::as_bool).unwrap_or(false);
    let alt_m = get(7).and_then(Value::as_f64);
    let geo_m = get(13).and_then(Value::as_f64);
    let vel = get(9).and_then(Value::as_f64);
    let vrate = get(11).and_then(Value::as_f64);
    let squawk = get(14)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let source = match get(16).and_then(Value::as_u64).unwrap_or(0) {
        0 => "adsb",
        1 => "asterix",
        2 => "mlat",
        3 => "flarm",
        _ => "opensky",
    };
    let callsign = clean_callsign(
        get(1)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
    );
    Some(Aircraft {
        hex,
        callsign,
        registration: None,
        type_code: None,
        lat,
        lon,
        alt_ft: alt_m.map(|m| (m / M_PER_FT).round() as i32),
        alt_geom_ft: geo_m.map(|m| (m / M_PER_FT).round() as i32),
        nav_alt_ft: None,
        on_ground,
        gs_kt: vel.map(|v| v * KT_PER_MPS),
        ias_kt: None,
        tas_kt: None,
        mach: None,
        track_deg: get(10).and_then(Value::as_f64),
        mag_heading: None,
        true_heading: None,
        vert_rate_fpm: vrate.map(|v| (v * FPM_PER_MPS).round() as i32),
        roll_deg: None,
        squawk: squawk.map(|s| s.to_string()),
        emergency: normalize_emergency(None, squawk),
        category: None,
        military: false,
        interesting: false,
        pia: false,
        ladd: false,
        spi: get(15).and_then(Value::as_bool).unwrap_or(false),
        source: source.to_string(),
        seen_s: 0.0,
        seen_pos_s: None,
        messages: 0,
        rssi: None,
        dst_nm: None,
        dir_deg: None,
        country: get(2)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
    })
}

fn parse_ac(v: &Value) -> Vec<Aircraft> {
    v.get("ac")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(from_adsbx).collect())
        .unwrap_or_default()
}

/* ------------------------------------------------------------ geometry */

/// Is this view wide enough that a few query circles cannot cover it?
pub fn is_wide_view(w: f64, s: f64, e: f64, n: f64) -> bool {
    (e - w).abs() >= 45.0 || (n - s).abs() >= 30.0
}

fn wrap_lon(lon: f64) -> f64 {
    (lon + 540.0) % 360.0 - 180.0
}

/// Choose where to point the provider so that what is on screen is what gets
/// fetched. Circles overlap by ~35% so tracks do not blink out at the seams.
pub fn cover(w: f64, s: f64, e: f64, n: f64, radius_nm: f64, max_circles: usize) -> Vec<Circle> {
    let radius = radius_nm.clamp(5.0, 250.0);
    let (s, n) = if s <= n { (s, n) } else { (n, s) };
    let s = s.clamp(-85.0, 85.0);
    let n = n.clamp(-85.0, 85.0);
    let lat_span = n - s;
    let lon_span = (e - w).abs().min(360.0);
    let clat = ((s + n) / 2.0).clamp(-85.0, 85.0);
    let r_lat = radius / 60.0;
    let r_lon = radius / (60.0 * clat.to_radians().cos().abs().max(0.08));

    let max_circles = max_circles.max(1);
    let mut nx = ((lon_span / (r_lon * 1.35)).ceil() as usize).max(1);
    let mut ny = ((lat_span / (r_lat * 1.35)).ceil() as usize).max(1);
    while nx * ny > max_circles {
        if nx >= ny && nx > 1 {
            nx -= 1;
        } else if ny > 1 {
            ny -= 1;
        } else {
            break;
        }
    }

    let mut out = Vec::with_capacity(nx * ny);
    for iy in 0..ny {
        let lat = if ny == 1 {
            (s + n) / 2.0
        } else {
            s + lat_span * iy as f64 / (ny - 1) as f64
        };
        for ix in 0..nx {
            let lon = if nx == 1 {
                (w + e) / 2.0
            } else {
                w + lon_span * ix as f64 / (nx - 1) as f64
            };
            out.push(Circle {
                lat,
                lon: wrap_lon(lon),
                radius_nm: radius,
            });
        }
    }
    out
}

/// Turn a viewport into work: a regional grid, plus a flag for the global
/// military sweep when the screen is too wide for the grid to mean much.
pub fn plan(w: f64, s: f64, e: f64, n: f64, radius_nm: f64, max_circles: usize) -> QueryPlan {
    let radius = radius_nm.clamp(5.0, 250.0);
    let lat_span = (n - s).abs().max(1e-6);
    let lon_span = (e - w).abs().max(1e-6);
    let clat = ((s + n) / 2.0).clamp(-85.0, 85.0);
    let need_x = (lon_span / (radius / (60.0 * clat.to_radians().cos().abs().max(0.08)) * 1.35))
        .ceil()
        .max(1.0) as usize;
    let need_y = (lat_span / (radius / 60.0 * 1.35)).ceil().max(1.0) as usize;
    let ideal = need_x.max(1) * need_y.max(1);
    let circles = cover(w, s, e, n, radius, max_circles);
    QueryPlan {
        global: is_wide_view(w, s, e, n) || ideal > max_circles.max(1) * 2,
        degraded: ideal > max_circles.max(1),
        circles,
    }
}

/// A square lattice of circles centred on a home point, used when nobody is
/// watching the air map. Spacing is 1.35 r so neighbouring circles overlap and
/// the home point itself always falls inside one of them — a naive 2x2 grid
/// placed at the corners of a box leaves a hole in the middle.
pub fn home_plan(lat: f64, lon: f64, radius_nm: f64, max_circles: usize) -> QueryPlan {
    let r = radius_nm.clamp(5.0, 250.0);
    let max = max_circles.max(1);
    let per_axis = ((max as f64).sqrt().ceil() as usize).max(1);
    let want = (per_axis * per_axis).min(max);
    let step_lat = 1.35 * r / 60.0;
    let step_lon = 1.35 * r / (60.0 * lat.to_radians().cos().abs().max(0.08));
    let mid = (per_axis as f64 - 1.0) / 2.0;

    let mut circles = Vec::with_capacity(want);
    'grid: for iy in 0..per_axis {
        for ix in 0..per_axis {
            if circles.len() >= want {
                break 'grid;
            }
            circles.push(Circle {
                lat: (lat + (iy as f64 - mid) * step_lat).clamp(-85.0, 85.0),
                lon: wrap_lon(lon + (ix as f64 - mid) * step_lon),
                radius_nm: r,
            });
        }
    }
    QueryPlan {
        circles,
        degraded: false,
        global: false,
    }
}

/* -------------------------------------------------------------- identity */

/// Country of registry from an ICAO address block or a tail number. Only
/// allocations that are unambiguous are listed; anything unknown stays blank
/// rather than guessing.
pub fn country_from_registration(reg: &str) -> Option<&'static str> {
    let r = reg.trim().to_ascii_uppercase();
    if r.is_empty() {
        return None;
    }
    // Longest-prefix wins: check two-character prefixes before one-character.
    const TWO: &[(&str, &str)] = &[
        ("G-", "United Kingdom"),
        ("HB", "Switzerland"),
        ("OE", "Austria"),
        ("OO", "Belgium"),
        ("PH", "Netherlands"),
        ("EI", "Ireland"),
        ("EC", "Spain"),
        ("CS", "Portugal"),
        ("CR", "Portugal"),
        ("CT", "Portugal"),
        ("SE", "Sweden"),
        ("LN", "Norway"),
        ("OY", "Denmark"),
        ("OH", "Finland"),
        ("TF", "Iceland"),
        ("SP", "Poland"),
        ("OK", "Czechia"),
        ("OM", "Slovakia"),
        ("HA", "Hungary"),
        ("LZ", "Bulgaria"),
        ("YR", "Romania"),
        ("TC", "Türkiye"),
        ("4X", "Israel"),
        ("A6", "United Arab Emirates"),
        ("A7", "Qatar"),
        ("HZ", "Saudi Arabia"),
        ("9K", "Kuwait"),
        ("EP", "Iran"),
        ("AP", "Pakistan"),
        ("VT", "India"),
        ("9V", "Singapore"),
        ("9M", "Malaysia"),
        ("HS", "Thailand"),
        ("VN", "Vietnam"),
        ("PK", "Indonesia"),
        ("JA", "Japan"),
        ("HL", "South Korea"),
        ("RP", "Philippines"),
        ("VH", "Australia"),
        ("ZK", "New Zealand"),
        ("ZS", "South Africa"),
        ("5Y", "Kenya"),
        ("5N", "Nigeria"),
        ("ET", "Ethiopia"),
        ("SU", "Egypt"),
        ("CN", "Morocco"),
        ("TS", "Tunisia"),
        ("7T", "Algeria"),
        ("XA", "Mexico"),
        ("YV", "Venezuela"),
        ("HK", "Colombia"),
        ("CC", "Chile"),
        ("LV", "Argentina"),
        ("PP", "Brazil"),
        ("PT", "Brazil"),
        ("PR", "Brazil"),
        ("PU", "Brazil"),
        ("RA", "Russia"),
        ("UR", "Ukraine"),
        ("EW", "Belarus"),
        ("9A", "Croatia"),
        ("S5", "Slovenia"),
        ("YU", "Serbia"),
    ];
    const ONE: &[(&str, &str)] = &[
        ("D", "Germany"),
        ("F", "France"),
        ("I", "Italy"),
        ("B", "China"),
        ("C", "Canada"),
        ("N", "United States"),
    ];
    for (p, c) in TWO {
        if r.starts_with(p) {
            return Some(c);
        }
    }
    // A tail number beginning with N is US only when a digit follows, so
    // "N123AB" matches while a stray "NO…" does not.
    if let Some(rest) = r.strip_prefix('N') {
        if rest
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
        {
            return Some("United States");
        }
    }
    for (p, c) in ONE {
        if r.starts_with(p) {
            return Some(c);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo;
    use serde_json::json;

    /// Distance in nautical miles, for the coverage assertions below.
    fn nm(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
        geo::haversine_m(lat1, lon1, lat2, lon2) / 1852.0
    }

    fn sample() -> Value {
        json!({
            "hex": "40643d", "type": "adsb_icao", "flight": "EZY21UM ",
            "r": "G-EZUC", "t": "A320",
            "alt_baro": 29775, "alt_geom": 30875, "gs": 396.5, "ias": 266,
            "tas": 310, "mach": 0.7, "track": 158.55, "mag_heading": 163.48,
            "true_heading": 164.12, "baro_rate": 1408, "geom_rate": 1312,
            "squawk": "0507", "emergency": "none", "category": "A3",
            "nav_altitude_mcp": 35008, "lat": 52.122299, "lon": -1.614456,
            "seen_pos": 0.434, "alert": 0, "spi": 0,
            "mlat": [], "tisb": [], "messages": 17125, "seen": 0.1, "rssi": -10.1,
            "dst": 58.297, "dir": 312.6
        })
    }

    #[test]
    fn parses_a_live_record() {
        let a = from_adsbx(&sample()).expect("record should parse");
        assert_eq!(a.hex, "40643d");
        assert_eq!(a.callsign.as_deref(), Some("EZY21UM"));
        assert_eq!(a.registration.as_deref(), Some("G-EZUC"));
        assert_eq!(a.type_code.as_deref(), Some("A320"));
        assert_eq!(a.alt_ft, Some(29775));
        assert_eq!(a.nav_alt_ft, Some(35008));
        assert_eq!(a.vert_rate_fpm, Some(1408));
        assert_eq!(a.squawk.as_deref(), Some("0507"));
        assert!(!a.on_ground);
        assert!(!a.military);
        assert!(!a.is_emergency());
        assert_eq!(a.source, "adsb_icao");
        assert_eq!(a.shape(), "air");
        assert_eq!(a.country.as_deref(), Some("United Kingdom"));
    }

    #[test]
    fn reads_ground_altitude_as_ground_not_altitude() {
        let v = json!({
            "hex": "407e32", "flight": "EAG1KW", "t": "AT76",
            "alt_baro": "ground", "gs": 6.2, "lat": 51.4, "lon": -0.4, "seen": 2.0
        });
        let a = from_adsbx(&v).unwrap();
        assert!(a.on_ground);
        assert_eq!(a.alt_ft, None, "\"ground\" must not parse as a number");
        assert_eq!(a.shape(), "ground");
    }

    #[test]
    fn decodes_dbflags() {
        let mut v = sample();
        v["dbFlags"] = json!(1);
        assert!(from_adsbx(&v).unwrap().military);
        v["dbFlags"] = json!(2);
        let a = from_adsbx(&v).unwrap();
        assert!(!a.military);
        assert!(a.interesting);
        v["dbFlags"] = json!(4 | 8);
        let a = from_adsbx(&v).unwrap();
        assert!(a.pia && a.ladd && !a.military);
    }

    #[test]
    fn emergency_field_and_squawks() {
        assert_eq!(normalize_emergency(Some("none"), Some("7000")), "");
        assert_eq!(normalize_emergency(None, Some("7500")), "unlawful");
        assert_eq!(normalize_emergency(None, Some("7600")), "nordo");
        assert_eq!(normalize_emergency(None, Some("7700")), "general");
        assert_eq!(
            normalize_emergency(Some("lifeguard"), Some("1200")),
            "lifeguard"
        );
        // the squawk is authoritative when both are present
        assert_eq!(
            normalize_emergency(Some("lifeguard"), Some("7700")),
            "general"
        );
        assert_eq!(emergency_severity("unlawful"), "high");
        assert_eq!(emergency_severity("lifeguard"), "info");
        let mut v = sample();
        v["squawk"] = json!("7700");
        let a = from_adsbx(&v).unwrap();
        assert!(a.is_emergency());
        assert_eq!(a.emergency, "general");
    }

    #[test]
    fn filler_callsigns_are_dropped_not_shown() {
        // seen live: an aircraft broadcasting "@@@@@@@@" as its callsign
        let mut v = sample();
        v["flight"] = json!("@@@@@@@@");
        let a = from_adsbx(&v).unwrap();
        assert_eq!(a.callsign, None, "garbage must not become a map label");
        assert_eq!(
            a.label(),
            "G-EZUC",
            "it should fall back to the tail number"
        );
        // short but real callsigns survive, padding gets trimmed
        v["flight"] = json!("  EZY21UM  ");
        assert_eq!(from_adsbx(&v).unwrap().callsign.as_deref(), Some("EZY21UM"));
        v["flight"] = json!("@");
        assert_eq!(from_adsbx(&v).unwrap().callsign, None);
    }

    #[test]
    fn rejects_records_without_a_position() {
        let mut v = sample();
        v["lat"] = json!(null);
        assert!(from_adsbx(&v).is_none());
        let mut v = sample();
        v["lon"] = json!(999.0);
        assert!(from_adsbx(&v).is_none());
        let mut v = sample();
        v["hex"] = json!("not-hex");
        assert!(from_adsbx(&v).is_none());
    }

    #[test]
    fn reads_the_military_feed_shape() {
        let v = json!({
            "hex": "ae5667", "flight": "SURF47  ", "t": "H60", "r": "12-20489",
            "alt_baro": 1775, "lat": 21.547989, "lon": -158.083324,
            "dbFlags": 1, "seen": 1.5, "category": "C2"
        });
        let a = from_adsbx(&v).unwrap();
        assert!(a.military);
        assert_eq!(a.callsign.as_deref(), Some("SURF47"));
        assert_eq!(a.shape(), "heli", "category C2 is a light rotorcraft");
        // and with no category at all it falls back to a generic airframe
        let mut plain = v.clone();
        plain.as_object_mut().unwrap().remove("category");
        assert_eq!(from_adsbx(&plain).unwrap().shape(), "air");
    }

    #[test]
    fn parse_ac_skips_bad_rows() {
        let v = json!({"ac": [sample(), json!({"hex":"abc","lat":1.0,"lon":2.0})], "total": 2});
        assert_eq!(parse_ac(&v).len(), 1);
    }

    #[test]
    fn covers_a_regional_viewport() {
        // ~2 degrees around London, 180 nm circles, at most 4 requests
        let (w, s, e, n) = (-2.0, 50.5, 1.0, 52.5);
        let circles = cover(w, s, e, n, 180.0, 4);
        assert!(!circles.is_empty() && circles.len() <= 4);
        for (la, lo) in [
            (s, w),
            (s, e),
            (n, w),
            (n, e),
            ((s + n) / 2.0, (w + e) / 2.0),
        ] {
            let inside = circles
                .iter()
                .any(|c| nm(la, lo, c.lat, c.lon) <= c.radius_nm * 1.02);
            assert!(inside, "corner {la},{lo} is not covered by {circles:?}");
        }
    }

    #[test]
    fn collapses_to_one_circle_when_the_view_is_small() {
        let circles = cover(-0.5, 51.4, 0.1, 51.6, 200.0, 6);
        assert_eq!(circles.len(), 1);
        assert!((circles[0].lat - 51.5).abs() < 0.2);
    }

    #[test]
    fn never_exceeds_the_request_budget() {
        for max in 1..6 {
            let circles = cover(-30.0, 10.0, 30.0, 60.0, 250.0, max);
            assert!(
                circles.len() <= max,
                "{} circles for max {max}",
                circles.len()
            );
        }
    }

    #[test]
    fn wide_views_ask_for_the_global_sweep() {
        let p = plan(-120.0, 10.0, 140.0, 60.0, 250.0, 8);
        assert!(p.global && p.degraded);
        let p = plan(-1.0, 51.0, 0.5, 52.0, 180.0, 8);
        assert!(!p.global && !p.degraded);
    }

    #[test]
    fn home_plan_always_covers_the_home_point() {
        for max in 1..=9 {
            let p = home_plan(51.47, -0.45, 180.0, max);
            assert!(!p.circles.is_empty() && p.circles.len() <= max);
            assert!(!p.global && !p.degraded);
            let covered = p
                .circles
                .iter()
                .any(|c| nm(51.47, -0.45, c.lat, c.lon) <= c.radius_nm);
            assert!(covered, "home not covered with max={max}: {:?}", p.circles);
        }
    }

    #[test]
    fn home_plan_neighbours_overlap_enough_to_see_the_gaps() {
        let r = 180.0;
        let p = home_plan(51.47, -0.45, r, 9);
        assert_eq!(p.circles.len(), 9);
        for a in &p.circles {
            let nearest = p
                .circles
                .iter()
                .filter(|b| !(b.lat == a.lat && b.lon == a.lon))
                .map(|b| nm(a.lat, a.lon, b.lat, b.lon))
                .fold(f64::INFINITY, f64::min);
            // sqrt(2)*r is the spacing at which the midpoint between two
            // circles stops being covered
            assert!(
                nearest <= 1.42 * r,
                "blind gap between circles: {nearest} nm"
            );
        }
    }

    #[test]
    fn tail_numbers_map_to_countries() {
        assert_eq!(country_from_registration("G-EZUC"), Some("United Kingdom"));
        assert_eq!(country_from_registration("N123AB"), Some("United States"));
        assert_eq!(country_from_registration("D-AIZY"), Some("Germany"));
        assert_eq!(country_from_registration("VT-ABC"), Some("India"));
        assert_eq!(country_from_registration("JA8088"), Some("Japan"));
        assert_eq!(country_from_registration("VH-OQA"), Some("Australia"));
        assert_eq!(country_from_registration(""), None);
        assert_eq!(country_from_registration("ZZZZ"), None);
    }

    #[test]
    fn opensky_rows_normalize_to_imperial() {
        let row = json!([
            "40643d",
            "EZY21UM ",
            "United Kingdom",
            1696000000,
            1696000001,
            -1.614456,
            52.122299,
            9075.0,
            false,
            204.0,
            158.55,
            4.3,
            [null],
            9400.0,
            "0507",
            false,
            0
        ]);
        let a = from_opensky(&row).unwrap();
        assert_eq!(a.hex, "40643d");
        assert_eq!(a.source, "adsb");
        assert_eq!(a.country.as_deref(), Some("United Kingdom"));
        assert!((a.alt_ft.unwrap() - 29774).abs() <= 2, "meters -> feet");
        assert!((a.gs_kt.unwrap() - 396.5).abs() < 1.0, "m/s -> knots");
        assert!((a.vert_rate_fpm.unwrap() - 846).abs() <= 2, "m/s -> ft/min");
    }

    #[test]
    fn emergency_tokens_have_readable_text() {
        assert!(emergency_text("unlawful").contains("7500"));
        assert!(emergency_text("nordo").contains("7600"));
        assert_eq!(emergency_text("none"), "");
    }

    #[test]
    fn budget_spends_a_burst_then_holds_to_the_sustained_rate() {
        let mut b = Budget::new(3.0, 40.0, Duration::from_millis(1000));
        assert!(b.wait_s() < 1.0, "a fresh bucket is spendable at once");
        assert!(b.take().is_none(), "the first request costs nothing");
        assert!(
            b.wait_s() >= 0.5,
            "the minimum gap now applies between requests"
        );

        // one token per window once the burst is gone
        let mut single = Budget::new(1.0, 40.0, Duration::from_millis(10));
        assert!(single.take().is_none());
        let wait = single.wait_s();
        assert!((wait - 40.0).abs() < 2.0, "expected ~40s, got {wait}");
    }

    #[test]
    fn rate_limit_penalty_doubles_then_recovers() {
        let mut b = Budget::new(3.0, 40.0, Duration::from_millis(1000));
        assert_eq!(b.penalize(), 20);
        assert!(b.wait_s() > 15.0, "a 429 must stop the client immediately");
        assert_eq!(b.penalize(), 40, "a repeat 429 doubles the hold-off");
        assert_eq!(b.penalize(), 80);
        for _ in 0..5 {
            b.penalize();
        }
        assert!(b.penalize() <= 300, "the hold-off is capped");
        // successes walk the penalty back down to nothing
        for _ in 0..10 {
            b.reward();
        }
        assert_eq!(b.penalty_s, 0.0);
    }

    #[test]
    fn a_penalised_bucket_does_not_hand_out_tokens() {
        let mut b = Budget::new(3.0, 40.0, Duration::from_millis(100));
        b.penalize();
        // even though the bucket is only empty on paper, the penalty wins
        assert!(b.take().is_some(), "no request may go out during a penalty");
    }

    #[test]
    fn nautical_mile_helper_matches_a_known_leg() {
        // London Heathrow to Paris CDG is ~187 nm
        let d = nm(51.4700, -0.4543, 49.0097, 2.5479);
        assert!((d - 187.0).abs() < 6.0, "got {d} nm");
    }
}
