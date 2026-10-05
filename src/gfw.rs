//! Global Fishing Watch API client (v3).
//!
//! Auth: a long-lived API token from https://globalfishingwatch.org/our-apis/tokens
//! sent as `Authorization: Bearer <token>` on every request. Set GFW_API_TOKEN
//! in the environment or .env. The GFW API is for non-commercial use.
//!
//! Endpoints used:
//!   GET  /v3/vessels/search        identity lookup by MMSI / IMO / call sign / name
//!   GET  /v3/vessels/{id}          full identity (registries, owners, authorizations)
//!   POST /v3/insights/vessels      fishing / AIS-gap / coverage / IUU insight
//!   POST /v3/events                fishing events inside a bounding region

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::Utc;
use serde_json::{json, Value};

use crate::config::GfwCfg;
use crate::model::GfwEvent;

/// GFW event datasets, keyed by the `types` value the API expects.
pub const EVENT_DATASETS: &[(&str, &str)] = &[
    ("FISHING", "public-global-fishing-events:latest"),
    ("ENCOUNTER", "public-global-encounters-events:latest"),
    ("LOITERING", "public-global-loitering-events:latest"),
    ("GAP", "public-global-gaps-events:latest"),
    ("PORT_VISIT", "public-global-port-visits-events:latest"),
];

/// Map requested event types to datasets, ignoring unknown names. An empty
/// request means "fishing", which is the historical default.
pub fn datasets_for(types: &[String]) -> (Vec<String>, Vec<String>) {
    let wanted: Vec<String> = if types.is_empty() {
        vec!["FISHING".to_string()]
    } else {
        types.iter().map(|t| t.to_ascii_uppercase()).collect()
    };
    let mut datasets = Vec::new();
    let mut accepted = Vec::new();
    for (name, dataset) in EVENT_DATASETS {
        if wanted.iter().any(|w| w == name) {
            datasets.push(dataset.to_string());
            accepted.push(name.to_string());
        }
    }
    if datasets.is_empty() {
        datasets.push(EVENT_DATASETS[0].1.to_string());
        accepted.push("FISHING".to_string());
    }
    (datasets, accepted)
}

pub struct GfwClient {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
    enabled: bool,
    ttl: Duration,
    cache: Mutex<HashMap<String, (Instant, Value)>>,
}

impl GfwClient {
    pub fn new(cfg: &GfwCfg) -> Self {
        let token = cfg
            .token
            .clone()
            .or_else(|| std::env::var("GFW_API_TOKEN").ok())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        let enabled = cfg.enabled && token.is_some();
        let http = reqwest::Client::builder()
            // /v3/events over a region can take ~60 s server-side.
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("OceanSentinel/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        GfwClient {
            http,
            base: cfg.base_url.trim_end_matches('/').to_string(),
            token,
            enabled,
            ttl: Duration::from_secs(cfg.cache_ttl_s.max(60)),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    fn cache_get(&self, key: &str) -> Option<Value> {
        let c = self.cache.lock().ok()?;
        let (at, v) = c.get(key)?;
        if at.elapsed() < self.ttl {
            Some(v.clone())
        } else {
            None
        }
    }

    fn cache_put(&self, key: &str, v: &Value) {
        if let Ok(mut c) = self.cache.lock() {
            if c.len() > 800 {
                c.clear();
            }
            c.insert(key.to_string(), (Instant::now(), v.clone()));
        }
    }

    async fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let token = self
            .token
            .clone()
            .ok_or_else(|| anyhow!("GFW_API_TOKEN is not set"))?;
        let mut req = self
            .http
            .request(method.clone(), url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/json");
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(anyhow!(
                "GFW request failed: {} {} -> {} {}",
                method,
                url,
                status.as_u16(),
                truncate(&text, 240)
            ));
        }
        Ok(value)
    }

    /// Identity search: MMSI, IMO, call sign or name.
    pub async fn search_vessel(&self, query: &str) -> Result<Value> {
        let key = format!("search:{}", query.to_lowercase());
        if let Some(v) = self.cache_get(&key) {
            return Ok(v);
        }
        let url = format!(
            "{}/v3/vessels/search?query={}&datasets[0]=public-global-vessel-identity:latest&limit=5\
             &includes[0]=OWNERSHIP&includes[1]=MATCH_CRITERIA&includes[2]=AUTHORIZATIONS",
            self.base,
            pct(query)
        );
        let v = self.request(reqwest::Method::GET, &url, None).await?;
        self.cache_put(&key, &v);
        Ok(v)
    }

    /// Full identity record for one vessel id.
    pub async fn vessel_detail(&self, id: &str) -> Result<Value> {
        let key = format!("detail:{id}");
        if let Some(v) = self.cache_get(&key) {
            return Ok(v);
        }
        let url = format!(
            "{}/v3/vessels/{}?dataset=public-global-vessel-identity:latest&registries-info-data=ALL",
            self.base,
            pct(id)
        );
        let v = self.request(reqwest::Method::GET, &url, None).await?;
        self.cache_put(&key, &v);
        Ok(v)
    }

    /// Fishing / AIS-gap / coverage / IUU insight for one vessel.
    pub async fn vessel_insights(&self, id: &str, days: i64) -> Result<Value> {
        let days = days.clamp(30, 730);
        let key = format!("insights:{id}:{days}");
        if let Some(v) = self.cache_get(&key) {
            return Ok(v);
        }
        let end = Utc::now().date_naive();
        let start = end - chrono::Duration::days(days);
        let body = json!({
            // datasetId is required by /v3/insights/vessels (422 without it).
            "vessels": [{ "vesselId": id, "datasetId": "public-global-vessel-identity:latest" }],
            "includes": ["FISHING", "GAP", "COVERAGE", "VESSEL-IDENTITY-IUU-VESSEL-LIST"],
            "startDate": format!("{}T00:00:00.000Z", start),
            "endDate": format!("{}T00:00:00.000Z", end + chrono::Duration::days(1)),
        });
        let url = format!("{}/v3/insights/vessels", self.base);
        let v = self
            .request(reqwest::Method::POST, &url, Some(body))
            .await?;
        self.cache_put(&key, &v);
        Ok(v)
    }

    /// Apparent activity events inside a bounding box, for the map layer.
    /// `types` selects which datasets to query (see `EVENT_DATASETS`).
    pub async fn events_bbox(
        &self,
        bbox: [f64; 4],
        days: i64,
        limit: u32,
        types: &[String],
    ) -> Result<(Vec<GfwEvent>, Value, Vec<String>)> {
        let days = days.clamp(1, 365);
        let limit = limit.clamp(1, 500);
        let [w, s, e, n] = bbox;
        let (datasets, accepted) = datasets_for(types);
        let key = format!(
            "events:{w:.3},{s:.3},{e:.3},{n:.3}:{days}:{limit}:{}",
            accepted.join("+")
        );
        if let Some(v) = self.cache_get(&key) {
            return Ok((parse_events(&v), v, accepted));
        }
        let end = Utc::now().date_naive();
        let start = end - chrono::Duration::days(days);
        let body = json!({
            "datasets": datasets,
            "startDate": start.to_string(),
            "endDate": (end + chrono::Duration::days(1)).to_string(),
            "geometry": {
                "type": "Polygon",
                "coordinates": [[[w, s], [e, s], [e, n], [w, n], [w, s]]]
            },
            "types": accepted,
            "limit": limit,
        });
        let url = format!("{}/v3/events?limit={}&offset=0", self.base, limit);
        let v = self
            .request(reqwest::Method::POST, &url, Some(body))
            .await?;
        self.cache_put(&key, &v);
        Ok((parse_events(&v), v, accepted))
    }
}

pub fn parse_events(v: &Value) -> Vec<GfwEvent> {
    v.pointer("/entries")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let lat = e.pointer("/position/lat")?.as_f64()?;
                    let lon = e.pointer("/position/lon")?.as_f64()?;
                    Some(GfwEvent {
                        id: e
                            .get("id")
                            .and_then(|x| x.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        event_type: e
                            .get("type")
                            .and_then(|x| x.as_str())
                            .unwrap_or("fishing")
                            .to_string(),
                        lat,
                        lon,
                        start: e.get("start").and_then(|x| x.as_str()).map(str::to_string),
                        end: e.get("end").and_then(|x| x.as_str()).map(str::to_string),
                        vessel_id: e
                            .pointer("/vessel/id")
                            .and_then(|x| x.as_str())
                            .map(str::to_string),
                        vessel_name: e
                            .pointer("/vessel/name")
                            .and_then(|x| x.as_str())
                            .map(str::to_string),
                        flag: e
                            .pointer("/vessel/flag")
                            .and_then(|x| x.as_str())
                            .map(str::to_string),
                        ssvid: e
                            .pointer("/vessel/ssvid")
                            .and_then(|x| x.as_str())
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Flatten a vessel-identity payload into the fields the UI shows. Accepts
/// either a search response (`{entries:[…]}`), a single search entry, or a
/// `/v3/vessels/{id}` detail object — they share the entry-level shape.
pub fn summarize_search_entry(input: &Value) -> Value {
    let entry = input.pointer("/entries/0").unwrap_or(input);
    let sri = entry.pointer("/selfReportedInfo/0").unwrap_or(&Value::Null);
    let combined = entry
        .pointer("/combinedSourcesInfo/0")
        .unwrap_or(&Value::Null);
    let registry = entry.pointer("/registryInfo/0").unwrap_or(&Value::Null);
    let owner = entry.pointer("/registryOwners/0").unwrap_or(&Value::Null);

    let s = |v: &Value, k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|x| !x.is_empty() && *x != "NULL")
            .map(str::to_string)
    };
    // geartypes/shiptypes are arrays of {name, source, yearFrom, yearTo}
    let first_name = |v: &Value, k: &str| {
        v.get(k)
            .and_then(|x| x.as_array())
            .and_then(|a| a.first())
            .and_then(|o| o.get("name"))
            .and_then(|x| x.as_str())
            .map(str::to_string)
    };

    json!({
        "vessel_id": s(combined, "vesselId").or_else(|| s(sri, "id")).or_else(|| s(registry, "id")),
        "name": s(sri, "shipname").or_else(|| s(registry, "shipname")),
        "flag": s(sri, "flag").or_else(|| s(registry, "flag")),
        "mmsi": s(sri, "ssvid").or_else(|| s(registry, "ssvid")),
        "imo": s(sri, "imo").or_else(|| s(registry, "imo")),
        "callsign": s(sri, "callsign").or_else(|| s(registry, "callsign")),
        "geartype": s(sri, "geartype")
            .or_else(|| first_name(combined, "geartypes"))
            .or_else(|| first_name(registry, "geartypes")),
        "shiptype": s(sri, "shiptype")
            .or_else(|| first_name(combined, "shiptypes"))
            .or_else(|| first_name(registry, "shiptypes")),
        "registry": s(registry, "sourceCode").or_else(|| s(registry, "registry")),
        "owner": s(owner, "name")
            .or_else(|| s(owner, "ownerName"))
            .or_else(|| s(owner, "companyName")),
        "transmission_from": s(sri, "transmissionDateFrom"),
        "transmission_to": s(sri, "transmissionDateTo"),
        "positions": sri.get("positionsCounter").and_then(|x| x.as_u64()),
        "dataset": entry.get("dataset").and_then(|x| x.as_str()),
    })
}

/// Flatten a vessel-insights response. The live endpoint returns a bare object
/// (`{period, gap, coverage, apparentFishing, vesselIdentity}`), so accept that
/// as well as an `entries[0]` wrapper.
pub fn summarize_insights(v: &Value) -> Value {
    let entry = v.pointer("/entries/0").unwrap_or(v);
    let fishing = entry.get("apparentFishing").cloned().unwrap_or(Value::Null);
    let coverage = entry.get("coverage").cloned().unwrap_or(Value::Null);
    let gaps = entry
        .get("gap")
        .or_else(|| entry.get("gaps"))
        .cloned()
        .unwrap_or(Value::Null);
    let identity = entry.get("vesselIdentity").cloned().unwrap_or(Value::Null);
    let counters = fishing
        .get("periodSelectedCounters")
        .cloned()
        .unwrap_or(Value::Null);
    let gap_counters = gaps
        .get("periodSelectedCounters")
        .cloned()
        .unwrap_or(Value::Null);
    let iuu = identity
        .get("iuuVesselList")
        .cloned()
        .unwrap_or(Value::Null);
    let flags_changes = identity.get("flagsChanges").cloned().unwrap_or(Value::Null);
    json!({
        "fishing_events": counters.get("events").and_then(|x| x.as_u64()),
        "fishing_events_in_no_take_mpas": counters.get("eventsInNoTakeMPAs").and_then(|x| x.as_u64()),
        "fishing_events_in_rfmo_without_authorization": counters.get("eventsInRFMOWithoutKnownAuthorization").and_then(|x| x.as_u64()),
        "ais_coverage_percentage": coverage.get("percentage").and_then(|x| x.as_f64()),
        "ais_gap_events": gap_counters.get("events").and_then(|x| x.as_u64()),
        "ais_off_periods": gaps.get("aisOff").and_then(|x| x.as_array()).map(|a| a.len()).unwrap_or(0),
        "iuu_listed": iuu_listed(&iuu),
        "flag_changes": flags_changes.as_array().map(|a| a.len()).unwrap_or(0),
    })
}

/// `iuuVesselList` is an object like
/// `{"totalTimesListed": 0, "totalTimesListedInThePeriod": 0, "valuesInThePeriod": []}`.
/// Never infer listing from mere presence — a false IUU badge would be a
/// damaging accusation, so require an explicit positive signal.
fn iuu_listed(iuu: &Value) -> bool {
    if iuu.is_null() {
        return false;
    }
    let count = |k: &str| iuu.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let non_empty = |k: &str| {
        iuu.get(k)
            .and_then(|x| x.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    };
    count("totalTimesListed") > 0
        || count("totalTimesListedInThePeriod") > 0
        || non_empty("valuesInThePeriod")
        || iuu.as_array().map(|a| !a.is_empty()).unwrap_or(false)
}

fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_dataset_selection() {
        // default is fishing
        let (ds, accepted) = datasets_for(&[]);
        assert_eq!(accepted, vec!["FISHING"]);
        assert_eq!(ds, vec!["public-global-fishing-events:latest"]);

        // multiple types, case-insensitive, unknown names ignored
        let (ds, accepted) = datasets_for(&[
            "fishing".to_string(),
            "encounter".to_string(),
            "gap".to_string(),
            "bogus".to_string(),
        ]);
        assert_eq!(accepted, vec!["FISHING", "ENCOUNTER", "GAP"]);
        assert_eq!(ds.len(), 3);
        assert!(ds.iter().any(|d| d.contains("encounters")));

        // all-unknown falls back to fishing rather than querying nothing
        let (ds, accepted) = datasets_for(&["nonsense".to_string()]);
        assert_eq!(accepted, vec!["FISHING"]);
        assert_eq!(ds.len(), 1);
    }

    #[test]
    fn parses_event_entries() {
        let v: Value = serde_json::from_str(
            r#"{"entries":[{"id":"abc","type":"fishing","start":"2026-01-01T00:00:00Z","position":{"lat":36.1,"lon":-5.2},"vessel":{"id":"vid","name":"TEST","flag":"ESP","ssvid":"123456789"}}],"total":1}"#,
        )
        .unwrap();
        let evs = parse_events(&v);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].vessel_name.as_deref(), Some("TEST"));
        assert!((evs[0].lat - 36.1).abs() < 1e-9);
    }

    #[test]
    fn summarizes_search_entry() {
        // Real live shape: a search response wrapper, with gear/ship types as
        // arrays of objects inside combinedSourcesInfo.
        let v: Value = serde_json::from_str(
            r#"{"entries":[{
                "dataset":"public-global-vessel-identity:v4.0",
                "combinedSourcesInfo":[{"vesselId":"e9efd1586","geartypes":[{"name":"SQUID_JIGGER"}],"shiptypes":[{"name":"FISHING"}]}],
                "selfReportedInfo":[{"id":"9b3e","ssvid":"701000948","shipname":"CLAUDINA","flag":"ARG","imo":"7831410","callsign":"LW3058","transmissionDateFrom":"2016-12-20T20:27:39Z"}],
                "registryInfo":[{"id":"r1","sourceCode":"ARG","shipname":"CLAUDINA","geartypes":[{"name":"SQUID_JIGGER"}]}],
                "registryOwners":[{"name":"PESQUERA EJEMPLO SA"}]
            }],"total":1}"#,
        )
        .unwrap();
        let s = summarize_search_entry(&v);
        assert_eq!(s["name"], "CLAUDINA");
        assert_eq!(s["vessel_id"], "e9efd1586");
        assert_eq!(s["mmsi"], "701000948");
        assert_eq!(s["flag"], "ARG");
        assert_eq!(s["imo"], "7831410");
        assert_eq!(s["geartype"], "SQUID_JIGGER");
        assert_eq!(s["shiptype"], "FISHING");
        assert_eq!(s["registry"], "ARG");
        assert_eq!(s["owner"], "PESQUERA EJEMPLO SA");
    }

    #[test]
    fn summarizes_insights_bare_object() {
        // /v3/insights/vessels returns a bare object, not an entries wrapper.
        let v: Value = serde_json::from_str(
            r#"{"period":{"startDate":"2025-10-03","endDate":"2026-10-04"},
                "gap":{"periodSelectedCounters":{"events":3,"eventsGapOff":2},"aisOff":[{"a":1},{"a":2}]},
                "coverage":{"percentage":76.4},
                "apparentFishing":{"periodSelectedCounters":{"events":12,"eventsInNoTakeMPAs":1,"eventsInRFMOWithoutKnownAuthorization":4}},
                "vesselIdentity":{"iuuVesselList":{"totalTimesListed":0,"totalTimesListedInThePeriod":0,"valuesInThePeriod":[]},"flagsChanges":[{"a":1}]}}"#,
        )
        .unwrap();
        let s = summarize_insights(&v);
        assert_eq!(s["fishing_events"], 12);
        assert_eq!(s["fishing_events_in_no_take_mpas"], 1);
        assert_eq!(s["fishing_events_in_rfmo_without_authorization"], 4);
        assert_eq!(s["ais_gap_events"], 3);
        assert_eq!(s["ais_off_periods"], 2);
        assert_eq!(s["flag_changes"], 1);
        assert_eq!(
            s["iuu_listed"], false,
            "an object with zero listings must not be reported as listed"
        );
        assert!((s["ais_coverage_percentage"].as_f64().unwrap() - 76.4).abs() < 0.01);
    }

    #[test]
    fn detects_iuu_listing_only_on_positive_signal() {
        let listed: Value = serde_json::from_str(
            r#"{"totalTimesListed":2,"totalTimesListedInThePeriod":1,"valuesInThePeriod":[{"rfmo":"CCAMLR"}]}"#,
        )
        .unwrap();
        assert!(iuu_listed(&listed));
        let clear: Value = serde_json::from_str(
            r#"{"totalTimesListed":0,"totalTimesListedInThePeriod":0,"valuesInThePeriod":[]}"#,
        )
        .unwrap();
        assert!(!iuu_listed(&clear));
        assert!(!iuu_listed(&Value::Null));
        let missing: Value = serde_json::from_str(r#"{}"#).unwrap();
        assert!(!iuu_listed(&missing));
    }
}
