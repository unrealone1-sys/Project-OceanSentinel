use std::time::Duration;

use tokio::task::JoinHandle;

use crate::sources::ingest::Router;
use crate::sources::{feed_status, Event, EventTx};

/// Listen on a UDP port for NMEA lines (AIS-catcher `-o 3`) or LiDAR contact
/// JSON lines. Several datagrams can arrive per packet; each line is routed.
pub fn spawn(
    name: String,
    addr: String,
    tx: EventTx,
    talkers: Vec<String>,
    lidar: bool,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut router = Router::new(name.clone(), talkers, lidar);
        let mut lines_total: u64 = 0;
        let mut last_report = tokio::time::Instant::now();
        let mut last_count: u64 = 0;

        let sock = match tokio::net::UdpSocket::bind(&addr).await {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.send(Event::Feed(feed_status(
                    &name,
                    "udp",
                    "error",
                    format!("bind {addr} failed: {e}"),
                    0,
                    0.0,
                )));
                return;
            }
        };
        let _ = tx.send(Event::Feed(feed_status(
            &name,
            "udp",
            "listening",
            format!("bound {addr}"),
            0,
            0.0,
        )));

        let mut buf = vec![0u8; 65_535];
        loop {
            match tokio::time::timeout(Duration::from_secs(5), sock.recv_from(&mut buf)).await {
                Ok(Ok((n, _peer))) => {
                    let text = String::from_utf8_lossy(&buf[..n]).to_string();
                    for line in text.lines() {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        lines_total += 1;
                        router.line(line, &tx);
                    }
                }
                Ok(Err(e)) => {
                    let _ = tx.send(Event::Feed(feed_status(
                        &name,
                        "udp",
                        "error",
                        format!("{addr}: {e}"),
                        lines_total,
                        0.0,
                    )));
                }
                Err(_) => {} // idle: keep listening
            }
            if last_report.elapsed().as_secs() >= 5 {
                let dt = last_report.elapsed().as_secs_f32().max(0.001);
                let pps = (lines_total - last_count) as f32 / dt;
                let _ = tx.send(Event::Feed(feed_status(
                    &name,
                    "udp",
                    "listening",
                    format!("bound {addr}"),
                    lines_total,
                    pps,
                )));
                last_report = tokio::time::Instant::now();
                last_count = lines_total;
            }
        }
    })
}
