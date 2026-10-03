//! Global live AIS via AISStream.io (WebSocket, free API key).
//!
//! Subscribe globally or to one or more bounding boxes. Messages arrive as
//! JSON envelopes; we map them onto the same `AisBody` types the NMEA decoder
//! produces, so fusion treats them identically to radio AIS.
//! Docs/keys: https://aisstream.io  (set AISSTREAM_API_KEY in .env)

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use crate::ais::AisBody;
use crate::config::AisStreamCfg;
use crate::sources::{feed_status, Event, EventTx};

const FEED: &str = "aisstream";

pub fn spawn(cfg: AisStreamCfg, tx: EventTx) -> JoinHandle<()> {
    tokio::spawn(async move {
        let key = match cfg.api_key.clone().filter(|k| !k.trim().is_empty()) {
            Some(k) => k,
            None => {
                let _ = tx.send(Event::Feed(feed_status(
                    FEED,
                    "aisstream",
                    "error",
                    "AISSTREAM_API_KEY is not set — get a free key at https://aisstream.io".to_string(),
                    0,
                    0.0,
                )));
                return;
            }
        };

        let mut total: u64 = 0;
        let mut backoff = 2u64;
        loop {
            match run_session(&cfg, &key, &tx, &mut total).await {
                Ok(()) => {
                    backoff = 2;
                    let _ = tx.send(Event::Feed(feed_status(
                        FEED,
                        "aisstream",
                        "disconnected",
                        "stream ended; reconnecting".to_string(),
                        total,
                        0.0,
                    )));
                }
                Err(e) => {
                    let _ = tx.send(Event::Feed(feed_status(
                        FEED,
                        "aisstream",
                        "error",
                        format!("{e:#}; retrying in {backoff}s"),
                        total,
                        0.0,
                    )));
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
            }
        }
    })
}

async fn run_session(cfg: &AisStreamCfg, key: &str, tx: &EventTx, total: &mut u64) -> Result<()> {
    let (ws, _resp) = tokio_tungstenite::connect_async(cfg.url.as_str())
        .await
        .context("connecting to AISStream")?;
    let (mut sink, mut stream) = ws.split();

    let boxes: Vec<Value> = cfg
        .bounding_boxes
        .iter()
        .map(|b| json!([[b[0][0], b[0][1]], [b[1][0], b[1][1]]]))
        .collect();
    let sub = json!({
        "APIKey": key,
        "BoundingBoxes": boxes,
        "FilterMessageTypes": ["PositionReport", "ShipStaticData", "StaticDataReport"],
    });
    sink.send(Message::Text(sub.to_string().into()))
        .await
        .context("sending AISStream subscription")?;

    let scope = if cfg.bounding_boxes.is_empty() {
        "global".to_string()
    } else {
        format!("{} box(es)", cfg.bounding_boxes.len())
    };
    let _ = tx.send(Event::Feed(feed_status(
        FEED,
        "aisstream",
        "connected",
        format!("subscribed ({scope})"),
        *total,
        0.0,
    )));

    let mut last_report = tokio::time::Instant::now();
    let mut last_count = *total;
    let mut decoded: u64 = 0;

    while let Some(msg) = stream.next().await {
        let msg = msg.context("AISStream read")?;
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            Message::Close(_) => break,
        };
        if text.trim().is_empty() {
            continue;
        }
        *total += 1;
        match serde_json::from_str::<Value>(&text) {
            Ok(v) => {
                if let Some(body) = body_from_aisstream(&v) {
                    decoded += 1;
                    let _ = tx.send(Event::Ais(body));
                }
            }
            Err(_) => continue,
        }
        if last_report.elapsed().as_secs() >= 5 {
            let dt = last_report.elapsed().as_secs_f32().max(0.001);
            let pps = (*total - last_count) as f32 / dt;
            let _ = tx.send(Event::Feed(feed_status(
                FEED,
                "aisstream",
                "connected",
                format!("{decoded} vessels decoded"),
                *total,
                pps,
            )));
            last_report = tokio::time::Instant::now();
            last_count = *total;
        }
    }
    Ok(())
}

/// Map an AISStream message envelope onto our AIS model.
pub fn body_from_aisstream(v: &Value) -> Option<AisBody> {
    let mtype = v.get("MessageType")?.as_str()?;
    let msg = v.get("Message")?;
    let meta = v.get("MetaData").cloned().unwrap_or(Value::Null);
    let meta_u64 = |k: &str| meta.get(k).and_then(|x| x.as_u64());
    let meta_str = |k: &str| {
        meta.get(k)
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    match mtype {
        "PositionReport" => {
            let pr = msg.get("PositionReport")?;
            let mmsi = pr
                .get("UserID")
                .and_then(|x| x.as_u64())
                .or_else(|| meta_u64("MMSI"))? as u32;
            Some(AisBody::Position {
                mmsi,
                nav_status: pr
                    .get("NavigationalStatus")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(15) as u8,
                sog: pr
                    .get("Sog")
                    .and_then(|x| x.as_f64())
                    .map(|s| s as f32)
                    .filter(|s| *s < 102.3),
                cog: pr
                    .get("Cog")
                    .and_then(|x| x.as_f64())
                    .map(|s| s as f32)
                    .filter(|s| *s < 360.0),
                heading: pr
                    .get("TrueHeading")
                    .and_then(|x| x.as_u64())
                    .filter(|h| *h < 360)
                    .map(|h| h as f32),
                lat: pr
                    .get("Latitude")
                    .and_then(|x| x.as_f64())
                    .filter(|l| l.abs() <= 90.0),
                lon: pr
                    .get("Longitude")
                    .and_then(|x| x.as_f64())
                    .filter(|l| l.abs() <= 180.0),
                maneuver: None,
                class_b: false,
            })
        }
        "ShipStaticData" => {
            let sd = msg.get("ShipStaticData")?;
            let mmsi = sd
                .get("UserID")
                .and_then(|x| x.as_u64())
                .or_else(|| meta_u64("MMSI"))? as u32;
            let dim = sd.get("Dimension").cloned().unwrap_or(Value::Null);
            let dim_u16 = |k: &str| dim.get(k).and_then(|x| x.as_u64()).unwrap_or(0) as u16;
            let (a, b, c, d) = (dim_u16("A"), dim_u16("B"), dim_u16("C"), dim_u16("D"));
            let name = sd
                .get("Name")
                .and_then(|x| x.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .or_else(|| meta_str("ShipName"));
            Some(AisBody::StaticA {
                mmsi,
                imo: sd
                    .get("ImoNumber")
                    .and_then(|x| x.as_u64())
                    .filter(|v| *v != 0)
                    .map(|v| v as u32),
                callsign: sd
                    .get("CallSign")
                    .and_then(|x| x.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                name,
                ship_type: sd
                    .get("Type")
                    .and_then(|x| x.as_u64())
                    .filter(|v| *v != 0)
                    .map(|v| v as u8),
                length: if a + b > 0 { Some(a + b) } else { None },
                beam: if c + d > 0 { Some(c + d) } else { None },
                destination: sd
                    .get("Destination")
                    .and_then(|x| x.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                draught: sd
                    .get("MaximumStaticDraught")
                    .and_then(|x| x.as_f64())
                    .map(|v| v as f32)
                    .filter(|v| *v > 0.0),
            })
        }
        "StaticDataReport" => {
            let r = msg.get("StaticDataReport")?;
            let mmsi = r.get("UserID").and_then(|x| x.as_u64())? as u32;
            if r.get("PartNumber").and_then(|x| x.as_u64()).unwrap_or(0) == 0 {
                Some(AisBody::StaticBName {
                    mmsi,
                    name: r
                        .pointer("/ReportA/Name")
                        .and_then(|x| x.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .or_else(|| meta_str("ShipName")),
                })
            } else {
                let dim = r.pointer("/ReportB/Dimension").cloned().unwrap_or(Value::Null);
                let dim_u16 = |k: &str| dim.get(k).and_then(|x| x.as_u64()).unwrap_or(0) as u16;
                let (a, b) = (dim_u16("A"), dim_u16("B"));
                Some(AisBody::StaticB {
                    mmsi,
                    ship_type: r
                        .pointer("/ReportB/ShipType")
                        .and_then(|x| x.as_u64())
                        .filter(|v| *v != 0)
                        .map(|v| v as u8),
                    callsign: r
                        .pointer("/ReportB/CallSign")
                        .and_then(|x| x.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    length: if a + b > 0 { Some(a + b) } else { None },
                    beam: {
                        let (c, d) = (dim_u16("C"), dim_u16("D"));
                        if c + d > 0 {
                            Some(c + d)
                        } else {
                            None
                        }
                    },
                })
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_position_report() {
        let v: Value = serde_json::from_str(
            r#"{"MessageType":"PositionReport",
                "MetaData":{"MMSI":244123456,"ShipName":"NORDIC STAR","latitude":36.05,"longitude":-5.31,"time_utc":"2026-10-03 08:00:00.0 +0000 UTC"},
                "Message":{"PositionReport":{"Cog":88.4,"Latitude":36.0512,"Longitude":-5.3098,"NavigationalStatus":0,"Sog":9.1,"TrueHeading":91,"UserID":244123456}}}"#,
        )
        .unwrap();
        match body_from_aisstream(&v).unwrap() {
            AisBody::Position {
                mmsi,
                lat,
                lon,
                sog,
                cog,
                heading,
                nav_status,
                ..
            } => {
                assert_eq!(mmsi, 244123456);
                assert!((lat.unwrap() - 36.0512).abs() < 1e-6);
                assert!((lon.unwrap() + 5.3098).abs() < 1e-6);
                assert!((sog.unwrap() - 9.1).abs() < 0.01);
                assert!((cog.unwrap() - 88.4).abs() < 0.01);
                assert_eq!(heading, Some(91.0));
                assert_eq!(nav_status, 0);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_ship_static_data() {
        let v: Value = serde_json::from_str(
            r#"{"MessageType":"ShipStaticData",
                "MetaData":{"MMSI":257123456,"ShipName":"BALTIC TRADER"},
                "Message":{"ShipStaticData":{"UserID":257123456,"ImoNumber":9123456,"CallSign":"LAAA",
                  "Name":"BALTIC TRADER","Type":70,"Destination":"ROTTERDAM","MaximumStaticDraught":9.5,
                  "Dimension":{"A":85,"B":25,"C":9,"D":9}}}}"#,
        )
        .unwrap();
        match body_from_aisstream(&v).unwrap() {
            AisBody::StaticA {
                mmsi,
                name,
                ship_type,
                length,
                beam,
                destination,
                draught,
                imo,
                ..
            } => {
                assert_eq!(mmsi, 257123456);
                assert_eq!(name.as_deref(), Some("BALTIC TRADER"));
                assert_eq!(ship_type, Some(70));
                assert_eq!(length, Some(110));
                assert_eq!(beam, Some(18));
                assert_eq!(destination.as_deref(), Some("ROTTERDAM"));
                assert!((draught.unwrap() - 9.5).abs() < 0.01);
                assert_eq!(imo, Some(9123456));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_class_b_static_report() {
        let v: Value = serde_json::from_str(
            r#"{"MessageType":"StaticDataReport","MetaData":{"MMSI":232012345},
                "Message":{"StaticDataReport":{"UserID":232012345,"PartNumber":0,"ReportA":{"Name":"SEA BREEZE"}}}}"#,
        )
        .unwrap();
        match body_from_aisstream(&v).unwrap() {
            AisBody::StaticBName { mmsi, name } => {
                assert_eq!(mmsi, 232012345);
                assert_eq!(name.as_deref(), Some("SEA BREEZE"));
            }
            other => panic!("unexpected {other:?}"),
        }
        let part_b: Value = serde_json::from_str(
            r#"{"MessageType":"StaticDataReport","MetaData":{"MMSI":232012345},
                "Message":{"StaticDataReport":{"UserID":232012345,"PartNumber":1,
                  "ReportB":{"ShipType":36,"CallSign":"OS1234","Dimension":{"A":8,"B":4,"C":2,"D":2}}}}}"#,
        )
        .unwrap();
        match body_from_aisstream(&part_b).unwrap() {
            AisBody::StaticB { mmsi, ship_type, length, beam, .. } => {
                assert_eq!(mmsi, 232012345);
                assert_eq!(ship_type, Some(36));
                assert_eq!(length, Some(12));
                assert_eq!(beam, Some(4));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn ignores_unknown_envelope() {
        let v: Value = serde_json::from_str(r#"{"MessageType":"Welcome","Message":{}}"#).unwrap();
        assert!(body_from_aisstream(&v).is_none());
    }
}
