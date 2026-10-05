//! Built-in traffic generator: a self-contained scenario that exercises the
//! entire pipeline (AIS encode -> NMEA -> decode, sonar TLL/TTM, LiDAR JSON,
//! own-ship navigation sentences, dark vessels, an AIS dropout) so the app is
//! demonstrable with zero hardware. Positions are synthetic.

use std::time::Duration;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tokio::task::JoinHandle;

use crate::ais;
use crate::config::{AoiCfg, SimCfg};
use crate::geo;
use crate::land;
use crate::sources::ingest::{nmea_sentence, Router};
use crate::sources::{feed_status, Event, EventTx};

const ADJ: [&str; 12] = [
    "ATLANTIC", "NORDIC", "IBERIAN", "PACIFIC", "BALTIC", "ADRIATIC", "LEVANT", "SAHARAN",
    "CASPIAN", "MALTA", "AEGEAN", "CELTIC",
];
const NOUN: [&str; 8] = [
    "DAWN", "STAR", "TRADER", "HORIZON", "VOYAGER", "MARINER", "SPIRIT", "GUARDIAN",
];

struct SimVessel {
    mmsi: Option<u32>,
    name: String,
    callsign: String,
    ship_type: u8,
    lat: f64,
    lon: f64,
    cog: f64,
    sog: f64,
    ais: bool,
    /// periodically stops transmitting (simulates an AIS gap / dark behaviour)
    gap_pattern: bool,
}

/// Headings to try (relative to the intended course) when the direct step would
/// put a vessel on land. Smallest deviation first, so traffic bends around a
/// coast instead of teleporting through it.
const STEER_OFFSETS: [f64; 13] = [
    0.0, 25.0, -25.0, 45.0, -45.0, 70.0, -70.0, 100.0, -100.0, 130.0, -130.0, 160.0, -160.0,
];

/// First heading whose next step stays on water.
fn steer_to_water(lat: f64, lon: f64, cog: f64, step_m: f64) -> Option<(f64, f64, f64)> {
    for d in STEER_OFFSETS {
        let hdg = (cog + d + 360.0) % 360.0;
        let (tlat, tlon) = geo::destination_point(lat, lon, hdg, step_m);
        if !land::on_land(tlat, tlon) {
            return Some((tlat, tlon, hdg));
        }
    }
    None
}

fn advance(v: &mut SimVessel, dt_h: f64, rng: &mut impl Rng) {
    let step_m = v.sog * dt_h * 1852.0;
    if step_m > 0.05 {
        match steer_to_water(v.lat, v.lon, v.cog, step_m) {
            Some((lat, lon, cog)) => {
                v.lat = lat;
                v.lon = lon;
                v.cog = cog;
            }
            None => {
                // boxed in on every heading: turn around and try again next tick
                v.cog = (v.cog + 180.0) % 360.0;
            }
        }
    }
    if rng.gen_bool(0.015) {
        v.cog = (v.cog + rng.gen_range(-20.0..20.0) + 360.0) % 360.0;
    }
    if rng.gen_bool(0.008) {
        v.sog = (v.sog * rng.gen_range(0.88..1.12)).clamp(1.0, 22.0);
    }
}

/// A random position inside the box centred on (lat0, lon0) that is on water.
/// Falls back to shrinking the box toward the centre, then to the centre itself.
fn water_position(
    lat0: f64,
    lon0: f64,
    spread_lat: f64,
    spread_lon: f64,
    rng: &mut impl Rng,
) -> (f64, f64) {
    for _ in 0..200 {
        let lat = lat0 + rng.gen_range(-spread_lat..spread_lat);
        let lon = lon0 + rng.gen_range(-spread_lon..spread_lon);
        if !land::on_land(lat, lon) {
            return (lat, lon);
        }
    }
    for shrink in 1..=9 {
        let f = 1.0 - shrink as f64 * 0.1;
        for _ in 0..200 {
            let lat = lat0 + rng.gen_range(-spread_lat * f..spread_lat * f);
            let lon = lon0 + rng.gen_range(-spread_lon * f..spread_lon * f);
            if !land::on_land(lat, lon) {
                return (lat, lon);
            }
        }
    }
    (lat0, lon0)
}

fn utc_time() -> String {
    chrono::Utc::now().format("%H%M%S.00").to_string()
}

fn lat_dm(lat: f64) -> String {
    let a = lat.abs();
    let deg = a.floor();
    let min = (a - deg) * 60.0;
    format!("{:02}{:07.4}", deg as u32, min)
}

fn lon_dm(lon: f64) -> String {
    let a = lon.abs();
    let deg = a.floor();
    let min = (a - deg) * 60.0;
    format!("{:03}{:07.4}", deg as u32, min)
}

pub fn spawn(cfg: SimCfg, aoi: AoiCfg, tx: EventTx) -> JoinHandle<()> {
    tokio::spawn(async move {
        // StdRng (not ThreadRng) so the task future stays Send
        let mut rng = StdRng::from_entropy();
        let mut router = Router::new("simulator", Vec::new(), false);

        let total = (cfg.vessels + cfg.dark_vessels) as usize;
        let mut vessels: Vec<SimVessel> = Vec::with_capacity(total);
        for i in 0..total {
            let ais_on = i < cfg.vessels as usize;
            let (ship_type, prefix, sog_lo, sog_hi) = match i % 7 {
                0 | 1 => (30u8, "F/V", 2.5, 5.5),
                2 => (70u8, "MV", 10.0, 14.0),
                3 => (80u8, "MT", 9.0, 12.5),
                4 => (60u8, "MV", 12.0, 18.0),
                5 => (52u8, "TUG", 4.0, 7.0),
                _ => (55u8, "P/V", 14.0, 22.0),
            };
            let adj = ADJ[i % ADJ.len()];
            let noun = NOUN[(i / 2) % NOUN.len()];
            let name = if ais_on {
                format!("{prefix} {adj} {noun}")
            } else {
                format!("DARK-{:02}", i - cfg.vessels as usize + 1)
            };
            // AIS targets are scattered across the whole AOI; dark contacts are
            // placed inside own ship's sensor footprint so the detection story
            // (sonar/lidar seeing what AIS does not) is visible.
            let (lat, lon) = if ais_on {
                water_position(aoi.center_lat, aoi.center_lon, 0.28, 0.34, &mut rng)
            } else {
                water_position(
                    aoi.center_lat + 0.02,
                    aoi.center_lon - 0.02,
                    0.085,
                    0.10,
                    &mut rng,
                )
            };
            vessels.push(SimVessel {
                mmsi: if ais_on {
                    Some(232_000_000 + (i as u32) * 1117)
                } else {
                    None
                },
                callsign: format!("OS{:04}", 1000 + i),
                name,
                ship_type,
                lat,
                lon,
                cog: rng.gen_range(0.0..360.0),
                sog: rng.gen_range(sog_lo..sog_hi),
                ais: ais_on,
                gap_pattern: i == 3,
            });
        }

        let (own_lat, own_lon) = water_position(
            aoi.center_lat + 0.02,
            aoi.center_lon - 0.02,
            0.05,
            0.05,
            &mut rng,
        );
        let mut own = SimVessel {
            mmsi: Some(232_999_999),
            name: "R/V SENTINEL".to_string(),
            callsign: "OS0001".to_string(),
            ship_type: 55,
            lat: own_lat,
            lon: own_lon,
            cog: 250.0,
            sog: 8.0,
            ais: true,
            gap_pattern: false,
        };

        let interval = Duration::from_millis(cfg.interval_ms.max(200));
        let mut ticker = tokio::time::interval(interval);
        let mut tick: u64 = 0;
        let mut lines_total: u64 = 0;
        let sonar_range_m = cfg.sonar_range_km * 1000.0;
        let lidar_range_m = cfg.lidar_range_km * 1000.0;

        loop {
            ticker.tick().await;
            tick += 1;
            let dt_h = cfg.interval_ms as f64 / 3_600_000.0;

            advance(&mut own, dt_h, &mut rng);
            for v in vessels.iter_mut() {
                advance(v, dt_h, &mut rng);
            }

            // Own-ship navigation sentences (GGA/RMC/HDT) + own AIS
            if tick.is_multiple_of(2) {
                let t = utc_time();
                lines_total += 1;
                router.line(
                    &nmea_sentence(&format!(
                        "GPGGA,{t},{},{},{},{},1,09,0.9,12.0,M,0.0,M,,",
                        lat_dm(own.lat),
                        if own.lat >= 0.0 { "N" } else { "S" },
                        lon_dm(own.lon),
                        if own.lon >= 0.0 { "E" } else { "W" },
                    )),
                    &tx,
                );
                lines_total += 1;
                router.line(
                    &nmea_sentence(&format!(
                        "GPRMC,{t},A,{},{},{},{},{:.1},{:.1},010126,0.0,W",
                        lat_dm(own.lat),
                        if own.lat >= 0.0 { "N" } else { "S" },
                        lon_dm(own.lon),
                        if own.lon >= 0.0 { "E" } else { "W" },
                        own.sog,
                        own.cog,
                    )),
                    &tx,
                );
                lines_total += 1;
                router.line(&nmea_sentence(&format!("HEHDT,{:.1},T", own.cog)), &tx);
                if let Some(mmsi) = own.mmsi {
                    lines_total += 1;
                    router.line(
                        &ais::encode_position_a(
                            mmsi, own.lat, own.lon, own.sog, own.cog, own.cog, 0,
                        ),
                        &tx,
                    );
                }
            }

            // Static identity for AIS targets: once, then every 60 ticks
            if tick % 60 == 1 {
                if let Some(mmsi) = own.mmsi {
                    lines_total += 1;
                    router.line(
                        &ais::encode_static_a(
                            mmsi,
                            9_999_991,
                            &own.callsign,
                            &own.name,
                            own.ship_type,
                            20,
                            8,
                            4,
                            4,
                            "PATROL AREA",
                            2.5,
                        ),
                        &tx,
                    );
                }
                for v in vessels.iter().filter(|v| v.ais) {
                    if let Some(mmsi) = v.mmsi {
                        let (bow, stern, port, starboard, draught, dest) = match v.ship_type {
                            30 => (14, 8, 3, 3, 4.2, "FISHING GROUNDS"),
                            70 => (85, 25, 9, 9, 9.5, "ROTTERDAM"),
                            80 => (110, 40, 11, 11, 12.0, "ALGECIRAS"),
                            60 => (60, 20, 8, 8, 6.5, "TANGIER"),
                            52 => (12, 8, 3, 3, 3.0, "PORT"),
                            _ => (20, 8, 4, 4, 2.5, "PATROL"),
                        };
                        lines_total += 1;
                        router.line(
                            &ais::encode_static_a(
                                mmsi,
                                9_000_000 + (mmsi % 100_000),
                                &v.callsign,
                                &v.name,
                                v.ship_type,
                                bow,
                                stern,
                                port,
                                starboard,
                                dest,
                                draught,
                            ),
                            &tx,
                        );
                    }
                }
            }

            // Per-vessel AIS + sonar + lidar
            for (i, v) in vessels.iter().enumerate() {
                let gap = v.gap_pattern && (tick % 600) >= 300;
                if v.ais && !gap && tick % 2 == (i as u64 % 2) {
                    if let Some(mmsi) = v.mmsi {
                        lines_total += 1;
                        router.line(
                            &ais::encode_position_a(
                                mmsi,
                                v.lat,
                                v.lon,
                                v.sog,
                                v.cog,
                                v.cog,
                                if v.ship_type == 30 { 7 } else { 0 },
                            ),
                            &tx,
                        );
                    }
                }

                let d = geo::haversine_m(own.lat, own.lon, v.lat, v.lon);

                if d <= sonar_range_m && tick.is_multiple_of(2) {
                    let brg = geo::bearing_deg(own.lat, own.lon, v.lat, v.lon);
                    // range-dependent bearing error, as a real sonar tracker has
                    let err = (18.0 + 0.012 * d) * rng.gen_range(-1.0..1.0);
                    let (clat, clon) =
                        geo::destination_point(v.lat, v.lon, rng.gen_range(0.0..360.0), err.abs());
                    lines_total += 1;
                    if i % 2 == 0 {
                        router.line(
                            &nmea_sentence(&format!(
                                "SDTLL,{:02},{},{},{},{},{},{},T",
                                (i % 99) + 1,
                                lat_dm(clat),
                                if clat >= 0.0 { "N" } else { "S" },
                                lon_dm(clon),
                                if clon >= 0.0 { "E" } else { "W" },
                                v.name,
                                utc_time()
                            )),
                            &tx,
                        );
                    } else {
                        router.line(
                            &nmea_sentence(&format!(
                                "SDTTM,{:02},{:.2},{:.1},T,{:.1},{:.1},T,{:.2},{:.1},N,{},{},T",
                                (i % 99) + 1,
                                d / 1852.0,
                                brg,
                                v.sog,
                                v.cog,
                                d * 0.8 / 1852.0,
                                12.0,
                                v.name,
                                utc_time()
                            )),
                            &tx,
                        );
                    }
                }

                if d <= lidar_range_m {
                    let (jlat, jlon) = geo::destination_point(
                        v.lat,
                        v.lon,
                        rng.gen_range(0.0..360.0),
                        rng.gen_range(0.0..9.0),
                    );
                    let j = serde_json::json!({
                        "sensor": "lidar",
                        "id": v.name,
                        "lat": jlat,
                        "lon": jlon,
                        "range_m": d,
                        "bearing_deg": geo::bearing_deg(own.lat, own.lon, v.lat, v.lon),
                        "confidence": 0.9 + rng.gen_range(0.0..0.08),
                    });
                    lines_total += 1;
                    router.line(&j.to_string(), &tx);
                }
            }

            if tick.is_multiple_of(5) {
                let _ = tx.send(Event::Feed(feed_status(
                    "simulator",
                    "sim",
                    "running",
                    format!(
                        "{} targets ({} dark), sonar {:.0} km / lidar {:.0} km",
                        total, cfg.dark_vessels, cfg.sonar_range_km, cfg.lidar_range_km
                    ),
                    lines_total,
                    (total * 3) as f32,
                )));
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steering_keeps_vessels_off_land() {
        // 40 km due north of the strait is Spain: the steerer must pick a
        // different heading whose step is on water, deviating as little as
        // the coastline allows.
        let (_, _, cog) =
            steer_to_water(36.00, -5.40, 0.0, 40_000.0).expect("a water heading exists");
        assert_ne!(cog, 0.0, "due north is Spain; a deviation was required");
        let (tlat, tlon) = geo::destination_point(36.00, -5.40, cog, 40_000.0);
        assert!(!land::on_land(tlat, tlon), "steered step landed on land");
    }

    #[test]
    fn spawned_vessels_are_always_on_water() {
        let mut rng = StdRng::seed_from_u64(42);
        for _ in 0..300 {
            let (lat, lon) = water_position(36.02, -5.36, 0.28, 0.34, &mut rng);
            assert!(
                !land::on_land(lat, lon),
                "spawned on land at {lat:.4},{lon:.4}"
            );
        }
    }

    #[test]
    fn vessels_stay_on_water_when_advancing() {
        let mut rng = StdRng::seed_from_u64(7);
        let mut v = SimVessel {
            mmsi: Some(1),
            name: "TEST".into(),
            callsign: "T".into(),
            ship_type: 70,
            lat: 36.00,
            lon: -5.40,
            cog: 0.0, // heading straight at Spain
            sog: 14.0,
            ais: true,
            gap_pattern: false,
        };
        for step in 0..600 {
            advance(&mut v, 1.0 / 3600.0, &mut rng);
            assert!(
                !land::on_land(v.lat, v.lon),
                "step {step}: vessel went ashore at {:.5},{:.5}",
                v.lat,
                v.lon
            );
        }
    }
}
