//! HTTP + WebSocket server: REST API for GFW enrichment and zones, a live
//! WebSocket feed for the map, and the embedded UI (single-exe deployment).

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, RwLock};
use tracing::warn;

use crate::config::Config;
use crate::gfw::GfwClient;
use crate::model::Zone;
use crate::store::{SnapshotMeta, Store};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<RwLock<Store>>,
    pub bcast: broadcast::Sender<String>,
    pub gfw: Arc<GfwClient>,
    pub cfg: Arc<Config>,
}

#[derive(RustEmbed)]
#[folder = "ui/"]
struct Assets;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/state", get(api_state))
        .route("/api/gfw/vessel", get(gfw_vessel))
        .route("/api/gfw/events", get(gfw_events))
        .route("/api/zones", post(add_zone).delete(delete_zone))
        .route("/ws", get(ws_handler))
        .fallback(static_handler)
        .with_state(state)
}

fn meta(st: &AppState) -> SnapshotMeta {
    SnapshotMeta {
        trail_limit: st.cfg.fusion.snapshot_trail,
        version: env!("CARGO_PKG_VERSION").to_string(),
        aoi: st.cfg.aoi.clone(),
        gfw_enabled: st.gfw.enabled(),
        gfw_token: st.gfw.has_token(),
        simulation: st.cfg.simulation.enabled,
        port: st.cfg.server.port,
    }
}

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(m: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::BAD_REQUEST,
            message: m.into(),
        }
    }
    fn not_found(m: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::NOT_FOUND,
            message: m.into(),
        }
    }
    fn bad_gateway(e: anyhow::Error) -> Self {
        ApiError {
            status: StatusCode::BAD_GATEWAY,
            // {:#} keeps the full source chain (timeout vs TLS vs HTTP status)
            message: format!("{e:#}"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true, "app": "oceansentinel", "version": env!("CARGO_PKG_VERSION")}))
}

async fn api_state(State(st): State<AppState>) -> Json<Value> {
    let store = st.store.read().await;
    let m = meta(&st);
    Json(store.snapshot(&m))
}

#[derive(Deserialize)]
struct VesselQuery {
    mmsi: Option<String>,
    imo: Option<String>,
    name: Option<String>,
    id: Option<String>,
}

fn extract_vessel_id(v: &Value) -> Option<String> {
    v.pointer("/entries/0/combinedSourcesInfo/0/vesselId")
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .or_else(|| {
            v.pointer("/entries/0/selfReportedInfo/0/id")
                .and_then(|x| x.as_str())
                .map(str::to_string)
        })
}

async fn gfw_vessel(
    State(st): State<AppState>,
    Query(q): Query<VesselQuery>,
) -> Result<Json<Value>, ApiError> {
    if !st.gfw.enabled() {
        return Err(ApiError::bad_request(
            "Global Fishing Watch enrichment is not active. Set GFW_API_TOKEN in .env (free non-commercial token: https://globalfishingwatch.org/our-apis/tokens)",
        ));
    }

    let mut search = Value::Null;
    let mut vessel_id = q.id.clone().filter(|s| !s.trim().is_empty());
    if vessel_id.is_none() {
        let query = q
            .mmsi
            .clone()
            .or(q.imo.clone())
            .or(q.name.clone())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ApiError::bad_request("provide mmsi, imo, name or id"))?;
        search = st.gfw.search_vessel(&query).await.map_err(ApiError::bad_gateway)?;
        vessel_id = extract_vessel_id(&search);
    }
    let id = vessel_id.ok_or_else(|| ApiError::not_found("no GFW vessel matched that query"))?;

    let detail = st.gfw.vessel_detail(&id).await.unwrap_or(Value::Null);
    let insights_raw = st.gfw.vessel_insights(&id, 365).await.unwrap_or(Value::Null);

    let summary = if search.is_null() {
        crate::gfw::summarize_search_entry(&detail)
    } else {
        crate::gfw::summarize_search_entry(&search)
    };
    let insights = if insights_raw.is_null() {
        Value::Null
    } else {
        crate::gfw::summarize_insights(&insights_raw)
    };

    let payload = json!({
        "vessel_id": id,
        "summary": summary,
        "insights": insights,
        "fetched_at": Utc::now(),
        "search": search,
        "detail": detail,
        "insights_raw": insights_raw,
    });

    // Cache into the live track so the map shows it on hover/click too.
    if let Some(mmsi) = q
        .mmsi
        .as_deref()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .or_else(|| {
            summary
                .get("mmsi")
                .and_then(|x| x.as_str())
                .and_then(|s| s.trim().parse::<u32>().ok())
        })
    {
        let mut store = st.store.write().await;
        let tid = store.mmsi_index.get(&mmsi).cloned();
        if let Some(tid) = tid {
            if let Some(t) = store.tracks.get_mut(&tid) {
                t.gfw = Some(payload.clone());
                t.gfw_at = Some(Utc::now());
            }
        }
    }

    Ok(Json(payload))
}

#[derive(Deserialize)]
struct EventsQuery {
    bbox: Option<String>,
    days: Option<i64>,
    limit: Option<u32>,
}

async fn gfw_events(
    State(st): State<AppState>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<Value>, ApiError> {
    if !st.gfw.enabled() {
        return Err(ApiError::bad_request(
            "Global Fishing Watch enrichment is not active. Set GFW_API_TOKEN in .env",
        ));
    }
    let bbox_str = q
        .bbox
        .clone()
        .ok_or_else(|| ApiError::bad_request("bbox=west,south,east,north is required"))?;
    let parts: Vec<f64> = bbox_str
        .split(',')
        .filter_map(|v| v.trim().parse::<f64>().ok())
        .collect();
    if parts.len() != 4 {
        return Err(ApiError::bad_request(
            "bbox must be four numbers: west,south,east,north",
        ));
    }
    let bbox = [parts[0], parts[1], parts[2], parts[3]];
    let (events, raw) = st
        .gfw
        .fishing_events_bbox(bbox, q.days.unwrap_or(14), q.limit.unwrap_or(250))
        .await
        .map_err(ApiError::bad_gateway)?;

    let count = events.len();
    let total = raw.get("total").and_then(|x| x.as_u64());
    {
        let mut store = st.store.write().await;
        store.gfw_events = events.clone();
    }
    Ok(Json(json!({
        "count": count,
        "total": total,
        "bbox": bbox,
        "days": q.days.unwrap_or(14),
        "events": events,
    })))
}

#[derive(Deserialize)]
struct ZoneIn {
    name: Option<String>,
    polygon: Vec<[f64; 2]>,
    color: Option<String>,
}

async fn add_zone(
    State(st): State<AppState>,
    Json(z): Json<ZoneIn>,
) -> Result<Json<Value>, ApiError> {
    if z.polygon.len() < 3 {
        return Err(ApiError::bad_request("polygon needs at least 3 points"));
    }
    let zone = Zone {
        id: uuid::Uuid::new_v4().to_string(),
        name: z.name.unwrap_or_else(|| {
            let n = st.cfg.aoi.name.clone().unwrap_or_else(|| "AOI".to_string());
            format!("Zone {n} {}", Utc::now().format("%H:%M:%S"))
        }),
        polygon: z.polygon,
        color: z.color.unwrap_or_else(|| "#38bdf8".to_string()),
        created: Utc::now(),
    };
    {
        let mut store = st.store.write().await;
        store.zones.push(zone.clone());
    }
    let _ = st.bcast.send(json!({"type": "zones"}).to_string());
    Ok(Json(json!({"zone": zone})))
}

#[derive(Deserialize)]
struct ZoneQuery {
    id: String,
}

async fn delete_zone(
    State(st): State<AppState>,
    Query(q): Query<ZoneQuery>,
) -> Result<Json<Value>, ApiError> {
    let mut store = st.store.write().await;
    let before = store.zones.len();
    store.zones.retain(|z| z.id != q.id);
    let removed = before - store.zones.len();
    drop(store);
    let _ = st.bcast.send(json!({"type": "zones"}).to_string());
    Ok(Json(json!({"removed": removed})))
}

async fn ws_handler(ws: WebSocketUpgrade, State(st): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| ws_loop(socket, st))
}

async fn ws_loop(socket: WebSocket, st: AppState) {
    let (mut sender, mut receiver) = socket.split();

    {
        let store = st.store.read().await;
        let snap = store.snapshot(&meta(&st)).to_string();
        drop(store);
        if sender.send(Message::Text(snap.into())).await.is_err() {
            return;
        }
    }

    let mut rx = st.bcast.subscribe();
    let mut send_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    if sender.send(Message::Text(msg.into())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("ws client lagged {n} messages");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if let Message::Close(_) = msg {
                break;
            }
        }
    });

    tokio::select! {
        _ = &mut send_task => recv_task.abort(),
        _ = &mut recv_task => send_task.abort(),
    }
}

async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path) {
        Some(f) => (
            [(header::CONTENT_TYPE, mime_for(path))],
            f.data.into_owned(),
        )
            .into_response(),
        None => match Assets::get("index.html") {
            Some(f) => (
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                f.data.into_owned(),
            )
                .into_response(),
            None => (StatusCode::NOT_FOUND, "ui not embedded").into_response(),
        },
    }
}

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}
