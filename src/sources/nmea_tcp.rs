use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use crate::sources::ingest::Router;
use crate::sources::{feed_status, Event, EventTx};

/// Connect to a line-oriented NMEA/JSON feed (e.g. AIS-catcher `-o 5 host port`),
/// auto-reconnecting with a 5 s backoff.
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

        loop {
            match TcpStream::connect(&addr).await {
                Ok(stream) => {
                    let _ = tx.send(Event::Feed(feed_status(
                        &name,
                        "tcp",
                        "connected",
                        format!("connected to {addr}"),
                        lines_total,
                        0.0,
                    )));
                    let mut reader = BufReader::new(stream);
                    let mut buf: Vec<u8> = Vec::with_capacity(512);
                    loop {
                        buf.clear();
                        match reader.read_until(b'\n', &mut buf).await {
                            Ok(0) => break,
                            Ok(_) => {
                                lines_total += 1;
                                let line = String::from_utf8_lossy(&buf).to_string();
                                router.line(&line, &tx);
                            }
                            Err(e) => {
                                let _ = tx.send(Event::Feed(feed_status(
                                    &name,
                                    "tcp",
                                    "error",
                                    format!("{addr}: {e}"),
                                    lines_total,
                                    0.0,
                                )));
                                break;
                            }
                        }
                        if last_report.elapsed().as_secs() >= 5 {
                            let dt = last_report.elapsed().as_secs_f32().max(0.001);
                            let pps = (lines_total - last_count) as f32 / dt;
                            let _ = tx.send(Event::Feed(feed_status(
                                &name,
                                "tcp",
                                "connected",
                                addr.to_string(),
                                lines_total,
                                pps,
                            )));
                            last_report = tokio::time::Instant::now();
                            last_count = lines_total;
                        }
                    }
                    let _ = tx.send(Event::Feed(feed_status(
                        &name,
                        "tcp",
                        "disconnected",
                        format!("{addr} closed; retrying in 5s"),
                        lines_total,
                        0.0,
                    )));
                }
                Err(e) => {
                    let _ = tx.send(Event::Feed(feed_status(
                        &name,
                        "tcp",
                        "error",
                        format!("{addr}: {e}; retrying in 5s"),
                        lines_total,
                        0.0,
                    )));
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    })
}
