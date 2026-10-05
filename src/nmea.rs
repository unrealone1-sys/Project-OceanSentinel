//! NMEA 0183 parsing: sentence framing, own-ship navigation sentences,
//! and the target sentences marine sonar / radar trackers emit
//! (`TLL` = target lat/lon, `TTM` = tracked target range/bearing).

use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct Sentence {
    pub raw: String,
    pub talker: String,
    pub kind: String,
    pub fields: Vec<String>,
    pub checksum_ok: bool,
}

/// XOR checksum over the sentence body (between the start char and '*').
pub fn checksum(body: &str) -> u8 {
    body.bytes().fold(0u8, |a, b| a ^ b)
}

pub fn parse(line: &str) -> Option<Sentence> {
    let line = line.trim();
    let start = line.find(['$', '!'])?;
    let line = &line[start..];
    let star = line.find('*');
    let body_end = star.unwrap_or(line.len());
    if body_end < 4 {
        return None;
    }
    let body = &line[1..body_end];
    let checksum_ok = match star {
        Some(i) => {
            let given = line
                .get(i + 1..)
                .map(|s| s.trim())
                .and_then(|s| u8::from_str_radix(s, 16).ok())
                .unwrap_or(0);
            given == checksum(body)
        }
        None => false,
    };
    let mut parts = body.split(',');
    let head = parts.next()?;
    let (talker, kind) = if head.starts_with('P') {
        ("P".to_string(), head.get(1..)?.to_string())
    } else {
        if head.len() < 4 {
            return None;
        }
        (head.get(0..2)?.to_string(), head.get(2..)?.to_string())
    };
    let fields: Vec<String> = parts.map(|s| s.trim().to_string()).collect();
    Some(Sentence {
        raw: line.to_string(),
        talker,
        kind,
        fields,
        checksum_ok,
    })
}

/// "ddmm.mmmm" / "dddmm.mmmm" -> decimal degrees.
pub fn dm_to_deg(v: &str) -> Option<f64> {
    let x: f64 = v.trim().parse().ok()?;
    let deg = (x / 100.0).trunc();
    let min = x - deg * 100.0;
    Some(deg + min / 60.0)
}

pub fn lat_lon(lat_s: &str, ns: &str, lon_s: &str, ew: &str) -> Option<(f64, f64)> {
    let mut lat = dm_to_deg(lat_s)?;
    let mut lon = dm_to_deg(lon_s)?;
    if ns.to_ascii_uppercase().starts_with('S') {
        lat = -lat;
    }
    if ew.to_ascii_uppercase().starts_with('W') {
        lon = -lon;
    }
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    Some((lat, lon))
}

fn f(fields: &[String], i: usize) -> Option<f64> {
    fields.get(i).and_then(|s| s.trim().parse::<f64>().ok())
}

/// Own-ship navigation state, updated from GGA / GLL / RMC / HDT / VTG.
#[derive(Debug, Clone, Default)]
pub struct OwnShipState {
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub sog: Option<f32>,
    pub cog: Option<f32>,
    pub heading: Option<f32>,
    pub updated: Option<DateTime<Utc>>,
}

impl OwnShipState {
    pub fn update(&mut self, s: &Sentence, ts: DateTime<Utc>) -> bool {
        match s.kind.as_str() {
            "GGA" => {
                // 0 time, 1 lat, 2 N/S, 3 lon, 4 E/W, ...
                if let Some((lat, lon)) = lat_lon(
                    s.fields.get(1).map(String::as_str).unwrap_or(""),
                    s.fields.get(2).map(String::as_str).unwrap_or(""),
                    s.fields.get(3).map(String::as_str).unwrap_or(""),
                    s.fields.get(4).map(String::as_str).unwrap_or(""),
                ) {
                    self.lat = Some(lat);
                    self.lon = Some(lon);
                    self.updated = Some(ts);
                    return true;
                }
                false
            }
            "GLL" => {
                if let Some((lat, lon)) = lat_lon(
                    s.fields.first().map(String::as_str).unwrap_or(""),
                    s.fields.get(1).map(String::as_str).unwrap_or(""),
                    s.fields.get(2).map(String::as_str).unwrap_or(""),
                    s.fields.get(3).map(String::as_str).unwrap_or(""),
                ) {
                    self.lat = Some(lat);
                    self.lon = Some(lon);
                    self.updated = Some(ts);
                    return true;
                }
                false
            }
            "RMC" => {
                let mut changed = false;
                if let Some((lat, lon)) = lat_lon(
                    s.fields.get(2).map(String::as_str).unwrap_or(""),
                    s.fields.get(3).map(String::as_str).unwrap_or(""),
                    s.fields.get(4).map(String::as_str).unwrap_or(""),
                    s.fields.get(5).map(String::as_str).unwrap_or(""),
                ) {
                    self.lat = Some(lat);
                    self.lon = Some(lon);
                    changed = true;
                }
                if let Some(v) = f(&s.fields, 6) {
                    self.sog = Some(v as f32);
                    changed = true;
                }
                if let Some(v) = f(&s.fields, 7) {
                    self.cog = Some(v as f32);
                    changed = true;
                }
                if changed {
                    self.updated = Some(ts);
                }
                changed
            }
            "HDT" | "HDG" => {
                if let Some(v) = f(&s.fields, 0) {
                    self.heading = Some(v as f32);
                    self.updated = Some(ts);
                    return true;
                }
                false
            }
            "VTG" => {
                let mut changed = false;
                if let Some(v) = f(&s.fields, 0) {
                    self.cog = Some(v as f32);
                    changed = true;
                }
                if let Some(v) = f(&s.fields, 4) {
                    self.sog = Some(v as f32);
                    changed = true;
                }
                if changed {
                    self.updated = Some(ts);
                }
                changed
            }
            _ => false,
        }
    }

    pub fn fix(&self, ts: DateTime<Utc>) -> Option<crate::model::OwnShipFix> {
        Some(crate::model::OwnShipFix {
            lat: self.lat?,
            lon: self.lon?,
            sog: self.sog,
            cog: self.cog,
            heading: self.heading,
            ts: Some(ts),
        })
    }
}

/// `$--TLL` target latitude/longitude (absolute target position).
#[derive(Debug, Clone)]
pub struct Tll {
    pub target: String,
    pub lat: f64,
    pub lon: f64,
    pub status: Option<char>,
    pub time: Option<String>,
}

pub fn parse_tll(s: &Sentence) -> Option<Tll> {
    // 0 target number, 1 lat, 2 N/S, 3 lon, 4 E/W, 5 name, 6 time, 7 status, 8 reference
    let (lat, lon) = lat_lon(
        s.fields.get(1).map(String::as_str).unwrap_or(""),
        s.fields.get(2).map(String::as_str).unwrap_or(""),
        s.fields.get(3).map(String::as_str).unwrap_or(""),
        s.fields.get(4).map(String::as_str).unwrap_or(""),
    )?;
    let target = s
        .fields
        .get(5)
        .filter(|v| !v.is_empty())
        .cloned()
        .or_else(|| s.fields.first().filter(|v| !v.is_empty()).cloned())
        .unwrap_or_else(|| "TGT".to_string());
    Some(Tll {
        target,
        lat,
        lon,
        status: s.fields.get(7).and_then(|v| v.chars().next()),
        time: s.fields.get(6).cloned(),
    })
}

/// `$--TTM` tracked target: range/bearing relative to own ship.
#[derive(Debug, Clone)]
pub struct Ttm {
    pub target: String,
    pub dist_m: f64,
    pub bearing_deg: f64,
    pub true_bearing: bool,
    pub speed_kn: Option<f64>,
    pub course_deg: Option<f64>,
    pub name: Option<String>,
    /// Closest point of approach (metres) and time to it (minutes).
    pub cpa_m: Option<f64>,
    pub tcpa_min: Option<f64>,
}

pub fn parse_ttm(s: &Sentence) -> Option<Ttm> {
    // 0 target#, 1 distance, 2 bearing, 3 T/R, 4 speed, 5 course, 6 T/R,
    // 7 CPA dist, 8 TCPA, 9 distance units (K/N/S), 10 name, 11 status, ...
    let raw_dist = f(&s.fields, 1)?;
    let units = s
        .fields
        .get(9)
        .map(|v| v.to_ascii_uppercase())
        .unwrap_or_default();
    let per_unit_m = match units.as_str() {
        "K" | "KM" => 1000.0,
        "S" | "SM" => 1609.344,
        _ => 1852.0, // NMEA default: nautical miles
    };
    let dist_m = raw_dist * per_unit_m;
    let bearing_deg = f(&s.fields, 2)?;
    let true_bearing = s
        .fields
        .get(3)
        .map(|v| v.to_ascii_uppercase().starts_with('T'))
        .unwrap_or(true);
    let target = s
        .fields
        .get(10)
        .filter(|v| !v.is_empty())
        .cloned()
        .or_else(|| s.fields.first().filter(|v| !v.is_empty()).cloned())
        .unwrap_or_else(|| "TGT".to_string());
    // Negative or unavailable CPA/TCPA values mean "not computed" — drop them.
    let cpa_m = f(&s.fields, 7)
        .filter(|v| *v >= 0.0)
        .map(|v| v * per_unit_m);
    let tcpa_min = f(&s.fields, 8).filter(|v| *v >= 0.0);
    Some(Ttm {
        target,
        dist_m,
        bearing_deg,
        true_bearing,
        speed_kn: f(&s.fields, 4),
        course_deg: f(&s.fields, 5),
        name: s.fields.get(10).filter(|v| !v.is_empty()).cloned(),
        cpa_m,
        tcpa_min,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_checksums() {
        let s = parse("$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47").unwrap();
        assert_eq!(s.talker, "GP");
        assert_eq!(s.kind, "GGA");
        assert!(s.checksum_ok);
        let (lat, lon) = lat_lon("4807.038", "N", "01131.000", "E").unwrap();
        assert!((lat - 48.1173).abs() < 0.001, "lat {lat}");
        assert!((lon - 11.51667).abs() < 0.001, "lon {lon}");
    }

    #[test]
    fn parses_tll() {
        let s = parse("$SDTLL,01,3600.5000,N,00530.2500,W,DARK-01,120000.00,T,*57").unwrap();
        let t = parse_tll(&s).unwrap();
        assert!((t.lat - 36.008333).abs() < 0.001, "lat {}", t.lat);
        assert!((t.lon + 5.504166).abs() < 0.001, "lon {}", t.lon);
        assert_eq!(t.target, "DARK-01");
    }

    #[test]
    fn parses_ttm_in_nautical_miles() {
        let s =
            parse("$SDTTM,03,2.50,45.0,T,12.0,90.0,T,1.2,3.4,N,SIM-03,120000.00,T,*3E").unwrap();
        let t = parse_ttm(&s).unwrap();
        assert!((t.dist_m - 4630.0).abs() < 1.0, "dist {}", t.dist_m);
        assert!(t.true_bearing);
        assert_eq!(t.target, "SIM-03");
        assert!((t.cpa_m.unwrap() - 2222.4).abs() < 1.0, "cpa {:?}", t.cpa_m);
        assert!(
            (t.tcpa_min.unwrap() - 3.4).abs() < 0.01,
            "tcpa {:?}",
            t.tcpa_min
        );
    }

    #[test]
    fn ttm_unavailable_cpa_is_dropped() {
        // 4294967295-style sentinels and negatives mean "not computed"
        let s = parse("$SDTTM,04,2.50,45.0,T,12.0,90.0,T,-1.0,-1.0,N,NA,120000.00,T,*00").unwrap();
        let t = parse_ttm(&s).unwrap();
        assert!(t.cpa_m.is_none(), "negative CPA must be ignored");
        assert!(t.tcpa_min.is_none(), "negative TCPA must be ignored");
    }

    #[test]
    fn own_ship_from_rmc() {
        let mut o = OwnShipState::default();
        let s =
            parse("$GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W*6A").unwrap();
        assert!(o.update(&s, Utc::now()));
        assert!((o.sog.unwrap() - 22.4).abs() < 0.01);
        assert!(o.lat.is_some());
    }
}
