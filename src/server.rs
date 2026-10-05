//! HTTP + WebSocket server: REST API for GFW enrichment and zones, a live
//! WebSocket feed for the map, and the embedded UI (single-exe deployment).

use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, RwLock};
use tracing::warn;

use crate::config::Config;
use crate::gfw::GfwClient;
use crate::model::{WatchEntry, Zone};
use crate::persist::{self, HistoryPoint};
use crate::store::{SnapshotMeta, Store};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<RwLock<Store>>,
    pub bcast: broadcast::Sender<ServerMsg>,
    pub gfw: Arc<GfwClient>,
    pub cfg: Arc<Config>,
    /// Parsed history per day, so the replay scrubber can scrub without
    /// re-reading files.
    pub history_cache: Arc<Mutex<Option<CachedHistory>>>,
}

pub struct CachedHistory {
    day: String,
    modified: Option<std::time::SystemTime>,
    batches: Vec<(DateTime<Utc>, Vec<HistoryPoint>)>,
}

/// Everything fanned out to WebSocket clients. State is shared (Arc) because
/// each connection filters it to its own viewport before serializing.
#[derive(Clone)]
pub enum ServerMsg {
    State(Arc<Value>),
    Alert(Value),
    Zones,
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
        .route(
            "/api/watchlist",
            get(list_watch).post(add_watch).delete(delete_watch),
        )
        .route("/api/history", get(api_history))
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
        recording: st.cfg.storage.record_tracks,
        alert_destinations: st.cfg.alerts.destinations(),
        auth_required: st
            .cfg
            .server
            .api_token
            .as_deref()
            .map(|t| !t.is_empty())
            .unwrap_or(false),
    }
}

/// Token check for every API and WebSocket request. Loopback-only deployments
/// without a configured token stay wide open, which is the local default.
fn guard(st: &AppState, headers: &HeaderMap, token: Option<&str>) -> Result<(), ApiError> {
    let Some(expected) = st.cfg.server.api_token.as_deref().filter(|t| !t.is_empty()) else {
        return Ok(());
    };
    if let Some(h) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(t) = h.strip_prefix("Bearer ") {
            if constant_eq(t.trim(), expected) {
                return Ok(());
            }
        }
    }
    if let Some(t) = token {
        if constant_eq(t, expected) {
            return Ok(());
        }
    }
    Err(ApiError {
        status: StatusCode::UNAUTHORIZED,
        message: "missing or invalid API token".to_string(),
    })
}

/// Length-aware constant-time comparison, so token checks do not leak by timing.
fn constant_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Deserialize)]
struct TokenQuery {
    token: Option<String>,
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

async fn health(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(_q): Query<TokenQuery>,
) -> Json<Value> {
    Json(json!({
        "ok": true,
        "app": "oceansentinel",
        "version": env!("CARGO_PKG_VERSION"),
        "auth_required": st.cfg.server.api_token.as_deref().map(|t| !t.is_empty()).unwrap_or(false),
        "authenticated": guard(&st, &headers, None).is_ok(),
    }))
}

async fn api_state(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Result<Json<Value>, ApiError> {
    guard(&st, &headers, q.token.as_deref())?;
    let store = st.store.read().await;
    let m = meta(&st);
    Ok(Json(store.snapshot(&m)))
}

#[derive(Deserialize)]
struct VesselQuery {
    mmsi: Option<String>,
    imo: Option<String>,
    name: Option<String>,
    id: Option<String>,
    token: Option<String>,
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
    headers: HeaderMap,
    Query(q): Query<VesselQuery>,
) -> Result<Json<Value>, ApiError> {
    guard(&st, &headers, q.token.as_deref())?;
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
        search = st
            .gfw
            .search_vessel(&query)
            .await
            .map_err(ApiError::bad_gateway)?;
        vessel_id = extract_vessel_id(&search);
    }
    let id = vessel_id.ok_or_else(|| ApiError::not_found("no GFW vessel matched that query"))?;

    let detail = st.gfw.vessel_detail(&id).await.unwrap_or(Value::Null);
    let insights_raw = st
        .gfw
        .vessel_insights(&id, 365)
        .await
        .unwrap_or(Value::Null);

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
    /// Comma-separated: FISHING, ENCOUNTER, LOITERING, GAP, PORT_VISIT
    types: Option<String>,
    token: Option<String>,
}

async fn gfw_events(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<EventsQuery>,
) -> Result<Json<Value>, ApiError> {
    guard(&st, &headers, q.token.as_deref())?;
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
    let types: Vec<String> = q
        .types
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let days = q.days.unwrap_or(14);
    let (events, raw, accepted) = st
        .gfw
        .events_bbox(bbox, days, q.limit.unwrap_or(250), &types)
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
        "days": days,
        "types": accepted,
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
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
    Json(z): Json<ZoneIn>,
) -> Result<Json<Value>, ApiError> {
    guard(&st, &headers, q.token.as_deref())?;
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
        store.save_zones();
    }
    let _ = st.bcast.send(ServerMsg::Zones);
    Ok(Json(json!({"zone": zone})))
}

#[derive(Deserialize)]
struct ZoneQuery {
    id: String,
    token: Option<String>,
}

async fn delete_zone(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ZoneQuery>,
) -> Result<Json<Value>, ApiError> {
    guard(&st, &headers, q.token.as_deref())?;
    let mut store = st.store.write().await;
    let before = store.zones.len();
    store.zones.retain(|z| z.id != q.id);
    let removed = before - store.zones.len();
    store.save_zones();
    drop(store);
    let _ = st.bcast.send(ServerMsg::Zones);
    Ok(Json(json!({"removed": removed})))
}

// ---------------------------------------------------------------- watchlist

async fn list_watch(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Result<Json<Value>, ApiError> {
    guard(&st, &headers, q.token.as_deref())?;
    let store = st.store.read().await;
    Ok(Json(json!({"watchlist": store.watchlist})))
}

#[derive(Deserialize)]
struct WatchIn {
    mmsi: Option<u32>,
    imo: Option<u32>,
    name: Option<String>,
    note: Option<String>,
}

async fn add_watch(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
    Json(w): Json<WatchIn>,
) -> Result<Json<Value>, ApiError> {
    guard(&st, &headers, q.token.as_deref())?;
    let name = w
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    if w.mmsi.is_none() && w.imo.is_none() && name.is_none() {
        return Err(ApiError::bad_request(
            "give at least one of mmsi, imo or name to watch for",
        ));
    }
    let entry = WatchEntry {
        id: uuid::Uuid::new_v4().to_string(),
        mmsi: w.mmsi,
        imo: w.imo,
        name,
        note: w.note.filter(|n| !n.trim().is_empty()),
        added: Utc::now(),
    };
    {
        let mut store = st.store.write().await;
        store.watchlist.push(entry.clone());
        store.save_watchlist();
    }
    Ok(Json(json!({"entry": entry})))
}

async fn delete_watch(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ZoneQuery>,
) -> Result<Json<Value>, ApiError> {
    guard(&st, &headers, q.token.as_deref())?;
    let mut store = st.store.write().await;
    let before = store.watchlist.len();
    store.watchlist.retain(|w| w.id != q.id);
    let removed = before - store.watchlist.len();
    store.save_watchlist();
    Ok(Json(json!({"removed": removed})))
}

// ------------------------------------------------------------------ replay

#[derive(Deserialize)]
struct HistoryQuery {
    /// Unix seconds or RFC3339; defaults to now.
    ts: Option<String>,
    token: Option<String>,
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(secs) = s.parse::<i64>() {
        return Utc.timestamp_opt(secs, 0).single();
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

async fn api_history(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    guard(&st, &headers, q.token.as_deref())?;
    if !st.cfg.storage.record_tracks {
        return Err(ApiError::bad_request(
            "track recording is off — set [storage] record_tracks = true to enable replay",
        ));
    }
    let target = q.ts.as_deref().and_then(parse_ts).unwrap_or_else(Utc::now);
    let day = persist::day_key(target);
    let path = {
        let store = st.store.read().await;
        store.paths.history_day(&day)
    };

    // Parse the day's file once and cache it; the scrubber then moves in memory.
    let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let batches = {
        let mut cache = st
            .history_cache
            .lock()
            .map_err(|_| ApiError::bad_request("history cache poisoned"))?;
        let stale = match cache.as_ref() {
            Some(c) => c.day != day || c.modified != modified,
            None => true,
        };
        if stale {
            let lines = persist::load_lines(&path);
            *cache = Some(CachedHistory {
                day: day.clone(),
                modified,
                batches: group_batches(&lines),
            });
        }
        cache
            .as_ref()
            .map(|c| c.batches.clone())
            .unwrap_or_default()
    };

    if batches.is_empty() {
        return Ok(Json(json!({
            "ts": target, "points": [], "count": 0,
            "note": format!("no recorded positions for {day}"),
        })));
    }
    let from = batches.first().map(|b| b.0).unwrap_or(target);
    let to = batches.last().map(|b| b.0).unwrap_or(target);
    let (ts, points) = batches
        .iter()
        .min_by_key(|(t, _)| (*t - target).num_seconds().abs())
        .map(|(t, p)| (*t, p.clone()))
        .unwrap_or((target, Vec::new()));

    Ok(Json(json!({
        "ts": ts,
        "requested": target,
        "from": from,
        "to": to,
        "count": points.len(),
        "points": points,
    })))
}

/// Group JSONL history lines into (timestamp, points) batches — the recorder
/// writes one batch per interval, so this is a linear scan.
fn group_batches(lines: &[String]) -> Vec<(DateTime<Utc>, Vec<HistoryPoint>)> {
    let mut out: Vec<(DateTime<Utc>, Vec<HistoryPoint>)> = Vec::new();
    for line in lines {
        let Ok(p) = serde_json::from_str::<HistoryPoint>(line) else {
            continue;
        };
        match out.last_mut() {
            Some((ts, pts)) if *ts == p.ts => pts.push(p),
            _ => out.push((p.ts, vec![p])),
        }
    }
    out
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    if guard(&st, &headers, q.token.as_deref()).is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            "missing or invalid API token (append ?token=... to the WebSocket URL)",
        )
            .into_response();
    }
    ws.on_upgrade(move |socket| ws_loop(socket, st))
}

/// Viewport the client is looking at, used to trim snapshots.
#[derive(Clone, Copy)]
struct Bounds {
    w: f64,
    s: f64,
    e: f64,
    n: f64,
}

fn parse_bounds(v: &Value) -> Option<Bounds> {
    let b = v.get("bounds")?;
    let g = |k: &str| b.get(k).and_then(|x| x.as_f64());
    Some(Bounds {
        w: g("w")?,
        s: g("s")?,
        e: g("e")?,
        n: g("n")?,
    })
}

/// Trim a snapshot to the client's viewport. Zoomed-out views keep everything
/// (clustering handles the density); zoomed-in views drop what cannot be seen.
fn filter_snapshot(snapshot: Arc<Value>, bounds: Option<Bounds>) -> String {
    let Some(b) = bounds else {
        return snapshot.to_string();
    };
    // pad by a quarter of the span so tracks do not pop at the edges
    let (dw, dh) = ((b.e - b.w) * 0.25, (b.n - b.s) * 0.25);
    let (w, s, e, n) = (b.w - dw, b.s - dh, b.e + dw, b.n + dh);
    let covers_world = (e - w) >= 200.0 || (n - s) >= 100.0;

    let total = snapshot
        .get("tracks")
        .and_then(|t| t.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    if covers_world || total <= 400 {
        if covers_world {
            // sending the whole planet: trails would dominate the payload, and
            // at world zoom they are sub-pixel anyway
            let mut trimmed = (*snapshot).clone();
            if let Some(arr) = trimmed.get_mut("tracks").and_then(|t| t.as_array_mut()) {
                for t in arr.iter_mut() {
                    if let Some(obj) = t.as_object_mut() {
                        obj.insert("trail".to_string(), json!([]));
                    }
                }
            }
            return trimmed.to_string();
        }
        return snapshot.to_string();
    }

    let mut out = (*snapshot).clone();
    let taken = std::mem::take(out.get_mut("tracks").unwrap_or(&mut Value::Null));
    let kept: Vec<Value> = taken
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|t| {
                    let lat = t.get("lat").and_then(|x| x.as_f64()).unwrap_or(f64::NAN);
                    let lon = t.get("lon").and_then(|x| x.as_f64()).unwrap_or(f64::NAN);
                    lon >= w && lon <= e && lat >= s && lat <= n
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let hidden = total - kept.len();
    if let Some(obj) = out.as_object_mut() {
        obj.insert("tracks".to_string(), Value::Array(kept));
        obj.insert("hidden_tracks".to_string(), json!(hidden));
    }
    out.to_string()
}

async fn ws_loop(socket: WebSocket, st: AppState) {
    let (mut sender, mut receiver) = socket.split();

    {
        let store = st.store.read().await;
        let snap = Arc::new(store.snapshot(&meta(&st)));
        drop(store);
        if sender
            .send(Message::Text(snap.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
    }

    let mut rx = st.bcast.subscribe();
    let mut viewport: Option<Bounds> = None;

    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Ok(ServerMsg::State(snap)) => {
                    let text = filter_snapshot(snap, viewport);
                    if sender.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Ok(ServerMsg::Alert(alert)) => {
                    let text = json!({"type": "alert", "alert": alert}).to_string();
                    if sender.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Ok(ServerMsg::Zones) => {
                    let text = json!({"type": "zones"}).to_string();
                    if sender.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("ws client lagged {n} messages");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            incoming = receiver.next() => match incoming {
                Some(Ok(Message::Text(t))) => {
                    if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                        if v.get("type").and_then(|x| x.as_str()) == Some("viewport") {
                            viewport = if v.get("bounds").map(|b| !b.is_null()).unwrap_or(false) {
                                parse_bounds(&v)
                            } else {
                                None
                            };
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {}
            },
        }
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
