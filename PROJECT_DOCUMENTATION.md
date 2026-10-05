# OceanSentinel — Project Documentation

| | |
|---|---|
| **Project** | OceanSentinel — Live Maritime Domain Awareness |
| **Version** | 1.0 (aligned with commit `3a4c3eb`) |
| **Date** | 2026-10-05 |
| **Platform** | Windows 10/11 (single self-contained executable) |
| **Licence** | MIT (see `LICENSE`) |
| **Quick start** | See `README.md` — this document is the full technical reference |

---

## 1. Executive summary

OceanSentinel is a desktop maritime domain awareness (MDA) application that
fuses multiple vessel-detection sources into one live operational picture:

* **AIS** — vessel identity and position radio, ingested three ways: raw NMEA
  (`!AIVDM`) over TCP/UDP from a local receiver (e.g. RTL-SDR running
  AIS-catcher), AIS-catcher-style JSON, and a global internet feed
  (AISStream.io WebSocket).
* **Sonar / radar / ARPA target trackers** — NMEA 0183 `TLL` (absolute target
  position) and `TTM` (range/bearing from own ship, resolved against own-ship
  GPS and heading).
* **LiDAR (and any custom contact source)** — newline-delimited JSON contacts
  over UDP, optionally pinned to an AIS identity by MMSI.

The fusion engine maintains live vessel **tracks**, detects **dark contacts**
(vessels physically present but silent on AIS), raises structured **alerts**
(dark contact, AIS lost, watchlist hits, collision risk from CPA/TCPA,
geofence entries, feed silence), persists state across restarts, enriches
vessel identities through the **Global Fishing Watch** API, and streams the
whole picture to an **OSIRIS-style live dark map** over WebSocket.

The entire system — HTTP/WebSocket server, embedded map UI, coastline data and
MapLibre — compiles into one ~10 MB Rust binary. No runtime dependencies.

---

## 2. System architecture

```
        AIS radio                sonar / radar / ARPA            marine LiDAR
  (RTL-SDR + AIS-catcher)      (NMEA 0183 TLL / TTM)         (contact JSON, UDP)
  + AISStream.io (global WS)          │                             │
            │                         │                             │
            ▼                         ▼                             ▼
     ┌─────────────────────────────────────────────────────────────────────┐
     │  ingest (src/sources)                                               │
     │  Router per feed: NMEA framing + checksum, AIS 6-bit decode with    │
     │  multi-fragment assembly, own-ship navigation (GGA/GLL/RMC/HDT/VTG),│
     │  LiDAR JSON, AISStream envelope mapping                             │
     └──────────────────────────────┬──────────────────────────────────────┘
                                    ▼  Event (mpsc)
     ┌─────────────────────────────────────────────────────────────────────┐
     │  fusion engine (src/fusion.rs)                                      │
     │  gated track association · AIS ↔ sensor corroboration · dark-contact│
     │  detection · AIS-lost (blind / still-visible) · watchlist matching  │
     │  CPA/TCPA collision risk · geofence transitions · feed-silence      │
     │  detection · track-cap eviction · position recording                │
     └───────────────┬──────────────────────────────┬──────────────────────┘
                     ▼                              ▼
     ┌──────────────────────────┐    ┌──────────────────────────────────────┐
     │  store (src/store.rs)    │    │  notify (src/notify.rs)              │
     │  tracks, MMSI index,     │    │  alert log (JSONL) + out-of-band     │
     │  alerts, zones, watchlist│    │  delivery: webhooks + Telegram       │
     └───────┬──────────────────┘    └──────────────────────────────────────┘
             ▼                                      ▲
     ┌──────────────────────────────────────────────┴──────────────────────┐
     │  server (src/server.rs) — axum                                      │
     │  REST API · WebSocket (per-connection viewport filtering) · token   │
     │  auth · embedded MapLibre UI (rust-embed) · replay API              │
     └──────────────────────────────┬──────────────────────────────────────┘
                                    ▼
                       browser UI (ui/) — live dark map
                       7 basemap styles · clusters · class icons
                       watchlist · replay scrubber · GFW panel

   persist (src/persist.rs) — atomic JSON state, JSONL logs, daily history,
   retention pruning. Global Fishing Watch client (src/gfw.rs) with TTL cache.
```

### Module map

| Module | Responsibility |
|---|---|
| `src/main.rs` | CLI, wiring, `.env` discovery, logging setup, browser launch |
| `src/config.rs` | `config.toml` + environment overrides (precedence: config < env < CLI) |
| `src/model.rs` | Core types: Contact, Track, Alert, Zone, WatchEntry, HistoryPoint |
| `src/geo.rs` | Haversine, destination point, bearing, point-in-polygon |
| `src/nmea.rs` | NMEA 0183 framing/checksum, own-ship nav, TLL/TTM target parsers |
| `src/ais.rs` | AIS 6-bit codec (types 1/2/3/4/5/11/18/19/24), assembler, encoder |
| `src/land.rs` | Natural Earth 50m land mask for the simulator |
| `src/fusion.rs` | Track association, corroboration, all alert logic, recording |
| `src/store.rs` | Live picture state + snapshot serialisation + persistence hooks |
| `src/gfw.rs` | GFW v3 client: identity search, detail, insights, multi-dataset events |
| `src/persist.rs` | Atomic JSON writes, JSONL append, history + retention |
| `src/notify.rs` | Alert delivery: webhooks, Telegram, severity floor, rate limit |
| `src/server.rs` | REST + WebSocket + auth + replay + embedded UI |
| `src/sources/*` | Feed transports (TCP/UDP), per-feed Router, AISStream client, simulator |
| `ui/` | MapLibre dark map UI, embedded into the binary |

---

## 3. Data sources and ingestion

### 3.1 AIS

| Path | Transport | Enable via |
|---|---|---|
| Local receiver (RTL-SDR + [AIS-catcher](https://github.com/jvde-github/AIS-catcher)) | NMEA over TCP `:10110` or UDP `:10111`; JSON also accepted | `[sources.ais]` |
| Global internet feed | [AISStream.io](https://aisstream.io) WebSocket (free key) | `[sources.aisstream]` + `AISSTREAM_API_KEY` in `.env` |

The decoder implements ITU-R M.1371 message types 1/2/3 (class A position),
4/11 (base stations), 5 (static voyage data, multi-fragment), 18/19 (class B)
and 24 (class B static). The encoder side powers the simulator and the test
suite; encoder output was cross-verified field-for-field against the
independent third-party decoder **pyais**.

**Production behaviour:** the built-in simulator stands down automatically the
moment a real AIS source is configured (force with `--sim`).

### 3.2 Sonar / radar / ARPA

Marine trackers export targets as NMEA 0183:

| Sentence | Meaning | Notes |
|---|---|---|
| `$--TLL` | Target latitude/longitude | status `L` (lost) is honoured and not tracked |
| `$--TTM` | Tracked target: range, bearing, speed, course, CPA, TCPA | distance units N (default) / K / S; resolved via own-ship GGA/RMC + HDT |

Talker IDs select the sensor: `SD`/`SN` sonar, `RA` radar, `LI`/`LD` LiDAR.
TTM provides target speed/course and closest-point-of-approach data, which
flows onto the track and into collision-risk alerting.

### 3.3 LiDAR / custom contacts

Newline-delimited JSON over UDP (default `:10113`):

```json
{"sensor":"lidar","id":"CONTACT-7","lat":36.0512,"lon":-5.3441,
 "range_m":1830.0,"bearing_deg":271.4,"speed":8.2,"confidence":0.93,"mmsi":235009123}
```

All fields except `lat`/`lon` are optional. If `mmsi` is present the contact is
fused directly onto that AIS track (sensor corroboration).

### 3.4 Own ship

`GGA`/`GLL`/`RMC` (position, speed, course) and `HDT` (heading) sentences on
any feed establish own ship, drawn on the map and required for TTM
range-bearing resolution.

---

## 4. Fusion engine

* **Association.** Sensor contacts attach to the nearest non-AIS track within
  `fusion.gate_m` (default 900 m), then any track within 400 m, else become a
  new sensor-only track. AIS positions are authoritative: sensor hits near an
  AIS track mark it **corroborated** but never move it.
* **Dark contacts.** A sensor-only track older than `dark_alert_after_s`
  raises a high-severity alert — something is on the water that is not
  transmitting AIS. Requires a local physical sensor; an internet AIS feed
  alone cannot produce dark detections.
* **AIS lost.** Measured from the last AIS-specific timestamp (`last_ais`), so
  sensor contacts cannot mask a silent transponder. Two variants: *blind*
  (medium) and *still visible on sensor* (high — possible AIS-off).
* **Watchlist.** Entries match live tracks by MMSI, IMO or exact name; the
  first sighting raises a high-severity alert per (entry, track) pair.
* **Collision risk.** When a tracker reports CPA ≤ `collision_cpa_m` (500 m)
  within TCPA ≤ `collision_tcpa_min` (10 min), a high-severity alert fires.
* **Geofences.** Polygon zones raise entry alerts with per-track cooldown.
  Zones are created by drawing on the map (double-click to close) or the API.
* **Feed silence.** A connected feed sending nothing for `feed_stall_after_s`
  (90 s) raises FEED SILENT — a stale picture otherwise looks like a quiet
  ocean.
* **Eviction.** `fusion.max_tracks` (2,500) caps the picture; the
  least-recently-seen tracks are dropped.

All alerts carry kind, severity, message, position, track reference and UTC
timestamp, and are subject to a per-key cooldown (`alert_cooldown_s`).

---

## 5. Alert delivery

| Destination | Configuration | Notes |
|---|---|---|
| Map UI | always | ALERTS tab + banner |
| Alert log | always | `data/alerts.jsonl`, last ~120 reloaded on restart |
| Webhooks | `[alerts] webhooks` or `OS_ALERT_WEBHOOK` | JSON POST `{text, content, alert}` — Slack, Discord, Teams, Zapier, n8n compatible |
| Telegram | `OS_TELEGRAM_BOT_TOKEN` + `OS_TELEGRAM_CHAT_ID` | high-severity alerts ring |

Delivery is bounded by `min_severity` (default medium) and `max_per_minute`
(30) to survive global-feed churn. Delivery failures never affect tracking.

---

## 6. Global Fishing Watch integration

Requires a free non-commercial token (`GFW_API_TOKEN` in `.env`). Used
endpoints (v3, `gateway.api.globalfishingwatch.org`):

| Endpoint | Use |
|---|---|
| `GET /v3/vessels/search` | identity match by MMSI/IMO/name (registry, flag, gear, owner) |
| `GET /v3/vessels/{id}` | full registry record incl. ownership and authorisations |
| `POST /v3/insights/vessels` | apparent fishing (incl. no-take MPAs / no RFMO authorisation), AIS coverage, AIS gaps, IUU list, flag changes |
| `POST /v3/events` | activity events for the map: FISHING, ENCOUNTER (transshipment), LOITERING, GAP, PORT_VISIT |

Integration verified live: identity lookups (e.g. IMO 7831410 → CLAUDINA, flag
ARG, owner DESEADO PESQUERA), insights with `datasetId`, regional queries, and
a global query returning 500 of 167,812 fishing events worldwide (7 days).
The IUU field is an object with counters; listing is only reported on a
positive count (a false IUU badge would be a damaging accusation).

---

## 7. Map and user interface

* **Basemap styles** (LAYERS tab, persisted per browser): Satellite (default,
  Esri), Ocean (Esri bathymetry — coastal coverage), Dark (Esri dark gray),
  Topo, Streets (OSM), Nat Geo, Relief. All keyless.
* **Performance engineering** for worldwide feeds:
  * server snapshots are shared `Arc` objects; each WebSocket connection
    receives only tracks inside its viewport (`hidden_tracks` reported in the
    status bar);
  * markers are clustered below zoom 8 (click a bubble to expand);
  * trails and labels thin adaptively with track density; sidebar caps at 400
    rows with class / sort / in-view filters and throttled re-renders;
  * update cadence stretches 1 s → 3 s as the picture grows.
* **Vessel motion** is dead-reckoned between AIS reports along course/speed,
  capped at 45 s so stale tracks stop rather than fly.
* **Icons** are class-shaped silhouettes (fishing / cargo / passenger / other)
  tinted by source: cyan AIS, orange sonar/radar, green LiDAR, white
  corroborated, pulsing red dark contact.
* **Detail drawer**: full live state + GFW identity, fishing effort, AIS
  coverage/gaps, IUU status, WATCH button, GeoJSON copy, fly-to.
* **Replay**: with `[storage] record_tracks = true`, positions are written to
  `data/history/<day>.jsonl` and the GFW tab's scrubber reconstructs the
  picture nearest any recorded timestamp.

---

## 8. Persistence and data model

| File | Content |
|---|---|
| `data/zones.json` | geofence zones (atomic writes) |
| `data/watchlist.json` | watchlist entries |
| `data/alerts.jsonl` | every alert ever raised (append-only audit log) |
| `data/history/<yyyy-mm-dd>.jsonl` | position batches, one per `record_interval_s` |
| `data/oceansentinel.log.<date>` | daily rotating application log |

All runtime state lives under `[storage] dir` (default `data/`), gitignored.
History is pruned to `retention_days`. Writes are atomic (temp file + rename)
or append-only; persistence failures degrade to in-memory operation, never to
a crash.

---

## 9. Security model

* Default bind `127.0.0.1:8787`, no authentication — local use only.
* For LAN/internet exposure, set `[server] api_token` (or `OS_API_TOKEN`).
  Every REST route and the WebSocket then require
  `Authorization: Bearer <token>` or `?token=<token>`; comparison is
  constant-time. The web UI prompts once and remembers (localStorage).
* Startup warns loudly if a non-loopback bind has no token.
* Secrets (API tokens) live only in `.env`, which is gitignored; `.env.example`
  ships without values and the repository is swept for secrets before every
  push.

---

## 10. Performance profile

Measured with a worldwide AISStream subscription (~2,500 concurrent tracks):

* Server: decode + fuse costs are microseconds per message (release build:
  LTO, single codegen unit); snapshot broadcast strides 1/2/3 s by track count.
* WebSocket payload per client scales with the viewport, not the planet
  (e.g. status bar `TRACKS 2500 (+1836 out of view)`).
* Browser: clusters below zoom 8, trail/label budgets, 400-row sidebar cap and
  throttled list rebuilds keep interaction smooth at full capacity.

---

## 11. Installation and operation

```bat
winget install Rustlang.Rustup BrechtSanders.WinLibs.POSIX.UCRT
rustup default stable-x86_64-pc-windows-gnu
build.bat          :: cargo build --release
run.bat            :: starts the app and opens the map
```

| Flag | Effect |
|---|---|
| `--port`, `--host` | override bind address (env `OS_PORT` / `OS_HOST` sit between config and flags) |
| `--aoi LAT,LON` | opening view |
| `--sim` / `--no-sim` | force the demo simulator |
| `--no-open` | do not open a browser window |
| `-v` | debug logging |

`config.toml` is generated from `config.example.toml` on first run; every
section is optional. Environment variables: `GFW_API_TOKEN`,
`AISSTREAM_API_KEY`, `OS_API_TOKEN`, `OS_ALERT_WEBHOOK`,
`OS_TELEGRAM_BOT_TOKEN`, `OS_TELEGRAM_CHAT_ID`, `OS_PORT`, `OS_HOST`, `OS_SIM`.

### HTTP API reference

| Route | Description |
|---|---|
| `GET /api/health` | liveness + auth state |
| `GET /api/state` | full snapshot (tracks, alerts, zones, watchlist, feeds, stats) |
| `GET /ws` | live stream (`state` snapshots, `alert` events); accepts `{"type":"viewport",...}` from the client |
| `GET /api/gfw/vessel?mmsi=\|imo=\|name=\|id=` | identity + insights |
| `GET /api/gfw/events?bbox=w,s,e,n&days=&types=` | regional activity events (comma-separated types) |
| `GET /api/history?ts=` | recorded positions nearest a timestamp |
| `GET/POST/DELETE /api/watchlist` | watchlist management |
| `POST/DELETE /api/zones` | geofence management |

---

## 12. Testing and verification

`cargo test` runs 38 unit tests covering: AIS encode/decode round-trips
(class A/B positions, static data, multi-fragment assembly), NMEA parsing
(GGA/RMC/TLL/TTM incl. CPA/TCPA), JSON ingest routing (incl. the
LiDAR-with-MMSI regression), watchlist matching, label classification,
severity ordering, persistence (atomic writes, retention pruning), webhook
payload shape, GFW parsing (identity, insights, multi-dataset selection),
geodesy and point-in-polygon.

Beyond unit tests, the following was verified against live systems during
development:

* AIS encoder conformance against the independent `pyais` decoder.
* All ingest paths exercised with real datagrams (TLL from two talkers, TTM
  resolved by own-ship geometry, LiDAR JSON, global WebSocket feed).
* Watchlist hit raised within 3 s of a watched MMSI appearing; persisted
  across restart (zone + watchlist restore verified).
* Auth matrix (no token 401 / query 200 / bearer 200 / wrong 401).
* Global GFW query (500 of 167,812 events worldwide); regional queries for
  India (10,853 events) and South Africa/Madagascar (6,127).
* Land-avoidance simulator: 1,050 recorded positions across 20 tracks, all on
  water, against the Natural Earth 50m mask.
* UI verified in a real browser: clustering, class icons, labels, trails,
  viewport filtering, drawer enrichment, replay, 7 basemap styles.

CI (GitHub Actions) runs `cargo fmt --check`, `clippy -D warnings` and the
test suite on every push; a tag triggers a release build that attaches the
Windows executable.

---

## 13. Known limitations and roadmap

* **Live coverage follows community receivers.** AISStream is dense in Europe
  and North America and sparse elsewhere (verified: 1,845 tracks in
  Europe/Med vs 0 in Indian coastal waters at time of measurement). Denser
  regional coverage requires a local RTL-SDR receiver or a commercial feed.
* **Dark-contact detection requires local hardware.** Internet AIS alone can
  never reveal non-transmitting vessels.
* No serial (COM port) reader yet — hardware connects via TCP/UDP bridges.
* AIS message types 9/21/27 are parsed as "other" (no SAR aircraft, ATON).
* File-based persistence is right-sized for zones/watchlists/alerts; heavy
  history analytics would warrant SQLite.
* Suggested next steps: replay scrubber polish (per-vessel trails over time),
  paid-feed adapters, view presets.

---

## 14. Legal and ethical use

* AIS and marine VHF are public radio, but recording/use obligations vary by
  jurisdiction — follow local law.
* Active sonar/LiDAR must comply with local regulations and must never
  interfere with other vessels.
* Global Fishing Watch data is licensed for **non-commercial** use; respect
  their terms and attribution requirements.
* Esri basemaps and AISStream are free but subject to their respective terms.
* This is a situational-awareness and research instrument; the operator is
  responsible for how it is deployed.

---

## 15. Glossary

| Term | Meaning |
|---|---|
| **AIS** | Automatic Identification System — vessels' mandated VHF transponder |
| **Dark contact** | A vessel detected by physical sensors with no matching AIS transmission |
| **Corroborated** | An AIS track independently confirmed by a physical sensor |
| **CPA / TCPA** | Closest Point of Approach / Time to CPA |
| **MMSI / IMO** | Maritime radio identity / permanent ship registration number |
| **IUU** | Illegal, Unreported and Unregulated fishing |
| **AOI** | Area of Interest — the map's opening view |
| **Transshipment** | Cargo/crew transfer between vessels at sea (GFW ENCOUNTER events) |
