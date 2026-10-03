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
    pub trail: Vec<TrailPoint>,
    pub confidence: f32,
    /// True while the target is tracked by sonar/lidar but transmits no AIS.
    pub dark: bool,
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
            trail: Vec::new(),
            confidence: c.confidence,
            dark: c.mmsi.is_none(),
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
            trail: Vec::new(),
            confidence: 0.6,
            dark: false,
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

/// Map an AIS ship-and-cargo type code to a coarse class used for icons/filters.
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
