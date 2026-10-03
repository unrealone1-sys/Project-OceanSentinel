//! AIS (Automatic Identification System) message decoding and encoding.
//!
//! Decoder: 6-bit ASCII armoured !AIVDM/!AIVDO payloads -> typed messages.
//! Assembler: stitches multi-fragment messages (type 5 typically spans two).
//! Encoder: used by the built-in simulator and by round-trip unit tests, so
//! the demo traffic flows through exactly the same decode path as live radio.

use std::collections::HashMap;

use crate::nmea;

const SIXBIT: &[u8; 64] = b"@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_ !\"#$%&'()*+,-./0123456789:;<=>?";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AisBody {
    Position {
        mmsi: u32,
        nav_status: u8,
        sog: Option<f32>,
        cog: Option<f32>,
        heading: Option<f32>,
        lat: Option<f64>,
        lon: Option<f64>,
        maneuver: Option<u8>,
        class_b: bool,
    },
    StaticA {
        mmsi: u32,
        imo: Option<u32>,
        callsign: Option<String>,
        name: Option<String>,
        ship_type: Option<u8>,
        length: Option<u16>,
        beam: Option<u16>,
        destination: Option<String>,
        draught: Option<f32>,
    },
    StaticBName {
        mmsi: u32,
        name: Option<String>,
    },
    StaticB {
        mmsi: u32,
        ship_type: Option<u8>,
        callsign: Option<String>,
        length: Option<u16>,
        beam: Option<u16>,
    },
    ExtendedClassB {
        mmsi: u32,
        name: Option<String>,
        ship_type: Option<u8>,
        lat: Option<f64>,
        lon: Option<f64>,
        sog: Option<f32>,
        cog: Option<f32>,
        heading: Option<f32>,
    },
    BaseStation {
        mmsi: u32,
        lat: Option<f64>,
        lon: Option<f64>,
    },
    Other {
        msg_type: u8,
        mmsi: Option<u32>,
    },
}

impl AisBody {
    pub fn mmsi(&self) -> Option<u32> {
        match self {
            AisBody::Position { mmsi, .. }
            | AisBody::StaticA { mmsi, .. }
            | AisBody::StaticBName { mmsi, .. }
            | AisBody::StaticB { mmsi, .. }
            | AisBody::ExtendedClassB { mmsi, .. }
            | AisBody::BaseStation { mmsi, .. } => Some(*mmsi),
            AisBody::Other { mmsi, .. } => *mmsi,
        }
    }
}

fn six_bit_value(c: u8) -> Option<u8> {
    match c {
        48..=87 => Some(c - 48),
        96..=119 => Some(c - 56),
        _ => None,
    }
}

fn payload_values(payload: &str) -> Option<Vec<u8>> {
    payload.bytes().map(six_bit_value).collect()
}

/// Read bits out of a 6-bit-per-cell payload.
pub struct Bits {
    data: Vec<u8>,
    pos: usize,
    fill: u8,
}

impl Bits {
    pub fn new(data: Vec<u8>, fill: u8) -> Self {
        Bits { data, pos: 0, fill }
    }

    pub fn remaining(&self) -> usize {
        (self.data.len() * 6)
            .saturating_sub(self.fill as usize)
            .saturating_sub(self.pos)
    }

    pub fn u(&mut self, n: usize) -> Option<u64> {
        if n == 0 || n > 64 || self.remaining() < n {
            return None;
        }
        let mut v = 0u64;
        for _ in 0..n {
            let idx = self.pos / 6;
            let bit = (self.data[idx] >> (5 - (self.pos % 6))) & 1;
            v = (v << 1) | bit as u64;
            self.pos += 1;
        }
        Some(v)
    }

    /// Two's-complement signed field.
    pub fn i(&mut self, n: usize) -> Option<i64> {
        if n == 0 || n >= 64 {
            return None;
        }
        let v = self.u(n)?;
        let sign = 1u64 << (n - 1);
        if v & sign != 0 {
            Some(v as i64 - (1i64 << n))
        } else {
            Some(v as i64)
        }
    }

    /// 6-bit ASCII text field, right-padded with '@'.
    pub fn text(&mut self, chars: usize) -> Option<String> {
        let mut out = String::with_capacity(chars);
        for _ in 0..chars {
            let v = self.u(6)? as usize;
            out.push(SIXBIT[v] as char);
        }
        Some(out.trim_end_matches(['@', ' ']).trim().to_string())
    }
}

fn scale_opt(v: u64, div: f64, unavailable: u64) -> Option<f32> {
    if v == unavailable {
        None
    } else {
        Some((v as f64 / div) as f32)
    }
}

fn coord(raw: i64, div: f64, max_abs: f64) -> Option<f64> {
    let v = raw as f64 / div;
    if v.abs() > max_abs {
        None
    } else {
        Some(v)
    }
}

fn heading_opt(v: u64) -> Option<f32> {
    if v == 511 {
        None
    } else {
        Some(v as f32)
    }
}

pub fn decode(payload: &str, fill: u8) -> Option<AisBody> {
    let vals = payload_values(payload)?;
    let mut b = Bits::new(vals, fill);
    let msg_type = b.u(6)? as u8;
    let _repeat = b.u(2)?;
    let mmsi = b.u(30)? as u32;
    match msg_type {
        1 | 2 | 3 => {
            let nav_status = b.u(4)? as u8;
            let _rot = b.i(8)?;
            let sog = scale_opt(b.u(10)?, 10.0, 1023);
            let _accuracy = b.u(1)?;
            let lon = coord(b.i(28)?, 600_000.0, 180.0);
            let lat = coord(b.i(27)?, 600_000.0, 90.0);
            let cog = scale_opt(b.u(12)?, 10.0, 3600);
            let heading = heading_opt(b.u(9)?);
            let _ts = b.u(6)?;
            let maneuver = b.u(2).map(|v| v as u8);
            Some(AisBody::Position {
                mmsi,
                nav_status,
                sog,
                cog,
                heading,
                lat,
                lon,
                maneuver,
                class_b: false,
            })
        }
        18 => {
            let _reserved = b.u(8)?;
            let sog = scale_opt(b.u(10)?, 10.0, 1023);
            let _accuracy = b.u(1)?;
            let lon = coord(b.i(28)?, 600_000.0, 180.0);
            let lat = coord(b.i(27)?, 600_000.0, 90.0);
            let cog = scale_opt(b.u(12)?, 10.0, 3600);
            let heading = heading_opt(b.u(9)?);
            Some(AisBody::Position {
                mmsi,
                nav_status: 15,
                sog,
                cog,
                heading,
                lat,
                lon,
                maneuver: None,
                class_b: true,
            })
        }
        19 => {
            let _reserved = b.u(8)?;
            let sog = scale_opt(b.u(10)?, 10.0, 1023);
            let _accuracy = b.u(1)?;
            let lon = coord(b.i(28)?, 600_000.0, 180.0);
            let lat = coord(b.i(27)?, 600_000.0, 90.0);
            let cog = scale_opt(b.u(12)?, 10.0, 3600);
            let heading = heading_opt(b.u(9)?);
            let _ts = b.u(6)?;
            let _reserved2 = b.u(4)?;
            let name = b.text(20);
            let ship_type = b.u(8).map(|v| v as u8);
            Some(AisBody::ExtendedClassB {
                mmsi,
                name: name.filter(|s| !s.is_empty()),
                ship_type: ship_type.filter(|t| *t != 0),
                lat,
                lon,
                sog,
                cog,
                heading,
            })
        }
        5 => {
            let _version = b.u(2)?;
            let imo = b.u(30)?;
            let callsign = b.text(7);
            let name = b.text(20);
            let ship_type = b.u(8)? as u8;
            let bow = b.u(9)? as u16;
            let stern = b.u(9)? as u16;
            let port = b.u(6)? as u16;
            let starboard = b.u(6)? as u16;
            let _fix = b.u(4)?;
            let _eta = b.u(20)?;
            let draught = b.u(8)?;
            let destination = b.text(20);
            let _dte = b.u(1).unwrap_or(0);
            Some(AisBody::StaticA {
                mmsi,
                imo: if imo == 0 { None } else { Some(imo as u32) },
                callsign: callsign.filter(|s| !s.is_empty()),
                name: name.filter(|s| !s.is_empty()),
                ship_type: if ship_type == 0 { None } else { Some(ship_type) },
                length: if bow + stern > 0 { Some(bow + stern) } else { None },
                beam: if port + starboard > 0 {
                    Some(port + starboard)
                } else {
                    None
                },
                destination: destination.filter(|s| !s.is_empty()),
                draught: if draught == 0 {
                    None
                } else {
                    Some(draught as f32 / 10.0)
                },
            })
        }
        24 => {
            let part = b.u(2)?;
            if part == 0 {
                let name = b.text(20);
                Some(AisBody::StaticBName {
                    mmsi,
                    name: name.filter(|s| !s.is_empty()),
                })
            } else {
                let ship_type = b.u(8)? as u8;
                let _vendor = b.text(3);
                let callsign = b.text(7);
                let bow = b.u(9)? as u16;
                let stern = b.u(9)? as u16;
                let port = b.u(6)? as u16;
                let starboard = b.u(6)? as u16;
                Some(AisBody::StaticB {
                    mmsi,
                    ship_type: if ship_type == 0 { None } else { Some(ship_type) },
                    callsign: callsign.filter(|s| !s.is_empty()),
                    length: if bow + stern > 0 { Some(bow + stern) } else { None },
                    beam: if port + starboard > 0 {
                        Some(port + starboard)
                    } else {
                        None
                    },
                })
            }
        }
        4 | 11 => {
            let _year = b.u(14)?;
            let _month = b.u(4)?;
            let _day = b.u(5)?;
            let _hour = b.u(5)?;
            let _min = b.u(6)?;
            let _sec = b.u(6)?;
            let _accuracy = b.u(1)?;
            let lon = coord(b.i(28)?, 600_000.0, 180.0);
            let lat = coord(b.i(27)?, 600_000.0, 90.0);
            Some(AisBody::BaseStation { mmsi, lat, lon })
        }
        _ => Some(AisBody::Other {
            msg_type,
            mmsi: Some(mmsi),
        }),
    }
}

/// Build an AisBody from an AIS-catcher style JSON line (their `-o 5` output).
pub fn body_from_json(v: &serde_json::Value) -> Option<AisBody> {
    let mmsi = v.get("mmsi")?.as_u64()? as u32;
    let num = |k: &str| v.get(k).and_then(|x| x.as_f64());
    let name = v
        .get("shipname")
        .or_else(|| v.get("name"))
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let callsign = v
        .get("callsign")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let ship_type = v
        .get("shiptype")
        .or_else(|| v.get("ship_type"))
        .and_then(|x| x.as_u64())
        .map(|t| t as u8)
        .filter(|t| *t != 0);
    let imo = v
        .get("imo")
        .and_then(|x| x.as_u64())
        .map(|t| t as u32)
        .filter(|t| *t != 0);
    let destination = v
        .get("destination")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if let (Some(lat), Some(lon)) = (num("lat"), num("lon")) {
        if lat.abs() <= 90.0 && lon.abs() <= 180.0 && lat != 0.0 && lon != 0.0 {
            let has_static = name.is_some() || callsign.is_some() || ship_type.is_some();
            if has_static && ship_type.is_some() && num("sog").is_none() && num("speed").is_none() {
                return Some(AisBody::StaticA {
                    mmsi,
                    imo,
                    callsign,
                    name,
                    ship_type,
                    length: None,
                    beam: None,
                    destination,
                    draught: num("draught").map(|d| d as f32),
                });
            }
            return Some(AisBody::Position {
                mmsi,
                nav_status: v.get("status").and_then(|x| x.as_u64()).unwrap_or(15) as u8,
                sog: num("speed").or_else(|| num("sog")).map(|s| s as f32),
                cog: num("course").or_else(|| num("cog")).map(|s| s as f32),
                heading: num("heading").map(|s| s as f32),
                lat: Some(lat),
                lon: Some(lon),
                maneuver: None,
                class_b: v.get("type").and_then(|x| x.as_u64()).map(|t| t == 18 || t == 19).unwrap_or(false),
            });
        }
    }
    if name.is_some() || callsign.is_some() || ship_type.is_some() {
        return Some(AisBody::StaticA {
            mmsi,
            imo,
            callsign,
            name,
            ship_type,
            length: None,
            beam: None,
            destination,
            draught: None,
        });
    }
    Some(AisBody::Other {
        msg_type: v.get("type").and_then(|x| x.as_u64()).unwrap_or(0) as u8,
        mmsi: Some(mmsi),
    })
}

/// Reassembles multi-fragment AIVDM messages.
#[derive(Default)]
pub struct Assembler {
    parts: HashMap<String, Partial>,
}

struct Partial {
    total: u8,
    got: u8,
    payload: String,
    fill: u8,
}

impl Assembler {
    pub fn push(&mut self, s: &nmea::Sentence) -> Option<(String, u8)> {
        if s.kind != "VDM" && s.kind != "VDO" {
            return None;
        }
        let total: u8 = s.fields.first()?.parse().ok()?;
        let index: u8 = s.fields.get(1)?.parse().ok()?;
        let seq = s.fields.get(2).cloned().unwrap_or_default();
        let channel = s.fields.get(3).cloned().unwrap_or_default();
        let payload = s.fields.get(4)?.clone();
        let fill: u8 = s.fields.get(5).and_then(|v| v.parse().ok()).unwrap_or(0);
        if payload.is_empty() {
            return None;
        }
        if total <= 1 {
            return Some((payload, fill));
        }
        let key = if seq.is_empty() {
            format!("c{channel}")
        } else {
            format!("{seq}:{channel}")
        };
        let done = {
            let entry = self.parts.entry(key.clone()).or_insert_with(|| Partial {
                total,
                got: 0,
                payload: String::new(),
                fill: 0,
            });
            if index == 1 {
                entry.payload.clear();
                entry.got = 0;
            }
            entry.total = total;
            entry.payload.push_str(&payload);
            entry.got += 1;
            entry.fill = fill;
            entry.got >= entry.total
        };
        if done {
            let p = self.parts.remove(&key)?;
            Some((p.payload, p.fill))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Encoding (simulator + tests)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct BitWriter {
    pub bits: Vec<u8>,
}

impl BitWriter {
    pub fn new() -> Self {
        Self { bits: Vec::new() }
    }

    pub fn u(&mut self, v: u64, n: usize) {
        for i in (0..n).rev() {
            self.bits.push(((v >> i) & 1) as u8);
        }
    }

    pub fn i(&mut self, v: i64, n: usize) {
        let m = if v < 0 {
            ((1i128 << n) + v as i128) as u64
        } else {
            v as u64
        };
        self.u(m, n);
    }

    pub fn text(&mut self, s: &str, chars: usize) {
        let up = s.to_uppercase();
        let bytes = up.as_bytes();
        for i in 0..chars {
            let c = *bytes.get(i).unwrap_or(&b' ');
            let v = SIXBIT.iter().position(|&x| x == c).unwrap_or(32);
            self.u(v as u64, 6);
        }
    }

    pub fn fill_bits(&self) -> u8 {
        ((6 - self.bits.len() % 6) % 6) as u8
    }
}

pub fn armor(bits: &[u8], fill: u8) -> String {
    let mut b = bits.to_vec();
    for _ in 0..fill {
        b.push(0);
    }
    let mut out = String::with_capacity(b.len() / 6 + 1);
    for chunk in b.chunks(6) {
        let mut v = 0u8;
        for (i, bit) in chunk.iter().enumerate() {
            v |= bit << (5 - i);
        }
        let c = if v < 40 { v + 48 } else { v + 56 };
        out.push(c as char);
    }
    out
}

pub fn nmea_line(payload: &str, fill: u8, channel: char) -> String {
    let body = format!("AIVDM,1,1,,{channel},{payload},{fill}");
    let cs = nmea::checksum(&body);
    format!("!{body}*{cs:02X}")
}

pub fn encode_position_a(
    mmsi: u32,
    lat: f64,
    lon: f64,
    sog: f64,
    cog: f64,
    heading: f64,
    nav_status: u8,
) -> String {
    let mut w = BitWriter::new();
    w.u(1, 6);
    w.u(0, 2);
    w.u(mmsi as u64, 30);
    w.u(nav_status as u64, 4);
    w.u(128, 8); // rate of turn: not available
    w.u((sog * 10.0).round().clamp(0.0, 1022.0) as u64, 10);
    w.u(0, 1); // position accuracy
    w.i((lon * 600_000.0).round() as i64, 28);
    w.i((lat * 600_000.0).round() as i64, 27);
    w.u((cog * 10.0).round().clamp(0.0, 3599.0) as u64, 12);
    w.u(heading.round().clamp(0.0, 359.0) as u64, 9);
    w.u(60, 6); // timestamp: not available
    w.u(0, 2); // maneuver
    w.u(0, 3); // spare
    w.u(0, 1); // raim
    w.u(0, 19); // radio status
    let fill = w.fill_bits();
    nmea_line(&armor(&w.bits, fill), fill, 'A')
}

#[allow(clippy::too_many_arguments)]
pub fn encode_static_a(
    mmsi: u32,
    imo: u32,
    callsign: &str,
    name: &str,
    ship_type: u8,
    bow: u16,
    stern: u16,
    port: u16,
    starboard: u16,
    destination: &str,
    draught: f32,
) -> String {
    let mut w = BitWriter::new();
    w.u(5, 6);
    w.u(0, 2);
    w.u(mmsi as u64, 30);
    w.u(0, 2); // AIS version
    w.u(imo as u64, 30);
    w.text(callsign, 7);
    w.text(name, 20);
    w.u(ship_type as u64, 8);
    w.u(bow as u64, 9);
    w.u(stern as u64, 9);
    w.u(port as u64, 6);
    w.u(starboard as u64, 6);
    w.u(1, 4); // position fix: GPS
    w.u(0, 20); // ETA
    w.u((draught * 10.0).round() as u64, 8);
    w.text(destination, 20);
    w.u(0, 1); // DTE
    // pad to 424 bits as real stations do; spare bits carry no information
    while w.bits.len() < 424 {
        w.bits.push(0);
    }
    let fill = w.fill_bits();
    nmea_line(&armor(&w.bits, fill), fill, 'A')
}

/// Class B (type 18) position report. Used by the round-trip tests and kept
/// for feeding Class B transponder traffic into the pipeline.
#[allow(dead_code)]
pub fn encode_class_b_position(
    mmsi: u32,
    lat: f64,
    lon: f64,
    sog: f64,
    cog: f64,
    heading: f64,
) -> String {
    let mut w = BitWriter::new();
    w.u(18, 6);
    w.u(0, 2);
    w.u(mmsi as u64, 30);
    w.u(0, 8); // reserved
    w.u((sog * 10.0).round().clamp(0.0, 1022.0) as u64, 10);
    w.u(0, 1);
    w.i((lon * 600_000.0).round() as i64, 28);
    w.i((lat * 600_000.0).round() as i64, 27);
    w.u((cog * 10.0).round().clamp(0.0, 3599.0) as u64, 12);
    w.u(heading.round().clamp(0.0, 359.0) as u64, 9);
    w.u(60, 6);
    w.u(0, 2); // flags
    w.u(0, 1); // unit
    w.u(0, 1); // display
    w.u(0, 1); // dsc
    w.u(0, 1); // band
    w.u(0, 1); // msg22
    w.u(0, 1); // assigned
    w.u(0, 1); // raim
    w.u(0, 20); // radio
    let fill = w.fill_bits();
    nmea_line(&armor(&w.bits, fill), fill, 'B')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn through_pipeline(line: &str) -> AisBody {
        let s = nmea::parse(line).expect("parse sentence");
        assert!(s.checksum_ok, "bad checksum in generated line: {line}");
        let mut asm = Assembler::default();
        let (payload, fill) = asm.push(&s).expect("assemble");
        decode(&payload, fill).expect("decode")
    }

    #[test]
    fn round_trip_class_a_position() {
        let line = encode_position_a(227006760, 36.012345, -5.398765, 8.3, 92.4, 95.0, 0);
        match through_pipeline(&line) {
            AisBody::Position {
                mmsi,
                lat,
                lon,
                sog,
                cog,
                heading,
                nav_status,
                class_b,
                ..
            } => {
                assert_eq!(mmsi, 227006760);
                assert!((lat.unwrap() - 36.012345).abs() < 1e-4, "lat {lat:?}");
                assert!((lon.unwrap() + 5.398765).abs() < 1e-4, "lon {lon:?}");
                assert!((sog.unwrap() - 8.3).abs() < 0.05);
                assert!((cog.unwrap() - 92.4).abs() < 0.05);
                assert!((heading.unwrap() - 95.0).abs() < 0.5);
                assert_eq!(nav_status, 0);
                assert!(!class_b);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_static_a() {
        let line = encode_static_a(
            227006760,
            9123456,
            "OS1234",
            "ATLANTIC DAWN",
            30,
            20,
            10,
            4,
            4,
            "GIBRALTAR",
            4.5,
        );
        match through_pipeline(&line) {
            AisBody::StaticA {
                mmsi,
                imo,
                name,
                callsign,
                ship_type,
                length,
                beam,
                destination,
                draught,
            } => {
                assert_eq!(mmsi, 227006760);
                assert_eq!(imo, Some(9123456));
                assert_eq!(name.as_deref(), Some("ATLANTIC DAWN"));
                assert_eq!(callsign.as_deref(), Some("OS1234"));
                assert_eq!(ship_type, Some(30));
                assert_eq!(length, Some(30));
                assert_eq!(beam, Some(8));
                assert_eq!(destination.as_deref(), Some("GIBRALTAR"));
                assert!((draught.unwrap() - 4.5).abs() < 0.05);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_class_b() {
        let line = encode_class_b_position(232012345, 36.1, -5.4, 6.2, 180.5, 181.0);
        match through_pipeline(&line) {
            AisBody::Position {
                mmsi,
                class_b,
                cog,
                ..
            } => {
                assert_eq!(mmsi, 232012345);
                assert!(class_b);
                assert!((cog.unwrap() - 180.5).abs() < 0.05);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn multi_part_assembly() {
        let line = encode_static_a(1, 0, "AB", "TWO PART SHIP", 70, 50, 20, 8, 8, "ROTTERDAM", 9.9);
        let s = nmea::parse(&line).unwrap();
        let payload = s.fields[4].clone();
        let (a, b) = payload.split_at(payload.len() / 2);
        let p1 = format!("!AIVDM,2,1,3,A,{a},0*00");
        let f1 = nmea::checksum(p1.trim_start_matches('!').split('*').next().unwrap());
        let p1 = format!("!AIVDM,2,1,3,A,{a},0*{f1:02X}");
        let p2 = format!("!AIVDM,2,2,3,A,{b},2*00");
        let f2 = nmea::checksum(p2.trim_start_matches('!').split('*').next().unwrap());
        let p2 = format!("!AIVDM,2,2,3,A,{b},2*{f2:02X}");
        let mut asm = Assembler::default();
        assert!(asm.push(&nmea::parse(&p1).unwrap()).is_none());
        let (payload, fill) = asm.push(&nmea::parse(&p2).unwrap()).expect("assembled");
        assert_eq!(fill, 2);
        match decode(&payload, fill).unwrap() {
            AisBody::StaticA { name, ship_type, .. } => {
                assert_eq!(name.as_deref(), Some("TWO PART SHIP"));
                assert_eq!(ship_type, Some(70));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn json_ingest() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"class":"AIS","mmsi":244123456,"lat":36.05,"lon":-5.31,"speed":9.1,"course":88.0,"heading":90,"shipname":"NORDIC STAR","shiptype":70}"#,
        )
        .unwrap();
        match body_from_json(&v).unwrap() {
            AisBody::Position { mmsi, sog, .. } => {
                assert_eq!(mmsi, 244123456);
                assert!((sog.unwrap() - 9.1).abs() < 0.01);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// Prints sample sentences so an external decoder (e.g. pyais) can confirm
    /// the encoder is spec-conformant. Run with:
    ///   cargo test print_samples -- --nocapture
    #[test]
    fn print_samples() {
        println!(
            "SAMPLE-POS {}",
            encode_position_a(227006760, 36.012345, -5.398765, 8.3, 92.4, 95.0, 0)
        );
        println!(
            "SAMPLE-STATIC {}",
            encode_static_a(
                227006760,
                9123456,
                "OS1234",
                "ATLANTIC DAWN",
                30,
                20,
                10,
                4,
                4,
                "GIBRALTAR",
                4.5
            )
        );
        println!(
            "SAMPLE-CLASSB {}",
            encode_class_b_position(232012345, 36.1, -5.4, 6.2, 180.5, 181.0)
        );
    }
}
