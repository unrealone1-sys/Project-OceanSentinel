//! Shared ingest router: one per feed. Turns raw lines (NMEA or JSON) into
//! typed events, keeping the per-feed AIS fragment assembler and own-ship state.

use chrono::Utc;
use serde_json::Value;

use crate::ais;
use crate::model::{Contact, SensorKind};
use crate::nmea::{self, OwnShipState};
use crate::sources::{Event, EventTx, RelativeTarget};

pub struct Router {
    pub feed: String,
    pub sonar_talkers: Vec<String>,
    pub lidar: bool,
    pub asm: ais::Assembler,
    pub own: OwnShipState,
}

impl Router {
    pub fn new(feed: impl Into<String>, sonar_talkers: Vec<String>, lidar: bool) -> Self {
        Router {
            feed: feed.into(),
            sonar_talkers,
            lidar,
            asm: ais::Assembler::default(),
            own: OwnShipState::default(),
        }
    }

    /// Route one line. Returns true when it was recognized as sensor data.
    pub fn line(&mut self, line: &str, tx: &EventTx) -> bool {
        let line = line.trim().trim_matches('\u{feff}');
        if line.is_empty() {
            return false;
        }
        if line.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                return self.json_line(&v, tx);
            }
            return false;
        }
        match nmea::parse(line) {
            Some(s) => {
                if !s.checksum_ok {
                    tracing::debug!(feed = %self.feed, line = %s.raw, "bad NMEA checksum (line still parsed)");
                }
                self.sentence(s, tx)
            }
            None => {
                tracing::debug!(feed = %self.feed, line = %line, "line is neither JSON nor a valid NMEA sentence");
                false
            }
        }
    }

    fn json_line(&mut self, v: &Value, tx: &EventTx) -> bool {
        // An explicit sensor marker wins over the mmsi heuristic: a LiDAR
        // contact may legitimately carry an mmsi to pin it to an AIS track.
        let sensor = v
            .get("sensor")
            .or_else(|| v.get("type"))
            .and_then(|s| s.as_str())
            .unwrap_or("");
        let declares_lidar = sensor.eq_ignore_ascii_case("lidar") || v.get("lidar").is_some();
        if declares_lidar {
            return self.lidar_contact(v, tx);
        }

        let looks_like_ais = v.get("mmsi").is_some()
            && (v.get("class").and_then(|c| c.as_str()) == Some("AIS")
                || v.get("lat").is_some()
                || v.get("shipname").is_some());
        if looks_like_ais {
            if let Some(body) = ais::body_from_json(v) {
                let _ = tx.send(Event::Ais(body));
                return true;
            }
        }

        // A lidar feed without a sensor marker: accept any unlabelled contact.
        if self.lidar {
            return self.lidar_contact(v, tx);
        }
        false
    }

    /// Build a LiDAR contact from a JSON object; false when it has no position.
    fn lidar_contact(&mut self, v: &Value, tx: &EventTx) -> bool {
        let lat = v.get("lat").and_then(|x| x.as_f64());
        let lon = v.get("lon").and_then(|x| x.as_f64());
        if let (Some(lat), Some(lon)) = (lat, lon) {
            if lat.abs() <= 90.0 && lon.abs() <= 180.0 {
                let c = Contact {
                    source: SensorKind::Lidar,
                    lat,
                    lon,
                    mmsi: v.get("mmsi").and_then(|x| x.as_u64()).map(|m| m as u32),
                    label: v
                        .get("id")
                        .or_else(|| v.get("name"))
                        .or_else(|| v.get("label"))
                        .and_then(|x| x.as_str())
                        .map(str::to_string),
                    range_m: v.get("range_m").and_then(|x| x.as_f64()),
                    bearing_deg: v.get("bearing_deg").and_then(|x| x.as_f64()),
                    sog_kn: v
                        .get("speed")
                        .or_else(|| v.get("sog"))
                        .and_then(|x| x.as_f64())
                        .map(|v| v as f32),
                    cog_deg: v
                        .get("course")
                        .or_else(|| v.get("cog"))
                        .and_then(|x| x.as_f64())
                        .map(|v| v as f32),
                    cpa_m: v.get("cpa_m").and_then(|x| x.as_f64()),
                    tcpa_min: v.get("tcpa_min").and_then(|x| x.as_f64()),
                    confidence: v.get("confidence").and_then(|x| x.as_f64()).unwrap_or(0.9) as f32,
                    ts: Utc::now(),
                };
                let _ = tx.send(Event::Contact(c));
                return true;
            }
        }
        false
    }

    fn sentence(&mut self, s: nmea::Sentence, tx: &EventTx) -> bool {
        let now = Utc::now();
        match s.kind.as_str() {
            "VDM" | "VDO" => {
                if let Some((payload, fill)) = self.asm.push(&s) {
                    if let Some(body) = ais::decode(&payload, fill) {
                        let _ = tx.send(Event::Ais(body));
                    }
                }
                true
            }
            "GGA" | "GLL" | "RMC" | "HDT" | "HDG" | "VTG" => {
                if self.own.update(&s, now) {
                    if let Some(fix) = self.own.fix(now) {
                        let _ = tx.send(Event::OwnShip(fix));
                    }
                }
                true
            }
            "TLL" => {
                if !self.talker_allowed(&s.talker) {
                    tracing::debug!(feed = %self.feed, talker = %s.talker, "TLL from unconfigured talker ignored");
                    return false;
                }
                match nmea::parse_tll(&s) {
                    Some(t) => {
                        // TLL status: T = tracking, L = lost, Q = query
                        if t.status
                            .map(|c| c.eq_ignore_ascii_case(&'L'))
                            .unwrap_or(false)
                        {
                            tracing::debug!(feed = %self.feed, target = %t.target, "TLL target reported lost; not tracking");
                            return true;
                        }
                        tracing::debug!(feed = %self.feed, target = %t.target, lat = t.lat, lon = t.lon, fix_time = t.time.as_deref().unwrap_or("-"), "sonar/radar TLL target");
                        let c = Contact {
                            source: self.source_for(&s.talker),
                            lat: t.lat,
                            lon: t.lon,
                            mmsi: None,
                            label: Some(t.target),
                            range_m: None,
                            bearing_deg: None,
                            sog_kn: None,
                            cog_deg: None,
                            cpa_m: None,
                            tcpa_min: None,
                            confidence: 0.8,
                            ts: now,
                        };
                        let _ = tx.send(Event::Contact(c));
                    }
                    None => {
                        tracing::warn!(feed = %self.feed, line = %s.raw, "TLL sentence could not be parsed")
                    }
                }
                true
            }
            "TTM" => {
                if !self.talker_allowed(&s.talker) {
                    tracing::debug!(feed = %self.feed, talker = %s.talker, "TTM from unconfigured talker ignored");
                    return false;
                }
                match nmea::parse_ttm(&s) {
                    Some(t) => {
                        tracing::debug!(feed = %self.feed, target = %t.target, dist_m = t.dist_m, bearing = t.bearing_deg, "sonar/radar TTM target");
                        let _ = tx.send(Event::RelativeTarget(RelativeTarget {
                            source: self.source_for(&s.talker),
                            target: t.target.clone(),
                            dist_m: t.dist_m,
                            bearing_deg: t.bearing_deg,
                            true_bearing: t.true_bearing,
                            speed_kn: t.speed_kn,
                            course_deg: t.course_deg,
                            cpa_m: t.cpa_m,
                            tcpa_min: t.tcpa_min,
                            name: t.name,
                            ts: now,
                        }));
                    }
                    None => {
                        tracing::warn!(feed = %self.feed, line = %s.raw, "TTM sentence could not be parsed")
                    }
                }
                true
            }
            _ => {
                tracing::debug!(feed = %self.feed, talker = %s.talker, kind = %s.kind, "unhandled NMEA sentence");
                false
            }
        }
    }

    fn talker_allowed(&self, talker: &str) -> bool {
        talker.eq_ignore_ascii_case("LI")
            || self
                .sonar_talkers
                .iter()
                .any(|t| t.eq_ignore_ascii_case(talker))
            || self.sonar_talkers.is_empty()
    }

    fn source_for(&self, talker: &str) -> SensorKind {
        if talker.eq_ignore_ascii_case("LI") || talker.eq_ignore_ascii_case("LD") {
            SensorKind::Lidar
        } else if talker.eq_ignore_ascii_case("RA") {
            SensorKind::Radar
        } else {
            SensorKind::Sonar
        }
    }
}

/// Build a checksummed NMEA sentence from a body like "SDTLL,01,...".
pub fn nmea_sentence(body: &str) -> String {
    format!("${}*{:02X}", body, nmea::checksum(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn routes_tll_and_ais() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut r = Router::new("test", vec!["SD".into(), "SN".into()], false);
        assert!(r.line(
            "$SDTLL,01,3600.5000,N,00530.2500,W,TGT-9,120000.00,T,*00",
            &tx
        ));
        match rx.try_recv().unwrap() {
            Event::Contact(c) => {
                assert_eq!(c.source, SensorKind::Sonar);
                assert_eq!(c.label.as_deref(), Some("TGT-9"));
            }
            other => panic!("unexpected {other:?}"),
        }
        let ais_line = crate::ais::encode_position_a(244000001, 36.1, -5.4, 7.0, 90.0, 91.0, 0);
        assert!(r.line(&ais_line, &tx));
        match rx.try_recv().unwrap() {
            Event::Ais(crate::ais::AisBody::Position { mmsi, .. }) => assert_eq!(mmsi, 244000001),
            other => panic!("unexpected {other:?}"),
        }
        // an unconfigured talker is ignored
        assert!(!r.line(
            "$GPTLL,02,3600.5000,N,00530.2500,W,OTHER,120000.00,T,*00",
            &tx
        ));
    }

    #[test]
    fn routes_lidar_json() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut r = Router::new("lidar", vec!["LI".into()], true);
        let line = r#"{"sensor":"lidar","id":"L-7","lat":36.05,"lon":-5.31,"range_m":1200.5,"bearing_deg":270.1,"confidence":0.93}"#;
        assert!(r.line(line, &tx));
        match rx.try_recv().unwrap() {
            Event::Contact(c) => {
                assert_eq!(c.source, SensorKind::Lidar);
                assert!((c.confidence - 0.93).abs() < 1e-6);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// A LiDAR contact may carry an mmsi to pin it to an AIS track; it must be
    /// routed as a sensor contact, not decoded as an AIS position report.
    #[test]
    fn lidar_json_with_mmsi_is_a_sensor_contact() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut r = Router::new("lidar", vec!["LI".into()], true);
        let line = r#"{"sensor":"lidar","id":"TRACKER","mmsi":227006760,"lat":36.0123,"lon":-5.3988,"confidence":0.95}"#;
        assert!(r.line(line, &tx));
        match rx.try_recv().unwrap() {
            Event::Contact(c) => {
                assert_eq!(c.source, SensorKind::Lidar);
                assert_eq!(c.mmsi, Some(227006760));
            }
            other => panic!("expected a sensor contact, got {other:?}"),
        }
    }

    #[test]
    fn routes_ais_json() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut r = Router::new("ais", Vec::new(), false);
        let line = r#"{"class":"AIS","mmsi":244123456,"lat":36.05,"lon":-5.31,"speed":9.1}"#;
        assert!(r.line(line, &tx));
        match rx.try_recv().unwrap() {
            Event::Ais(crate::ais::AisBody::Position { mmsi, .. }) => assert_eq!(mmsi, 244123456),
            other => panic!("unexpected {other:?}"),
        }
    }
}
