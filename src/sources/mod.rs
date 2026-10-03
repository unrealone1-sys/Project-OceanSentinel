pub mod aisstream;
pub mod ingest;
pub mod nmea_tcp;
pub mod nmea_udp;
pub mod simulator;

use chrono::{DateTime, Utc};
use tokio::sync::mpsc;

use crate::ais::AisBody;
use crate::config::Config;
use crate::model::{Contact, FeedStatus, OwnShipFix, SensorKind};

/// Everything a sensor feed can hand to the fusion engine.
#[derive(Debug, Clone)]
pub enum Event {
    Ais(AisBody),
    Contact(Contact),
    /// Target report relative to own ship (NMEA TTM); resolved by fusion.
    RelativeTarget(RelativeTarget),
    OwnShip(OwnShipFix),
    Feed(FeedStatus),
}

#[derive(Debug, Clone)]
pub struct RelativeTarget {
    pub source: SensorKind,
    pub target: String,
    pub dist_m: f64,
    pub bearing_deg: f64,
    pub true_bearing: bool,
    pub speed_kn: Option<f64>,
    pub course_deg: Option<f64>,
    pub name: Option<String>,
    pub ts: DateTime<Utc>,
}

pub type EventTx = mpsc::UnboundedSender<Event>;

pub fn feed_status(
    name: &str,
    kind: &str,
    state: &str,
    detail: String,
    lines: u64,
    pps: f32,
) -> FeedStatus {
    FeedStatus {
        name: name.to_string(),
        kind: kind.to_string(),
        state: state.to_string(),
        detail,
        lines,
        pps,
        last_line: Some(Utc::now()),
    }
}

/// Start every configured feed plus the simulator. Returns task handles.
pub fn spawn_all(cfg: &Config, tx: &EventTx) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();
    let s = &cfg.sources;

    if s.ais.enabled {
        for addr in &s.ais.tcp {
            handles.push(nmea_tcp::spawn(
                format!("ais-tcp {addr}"),
                addr.clone(),
                tx.clone(),
                Vec::new(),
                false,
            ));
        }
        for addr in &s.ais.udp {
            handles.push(nmea_udp::spawn(
                format!("ais-udp {addr}"),
                addr.clone(),
                tx.clone(),
                Vec::new(),
                false,
            ));
        }
    }

    if s.sonar.enabled {
        for addr in &s.sonar.tcp {
            handles.push(nmea_tcp::spawn(
                format!("sonar-tcp {addr}"),
                addr.clone(),
                tx.clone(),
                s.sonar.talkers.clone(),
                false,
            ));
        }
        for addr in &s.sonar.udp {
            handles.push(nmea_udp::spawn(
                format!("sonar-udp {addr}"),
                addr.clone(),
                tx.clone(),
                s.sonar.talkers.clone(),
                false,
            ));
        }
    }

    if s.lidar.enabled {
        for addr in &s.lidar.tcp {
            handles.push(nmea_tcp::spawn(
                format!("lidar-tcp {addr}"),
                addr.clone(),
                tx.clone(),
                s.lidar.talkers.clone(),
                true,
            ));
        }
        for addr in &s.lidar.udp {
            handles.push(nmea_udp::spawn(
                format!("lidar-udp {addr}"),
                addr.clone(),
                tx.clone(),
                s.lidar.talkers.clone(),
                true,
            ));
        }
    }

    if cfg.sources.aisstream.enabled {
        handles.push(aisstream::spawn(cfg.sources.aisstream.clone(), tx.clone()));
    }

    if cfg.simulation.enabled {
        handles.push(simulator::spawn(
            cfg.simulation.clone(),
            cfg.aoi.clone(),
            tx.clone(),
        ));
    }

    handles
}
