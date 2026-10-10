use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SensorKind {
    Ais,
    Sonar,
    Lidar,
    Radar,
}

impl SensorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SensorKind::Ais => "ais",
            SensorKind::Sonar => "sonar",
            SensorKind::Lidar => "lidar",
            SensorKind::Radar => "radar",
        }
    }
}

/// A single detection from one sensor: absolute position plus optional provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub source: SensorKind,
    pub lat: f64,
    pub lon: f64,
    pub mmsi: Option<u32>,
    pub label: Option<String>,
    pub range_m: Option<f64>,
    pub bearing_deg: Option<f64>,
    /// Target speed/course when the sensor reports them (NMEA TTM does).
    pub sog_kn: Option<f32>,
    pub cog_deg: Option<f32>,
    /// Closest point of approach / time to it, when the tracker provides them.
    pub cpa_m: Option<f64>,
    pub tcpa_min: Option<f64>,
    pub confidence: f32,
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrailPoint {
    pub lat: f64,
    pub lon: f64,
    pub ts: DateTime<Utc>,
}

/// A fused vessel track: the union of everything we know about one target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub mmsi: Option<u32>,
    pub imo: Option<u32>,
    pub name: Option<String>,
    pub callsign: Option<String>,
    pub ship_type: Option<u8>,
    pub classification: String,
    pub lat: f64,
    pub lon: f64,
    pub sog: Option<f32>,
    pub cog: Option<f32>,
    pub heading: Option<f32>,
    pub nav_status: Option<u8>,
    pub destination: Option<String>,
    pub draught: Option<f32>,
    pub length: Option<u16>,
    pub beam: Option<u16>,
    pub sources: Vec<SensorKind>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// Last position received over AIS specifically (AIS-lost detection must not
    /// be reset by sonar/LiDAR contacts).
    pub last_ais: Option<DateTime<Utc>>,
    pub last_sensor_contact: Option<DateTime<Utc>>,
    /// Closest point of approach and time to it, as reported by a target
    /// tracker (NMEA TTM). Only populated for own-ship sensor targets.
    pub cpa_m: Option<f64>,
    pub tcpa_min: Option<f64>,
    pub trail: Vec<TrailPoint>,
    pub confidence: f32,
    /// True while the target is tracked by sonar/lidar but transmits no AIS.
    pub dark: bool,
    /// Naval vessel: AIS ship type 35 (military operations) or a military name
    /// prefix (USS/HMS/CNS/INS/JS/…). Set from the identity the vessel itself
    /// broadcasts; never inferred from position or behaviour.
    #[serde(default)]
    pub naval: bool,
    /// Aircraft carrier, from an exact vessel-name match. A carrier is always
    /// naval; the reverse is not true.
    #[serde(default)]
    pub carrier: bool,
    /// Whether the carrier alert has already been raised for this track.
    #[serde(default)]
    pub carrier_alerted: bool,
    /// True when an AIS track is independently confirmed by a physical sensor.
    pub corroborated: bool,
    /// Cached Global Fishing Watch enrichment for this vessel.
    pub gfw: Option<serde_json::Value>,
    pub gfw_at: Option<DateTime<Utc>>,
}

impl Track {
    pub fn new_sensor(id: String, c: &Contact, classification: &str) -> Self {
        Track {
            id,
            mmsi: c.mmsi,
            imo: None,
            name: c.label.clone(),
            callsign: None,
            ship_type: None,
            classification: classification.to_string(),
            lat: c.lat,
            lon: c.lon,
            sog: None,
            cog: None,
            heading: None,
            nav_status: None,
            destination: None,
            draught: None,
            length: None,
            beam: None,
            sources: vec![c.source],
            first_seen: c.ts,
            last_seen: c.ts,
            last_ais: None,
            last_sensor_contact: Some(c.ts),
            cpa_m: c.cpa_m,
            tcpa_min: c.tcpa_min,
            trail: Vec::new(),
            confidence: c.confidence,
            dark: c.mmsi.is_none(),
            naval: false,
            carrier: false,
            carrier_alerted: false,
            corroborated: false,
            gfw: None,
            gfw_at: None,
        }
    }

    pub fn new_ais(mmsi: u32, lat: f64, lon: f64, ts: DateTime<Utc>) -> Self {
        Track {
            id: format!("ais:{mmsi}"),
            mmsi: Some(mmsi),
            imo: None,
            name: None,
            callsign: None,
            ship_type: None,
            classification: "unknown".to_string(),
            lat,
            lon,
            sog: None,
            cog: None,
            heading: None,
            nav_status: None,
            destination: None,
            draught: None,
            length: None,
            beam: None,
            sources: vec![SensorKind::Ais],
            first_seen: ts,
            last_seen: ts,
            last_ais: Some(ts),
            last_sensor_contact: None,
            cpa_m: None,
            tcpa_min: None,
            trail: Vec::new(),
            confidence: 0.6,
            dark: false,
            naval: false,
            carrier: false,
            carrier_alerted: false,
            corroborated: false,
            gfw: None,
            gfw_at: None,
        }
    }

    pub fn add_source(&mut self, k: SensorKind) {
        if !self.sources.contains(&k) {
            self.sources.push(k);
            self.sources.sort();
        }
    }

    pub fn has_ais(&self) -> bool {
        self.sources.contains(&SensorKind::Ais)
    }

    pub fn sensor_sources(&self) -> Vec<SensorKind> {
        self.sources
            .iter()
            .copied()
            .filter(|s| *s != SensorKind::Ais)
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub id: String,
    pub ts: DateTime<Utc>,
    pub kind: String,
    pub severity: String,
    pub message: String,
    pub track_id: Option<String>,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Zone {
    pub id: String,
    pub name: String,
    /// GeoJSON-order vertices: [lon, lat]
    pub polygon: Vec<[f64; 2]>,
    pub color: String,
    pub created: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedStatus {
    pub name: String,
    pub kind: String,
    pub state: String,
    pub detail: String,
    pub lines: u64,
    pub pps: f32,
    pub last_line: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OwnShipFix {
    pub lat: f64,
    pub lon: f64,
    pub sog: Option<f32>,
    pub cog: Option<f32>,
    pub heading: Option<f32>,
    pub ts: Option<DateTime<Utc>>,
}

/// One Global Fishing Watch fishing event (used as a map layer).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GfwEvent {
    pub id: String,
    pub event_type: String,
    pub lat: f64,
    pub lon: f64,
    pub start: Option<String>,
    pub end: Option<String>,
    pub vessel_id: Option<String>,
    pub vessel_name: Option<String>,
    pub flag: Option<String>,
    pub ssvid: Option<String>,
}

/// One watchlist entry: matched against live tracks by MMSI, IMO or exact name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchEntry {
    /// Filled in automatically when omitted from config.toml.
    #[serde(default)]
    pub id: String,
    pub mmsi: Option<u32>,
    pub imo: Option<u32>,
    pub name: Option<String>,
    pub note: Option<String>,
    #[serde(default = "Utc::now")]
    pub added: DateTime<Utc>,
}

impl WatchEntry {
    pub fn label(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.mmsi.map(|m| format!("MMSI {m}")))
            .or_else(|| self.imo.map(|i| format!("IMO {i}")))
            .unwrap_or_else(|| self.id.clone())
    }

    pub fn matches_track(&self, t: &Track) -> bool {
        if let Some(m) = self.mmsi {
            if t.mmsi == Some(m) {
                return true;
            }
        }
        if let Some(i) = self.imo {
            if t.imo == Some(i) {
                return true;
            }
        }
        if let Some(n) = self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            if let Some(tn) = t.name.as_deref() {
                if tn.trim().eq_ignore_ascii_case(n) {
                    return true;
                }
            }
        }
        false
    }
}

/// Leading prefixes navies broadcast in the AIS name field.
const NAVAL_PREFIXES: &[&str] = &[
    "USS ", "USNS ", "HMS ", "HMCS ", "HMNZS ", "HMAS ", "HMSN ", "FS ", "FNS ", "CNS ", "INS ",
    "JS ", "JDS ", "ITS ", "HNLMS ", "HDMS ", "HSwMS ", "NOR ",
];

/// Carrier names, matched on the vessel name alone. Exact matches only, after
/// stripping a parenthetical hull number and any naval prefix, so a liner
/// called "QUEEN ELIZABETH 2" is not reported as a carrier while
/// "USS NIMITZ (CVN-68)" is.
pub fn is_carrier_name(name: &str) -> bool {
    let cleaned = normalise_vessel_name(name);
    CARRIER_NAMES.contains(&cleaned.as_str())
}

/// True when an AIS name looks naval (military prefix). Deliberately
/// conservative: an unlisted name is simply not flagged.
pub fn is_naval_name(name: &str) -> bool {
    let upper = name.trim().to_ascii_uppercase();
    NAVAL_PREFIXES.iter().any(|p| upper.starts_with(p))
}

/// Uppercase, drop parentheticals, drop a naval prefix, collapse punctuation.
fn normalise_vessel_name(name: &str) -> String {
    let upper = name.trim().to_ascii_uppercase();
    let without_paren = match upper.find('(') {
        Some(i) => upper[..i].to_string(),
        None => upper,
    };
    let mut out = without_paren.trim().to_string();
    for p in NAVAL_PREFIXES {
        if let Some(rest) = out.strip_prefix(p) {
            out = rest.trim().to_string();
            break;
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

const CARRIER_NAMES: &[&str] = &[
    // United States (CVN-68 … CVN-79)
    "NIMITZ",
    "DWIGHT D EISENHOWER",
    "CARL VINSON",
    "THEODORE ROOSEVELT",
    "ABRAHAM LINCOLN",
    "GEORGE WASHINGTON",
    "JOHN C STENNIS",
    "HARRY S TRUMAN",
    "RONALD REAGAN",
    "GEORGE HW BUSH",
    "GERALD R FORD",
    "JOHN F KENNEDY",
    "ENTERPRISE",
    // United Kingdom
    "QUEEN ELIZABETH",
    "PRINCE OF WALES",
    // France, Italy, Spain
    "CHARLES DE GAULLE",
    "CAVOUR",
    "GIUSEPPE GARIBALDI",
    "TRIESTE",
    "JUAN CARLOS I",
    // Russia, China, India
    "ADMIRAL KUZNETSOV",
    "LIAONING",
    "SHANDONG",
    "FUJIAN",
    "VIKRANT",
    "VIKRAMADITYA",
    // Japan, South Korea, Turkey, Thailand, Brazil
    "IZUMO",
    "KAGA",
    "DOKDO",
    "MARADO",
    "ANADOLU",
    "CHAKRI NARUEBET",
    "ATLANTICO",
];

/// Best-effort vessel class from a tracker label. Marine target trackers
/// transcribe names with their type prefix, so this recovers something useful
/// about a contact that carries no AIS identity at all.
pub fn classify_from_label(label: &str) -> &'static str {
    let l = label.trim_start().to_ascii_uppercase();
    for (prefix, class) in [
        ("F/V", "fishing"),
        ("FV ", "fishing"),
        ("M/V", "cargo"),
        ("MV ", "cargo"),
        ("M/T", "tanker"),
        ("MT ", "tanker"),
        ("TUG", "towing"),
        ("P/V", "patrol"),
        ("PV ", "patrol"),
        ("SY ", "sailing"),
    ] {
        if l.starts_with(prefix) {
            return class;
        }
    }
    "unknown"
}

/// Ordinal ranking for severity strings ("high" > "medium" > "info").
pub fn severity_rank(s: &str) -> u8 {
    match s {
        "high" => 2,
        "medium" => 1,
        _ => 0,
    }
}

/// Map an AIS ship-and-cargo type code to a coarse class used for icons/filters.
/// Classification for a track, upgraded to "carrier" when the name says so.
pub fn classify_with_name(name: Option<&str>, ship_type: Option<u8>) -> &'static str {
    if let Some(n) = name {
        if is_carrier_name(n) {
            return "carrier";
        }
    }
    match ship_type {
        Some(t) => classify_ship_type(t),
        None => "unknown",
    }
}

pub fn classify_ship_type(t: u8) -> &'static str {
    match t {
        30 => "fishing",
        31 | 32 | 52 => "towing",
        33 => "dredging",
        34 => "diving",
        35 => "military",
        36 => "sailing",
        37 => "pleasure",
        40..=49 => "highspeed",
        50 => "pilot",
        51 => "sar",
        53 => "port",
        55 => "patrol",
        58 => "medical",
        60..=69 => "passenger",
        70..=79 => "cargo",
        80..=89 => "tanker",
        90..=99 => "other",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carriers_are_matched_on_exact_names_only() {
        assert!(is_carrier_name("USS NIMITZ (CVN-68)"));
        assert!(is_carrier_name("NIMITZ"));
        assert!(is_carrier_name("hms queen elizabeth"));
        assert!(is_carrier_name("CHARLES DE GAULLE"));
        assert!(is_carrier_name("LIAONING"));
        assert!(is_carrier_name("INS VIKRANT"));
        // a liner that merely shares a name is not a carrier
        assert!(!is_carrier_name("QUEEN ELIZABETH 2"));
        assert!(!is_carrier_name("QUEEN MARY 2"));
        assert!(!is_carrier_name("SHANDONG EXPRESS"));
        assert!(!is_carrier_name(""));
        assert!(!is_carrier_name("EVER GIVEN"));
    }

    #[test]
    fn naval_names_need_a_military_prefix() {
        assert!(is_naval_name("USS ARLEIGH BURKE"));
        assert!(is_naval_name("HMS DARING"));
        assert!(is_naval_name("CNS SHANDONG"));
        assert!(is_naval_name("INS KOCHI"));
        assert!(!is_naval_name("MAERSK EMDEN"));
        assert!(!is_naval_name("MV BALTIC TRADER"));
    }

    #[test]
    fn a_carrier_classification_wins_over_the_type_code() {
        assert_eq!(
            classify_with_name(Some("USS GERALD R FORD"), Some(70)),
            "carrier"
        );
        assert_eq!(
            classify_with_name(Some("USS NIMITZ (CVN-68)"), None),
            "carrier"
        );
        assert_eq!(classify_with_name(Some("EVER GIVEN"), Some(70)), "cargo");
        assert_eq!(classify_with_name(None, Some(35)), "military");
        assert_eq!(classify_with_name(None, None), "unknown");
    }

    #[test]
    fn classifies_sensor_labels() {
        assert_eq!(classify_from_label("F/V ATLANTIC DAWN"), "fishing");
        assert_eq!(classify_from_label("MV BALTIC TRADER"), "cargo");
        assert_eq!(classify_from_label("MT PACIFIC STAR"), "tanker");
        assert_eq!(classify_from_label("TUG ADRIATIC TRADER"), "towing");
        assert_eq!(classify_from_label("P/V LEVANT HORIZON"), "patrol");
        assert_eq!(classify_from_label("DARK-03"), "unknown");
        assert_eq!(classify_from_label(""), "unknown");
    }
}
