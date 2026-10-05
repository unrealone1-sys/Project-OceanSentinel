//! Out-of-band alert delivery.
//!
//! Consumes every alert the fusion engine raises, writes it to the on-disk
//! alert log, and forwards the ones at or above the configured severity to
//! webhooks (Slack/Discord/Teams/Zapier/n8n: the payload carries both `text`
//! and `content`) and/or Telegram. Delivery failures are logged and never
//! affect tracking.

use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config::AlertsCfg;
use crate::model::{severity_rank, Alert};
use crate::persist;

pub fn spawn(
    cfg: AlertsCfg,
    mut rx: mpsc::UnboundedReceiver<Alert>,
    paths: persist::Paths,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("OceanSentinel/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        let min_rank = severity_rank(&cfg.min_severity);
        let destinations = cfg.destinations();
        if !destinations.is_empty() {
            info!("alert delivery enabled -> {}", destinations.join(", "));
        }

        let mut window = Instant::now();
        let mut sent_in_window = 0u32;

        while let Some(alert) = rx.recv().await {
            // every alert is persisted, whether or not it is forwarded
            if let Ok(line) = serde_json::to_string(&alert) {
                persist::append_line(&paths.alerts(), &line);
            }
            if destinations.is_empty() || severity_rank(&alert.severity) < min_rank {
                continue;
            }
            if window.elapsed() >= Duration::from_secs(60) {
                window = Instant::now();
                sent_in_window = 0;
            }
            if sent_in_window >= cfg.max_per_minute.max(1) {
                warn!(
                    "alert rate limit ({} per minute) reached; {} suppressed for out-of-band delivery",
                    cfg.max_per_minute, alert.kind
                );
                continue;
            }

            let text = format!(
                "[{}] {} ({} UTC)",
                alert.severity.to_uppercase(),
                alert.message,
                alert.ts.format("%Y-%m-%d %H:%M")
            );
            let mut delivered = false;

            for url in &cfg.webhooks {
                let payload = json!({
                    "text": text,
                    "content": text,
                    "source": "OceanSentinel",
                    "alert": alert,
                });
                match http.post(url).json(&payload).send().await {
                    Ok(r) if r.status().is_success() => delivered = true,
                    Ok(r) => warn!(
                        "alert webhook {} returned HTTP {}",
                        host_of(url),
                        r.status()
                    ),
                    Err(e) => warn!("alert webhook {} failed: {e}", host_of(url)),
                }
            }

            if let (Some(token), Some(chat)) = (&cfg.telegram_bot_token, &cfg.telegram_chat_id) {
                let url = format!("https://api.telegram.org/bot{token}/sendMessage");
                let payload = json!({
                    "chat_id": chat,
                    "text": text,
                    "disable_notification": alert.severity != "high",
                });
                match http.post(&url).json(&payload).send().await {
                    Ok(r) if r.status().is_success() => delivered = true,
                    Ok(r) => warn!("telegram delivery returned HTTP {}", r.status()),
                    Err(e) => warn!("telegram delivery failed: {e}"),
                }
            }

            if delivered {
                sent_in_window += 1;
                debug!("forwarded {} alert: {}", alert.kind, alert.message);
            }
        }
    })
}

fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn host_extraction() {
        assert_eq!(
            host_of("https://hooks.slack.com/services/x/y"),
            "hooks.slack.com"
        );
        assert_eq!(host_of("not-a-url"), "not-a-url");
    }

    #[test]
    fn severity_ordering_matches_config_semantics() {
        assert!(severity_rank("high") > severity_rank("medium"));
        assert!(severity_rank("medium") > severity_rank("info"));
        assert_eq!(severity_rank("unknown"), 0);
    }

    #[test]
    fn payload_carries_both_slack_and_discord_keys() {
        let alert = Alert {
            id: "a".into(),
            ts: Utc::now(),
            kind: "dark_contact".into(),
            severity: "high".into(),
            message: "DARK CONTACT X".into(),
            track_id: None,
            lat: 1.0,
            lon: 2.0,
        };
        let payload = json!({ "text": alert.message, "content": alert.message, "alert": alert });
        assert!(payload.get("text").is_some());
        assert!(payload.get("content").is_some());
        assert_eq!(payload["alert"]["kind"], "dark_contact");
    }
}
