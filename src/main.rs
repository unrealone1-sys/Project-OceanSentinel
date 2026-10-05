//! OceanSentinel — live maritime domain awareness.
//!
//! Ingests AIS (radio), sonar/ARPA target sentences and LiDAR contacts,
//! fuses them into live vessel tracks with dark-vessel detection, enriches
//! vessels through the Global Fishing Watch API, and serves an OSIRIS-style
//! live map over HTTP/WebSocket.

mod ais;
mod config;
mod fusion;
mod geo;
mod gfw;
mod land;
mod model;
mod nmea;
mod notify;
mod persist;
mod server;
mod sources;
mod store;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use config::Config;

#[derive(Parser, Debug)]
#[command(
    name = "oceansentinel",
    version,
    about = "Live maritime domain awareness: AIS + sonar + LiDAR sensor fusion with Global Fishing Watch enrichment and a live OSINT map"
)]
struct Args {
    /// Path to config file (defaults are written there on first run)
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
    /// Override HTTP port
    #[arg(long)]
    port: Option<u16>,
    /// Override bind host (use 0.0.0.0 to share on the LAN)
    #[arg(long)]
    host: Option<String>,
    /// Force-enable the built-in traffic simulator
    #[arg(long)]
    sim: bool,
    /// Force-disable the built-in traffic simulator
    #[arg(long)]
    no_sim: bool,
    /// Do not open a browser window on startup
    #[arg(long)]
    no_open: bool,
    /// Area of interest center, e.g. --aoi 36.02,-5.36
    #[arg(long)]
    aoi: Option<String>,
    /// Verbose (debug) logging
    #[arg(short, long)]
    verbose: bool,
}

fn open_ui(url: &str) {
    #[cfg(windows)]
    {
        // Prefer a chromeless app window via Edge/Chrome, fall back to the
        // default browser.
        let candidates = [
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ];
        for p in candidates {
            if std::path::Path::new(p).exists() {
                let spawned = std::process::Command::new(p)
                    .arg(format!("--app={url}"))
                    .arg("--window-size=1700,1000")
                    .spawn();
                if spawned.is_ok() {
                    return;
                }
            }
        }
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

/// Load `.env` from the working directory (dotenvy's own upward search), then
/// from next to the config file, then next to the executable — so `run.bat`,
/// a desktop shortcut, or a different shell cwd all find the same secrets.
/// Existing variables always win; nothing is overwritten.
fn load_env_files(config_path: &std::path::Path) {
    let _ = dotenvy::dotenv();
    if let Some(dir) = config_path.parent() {
        if !dir.as_os_str().is_empty() {
            let _ = dotenvy::from_path(dir.join(".env"));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let _ = dotenvy::from_path(dir.join(".env"));
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    load_env_files(&args.config);

    let mut cfg = Config::load(&args.config)?;
    let sim_override = if args.sim {
        Some(true)
    } else if args.no_sim {
        Some(false)
    } else {
        None
    };
    cfg.apply_overrides(
        args.port,
        args.host.clone(),
        sim_override,
        args.aoi.as_deref(),
    );

    // Production behaviour: the simulator is a fallback for empty feeds, not a
    // companion to real ones. The moment a real AIS source is configured
    // (AISStream key, or a radio feed), the demo traffic stands down — unless
    // the operator explicitly forced it with --sim/--no-sim or OS_SIM.
    let real_ais = cfg.sources.ais.enabled
        || (cfg.sources.aisstream.enabled && cfg.sources.aisstream.api_key.is_some());
    let explicit_sim = sim_override.is_some()
        || std::env::var("OS_SIM")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
    if cfg.simulation.enabled && real_ais && !explicit_sim {
        cfg.simulation.enabled = false;
        info!("real AIS feed configured — built-in simulator disabled (use --sim to keep it)");
    }

    let filter = if args.verbose {
        "oceansentinel=debug"
    } else {
        "oceansentinel=info"
    };
    let paths = persist::Paths::new(&cfg.storage.dir);
    // Console + a daily rotating file, so an unattended run leaves a trail.
    let file_appender = tracing_appender::rolling::daily(&paths.root, "oceansentinel.log");
    let (non_blocking, _log_guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter)),
        )
        .with_target(false)
        .with_writer(non_blocking)
        .init();

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (bcast_tx, _) = broadcast::channel::<server::ServerMsg>(256);
    let store = Arc::new(RwLock::new(store::Store::new(
        &cfg.fusion,
        paths.clone(),
        cfg.watchlist.entries.clone(),
    )));
    let gfw = Arc::new(gfw::GfwClient::new(&cfg.gfw));

    let meta = store::SnapshotMeta {
        trail_limit: cfg.fusion.snapshot_trail,
        version: env!("CARGO_PKG_VERSION").to_string(),
        aoi: cfg.aoi.clone(),
        gfw_enabled: gfw.enabled(),
        gfw_token: gfw.has_token(),
        simulation: cfg.simulation.enabled,
        port: cfg.server.port,
        recording: cfg.storage.record_tracks,
        alert_destinations: cfg.alerts.destinations(),
        auth_required: cfg
            .server
            .api_token
            .as_deref()
            .map(|t| !t.is_empty())
            .unwrap_or(false),
    };

    let (notify_tx, notify_rx) = mpsc::unbounded_channel();
    notify::spawn(cfg.alerts.clone(), notify_rx, paths.clone());

    tokio::spawn(
        fusion::Fusion::new(
            fusion::FusionDeps {
                store: store.clone(),
                bcast: bcast_tx.clone(),
                notify: notify_tx,
                paths: paths.clone(),
            },
            cfg.fusion.clone(),
            meta,
            &cfg.storage,
        )
        .run(event_rx),
    );

    let handles = sources::spawn_all(&cfg, &event_tx);

    if cfg.alerts.destinations().is_empty() {
        info!(
            "alert delivery: UI only (add [alerts] webhooks or Telegram to get alerts off-screen)"
        );
    }
    if cfg.storage.record_tracks {
        info!(
            "track recording: ON every {}s into {} (retention {} days)",
            cfg.storage.record_interval_s,
            paths.history.display(),
            cfg.storage.retention_days
        );
    }
    let loopback = matches!(cfg.server.host.as_str(), "127.0.0.1" | "localhost" | "::1");
    if !loopback && cfg.server.api_token.as_deref().unwrap_or("").is_empty() {
        warn!(
            "{} is not loopback and no [server] api_token is set — anyone on the network can read this map and its API",
            cfg.server.host
        );
    }

    if gfw.enabled() {
        info!("Global Fishing Watch enrichment: ENABLED");
    } else if gfw.has_token() {
        warn!("Global Fishing Watch: disabled in config");
    } else {
        info!("Global Fishing Watch: no token — set GFW_API_TOKEN in .env (free, non-commercial: https://globalfishingwatch.org/our-apis/tokens)");
    }
    info!("sensor feeds started: {}", handles.len());
    if cfg.sources.aisstream.enabled {
        if cfg.sources.aisstream.api_key.is_some() {
            let scope = if cfg.sources.aisstream.bounding_boxes.is_empty() {
                "the whole globe".to_string()
            } else {
                format!(
                    "{} bounding box(es)",
                    cfg.sources.aisstream.bounding_boxes.len()
                )
            };
            info!("global AIS (AISStream.io): ENABLED — subscribed to {scope}");
        } else {
            warn!("global AIS (AISStream.io) is enabled but AISSTREAM_API_KEY is not set — free key at https://aisstream.io");
        }
    }
    if cfg.simulation.enabled {
        let mask = land::get();
        if !mask.is_loaded() {
            warn!("land mask unavailable — simulated vessels may cross land");
        }
        info!(
            "simulator: {} vessels + {} dark contacts around {:?} (land mask: {} coastline rings)",
            cfg.simulation.vessels,
            cfg.simulation.dark_vessels,
            cfg.aoi.name,
            mask.ring_count()
        );
    }

    let addr: SocketAddr = format!("{}:{}", cfg.server.host, cfg.server.port).parse()?;
    let app = server::router(server::AppState {
        store: store.clone(),
        bcast: bcast_tx.clone(),
        gfw: gfw.clone(),
        cfg: Arc::new(cfg.clone()),
        history_cache: Arc::new(std::sync::Mutex::new(None)),
    });
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let url = format!("http://{}:{}/", cfg.server.host, cfg.server.port);
    info!("OceanSentinel map ready: {url}");
    if cfg.server.open_browser && !args.no_open {
        open_ui(&url);
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutting down");
        })
        .await?;

    let _ = handles;
    Ok(())
}
