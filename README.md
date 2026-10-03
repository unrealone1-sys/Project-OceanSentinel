# OceanSentinel

**Live maritime domain awareness for Windows.** OceanSentinel fuses AIS radio,
sonar/ARPA target trackers and marine LiDAR into one live vessel picture, flags
vessels that are physically present but not transmitting AIS ("dark contacts"),
enriches identities through the **Global Fishing Watch** API, and renders it all
on an OSIRIS-style live dark map.

Written in Rust (tokio + axum + MapLibre). Single self-contained `.exe`: the map
UI, WebSocket feed and API are embedded in the binary.

```
        AIS radio                 sonar / radar / ARPA            marine LiDAR
  (RTL-SDR + AIS-catcher)        (NMEA 0183 TLL / TTM)         (contact JSON over UDP)
            │                            │                            │
            ▼                            ▼                            ▼
     ┌─────────────────────────────────────────────────────────────────────┐
     │  ingest layer   !AIVDM decode · NMEA router · own-ship nav          │
     │                 (GGA/RMC/HDT for TTM range-bearing resolution)      │
     └──────────────────────────────┬──────────────────────────────────────┘
                                    ▼
     ┌─────────────────────────────────────────────────────────────────────┐
     │  fusion engine  track association (±gate) · AIS↔sensor corroboration│
     │                 dark-contact alerts · AIS-lost alerts · geofences   │
     └──────────────────────────────┬──────────────────────────────────────┘
                                    ▼
     ┌─────────────────────────────────────────────────────────────────────┐
     │  track store    live picture + trails + alerts (in memory)          │
     └───────┬──────────────────────────────────────┬──────────────────────┘
             ▼                                      ▼
   Global Fishing Watch enrichment          axum server: REST + WebSocket
   (identity, fishing effort, AIS gaps,     → embedded MapLibre dark map UI
    IUU list, regional fishing events)      → /api/state · /api/gfw/* · /ws
```

---

## Quick start (Windows)

Requirements: Windows 10/11, ~500 MB disk. Install the Rust toolchain once:

```bat
winget install Rustlang.Rustup BrechtSanders.WinLibs.POSIX.UCRT
rustup default stable-x86_64-pc-windows-gnu
```

Build and run:

```bat
build.bat            :: cargo build --release
run.bat              :: starts the app and opens the map window
```

**Production mode (real vessels, no demo traffic):** create `.env` next to the
app with your keys — `AISSTREAM_API_KEY=…` (free at aisstream.io) and
`GFW_API_TOKEN=…` (free, non-commercial) — then restart. Real AIS takes over
automatically: the built-in simulator stands down the moment a real feed is
configured (force it back with `--sim`). To share the map on your LAN, run
`oceansentinel.exe --host 0.0.0.0`.

Performance with worldwide feeds is managed automatically: a hard track cap
(`fusion.max_tracks`, least-recently-seen evicted), thinner trails and a slower
update cadence as the picture grows.

Or manually:

```bat
cargo build --release
target\release\oceansentinel.exe
```

The app starts on <http://127.0.0.1:8787> and opens a chromeless Edge/Chrome
app window. With no hardware attached it runs a **built-in simulator** (14 AIS
vessels + 5 dark contacts around the configured area of interest) so every
feature is demonstrable immediately. Use `--no-open` to skip the browser.

```
Usage: oceansentinel.exe [OPTIONS]
  -c, --config <FILE>   config file (default config.toml, written on first run)
      --port <PORT>     override HTTP port
      --host <HOST>     bind address (0.0.0.0 to share on the LAN)
      --aoi <LAT,LON>   area of interest center, e.g. --aoi 36.02,-5.36
      --sim / --no-sim  force the simulator on/off
      --no-open         do not open a browser window
  -v, --verbose         debug logging
```

---

## Connecting real sensors

### 1. AIS (the primary "radio sonar")

Any receiver that emits NMEA `!AIVDM` lines works. The cheapest path is an
RTL-SDR dongle plus the open-source
[AIS-catcher](https://github.com/jvde-github/AIS-catcher):

```bat
:: raw NMEA over TCP (OceanSentinel default: 127.0.0.1:10110)
AIS-catcher -d 0 -o 5 127.0.0.1 10110

:: raw NMEA over UDP (127.0.0.1:10111)
AIS-catcher -d 0 -o 3 127.0.0.1 10111
```

AIS-catcher's JSON output (`-o 6`) is also accepted on the same ports.
Then in `config.toml`:

```toml
[sources.ais]
enabled = true
tcp = ["127.0.0.1:10110"]
udp = []
```

Online AIS aggregators that offer NMEA-over-TCP streams work the same way.

### 2. Sonar / radar / ARPA target trackers

Marine sonar and radar sets export tracked targets as NMEA 0183 sentences.
OceanSentinel ingests the two universal target sentences:

| Sentence | Meaning | Used for |
|---|---|---|
| `$--TLL` | Target latitude/longitude | absolute target position |
| `$--TTM` | Tracked target: range + bearing from own ship | resolved to lat/lon using own ship's GGA/RMC + HDT heading |

The talker id selects the sensor: `SD`/`SN` = sonar, `RA` = radar/ARPA,
`LI` = LiDAR tracker. Point the feed (serial-to-UDP bridge, chartplotter
multiplexer, vendor tracker) at the configured UDP port:

```toml
[sources.sonar]
enabled = true
udp = ["0.0.0.0:10112"]
talkers = ["SD", "SN", "RA", "LI"]
```

Notes:
* `TTM` distances are treated as nautical miles unless the sentence declares
  `K` (km) or `S` (statute miles).
* Own ship must be providing `GGA`/`RMC` (+ `HDT` for relative bearings) on the
  same feed for `TTM` to resolve; `TLL` needs nothing.
* Commercial fishing sonar (Simrad, Furuno) needs its vendor tracker output
  enabled; if your unit emits a proprietary Ethernet protocol instead, bridge
  it to `TLL`/`TTM` NMEA or to the LiDAR JSON schema below.

### 3. Marine LiDAR / any other contact source

Newline-delimited JSON over UDP (`config.toml` → `[sources.lidar]`), one
contact per line:

```json
{"sensor":"lidar","id":"CONTACT-7","lat":36.0512,"lon":-5.3441,"range_m":1830.0,"bearing_deg":271.4,"confidence":0.93}
```

| field | required | notes |
|---|---|---|
| `lat`, `lon` | yes | WGS-84 decimal degrees |
| `id` | no | label used until an AIS identity is fused |
| `mmsi` | no | include it and the contact is fused straight onto that AIS track |
| `range_m`, `bearing_deg` | no | kept as provenance, shown in the UI |
| `confidence` | no | 0–1, defaults to 0.9 |

Quick smoke test from PowerShell:

```powershell
$udp = New-Object System.Net.Sockets.UdpClient
$bytes = [Text.Encoding]::ASCII.GetBytes('{"sensor":"lidar","id":"TEST-1","lat":36.04,"lon":-5.36,"confidence":0.95}')
$udp.Send($bytes, $bytes.Length, "127.0.0.1", 10113)
```

Enable with `[sources.lidar] enabled = true`.

### 4. Global live AIS over the internet (no hardware required)

For worldwide live coverage without a radio, use [AISStream.io](https://aisstream.io)
(free API key, community AIS network over WebSocket):

1. Get a free key at <https://aisstream.io>.
2. Put `AISSTREAM_API_KEY=…` in `.env` (same folder as the app).
3. Enable the feed:

```toml
[sources.aisstream]
enabled = true
url = "wss://stream.aisstream.io/v0/stream"
bounding_boxes = [[[-90.0, -180.0], [90.0, 180.0]]]   # whole globe
```

Messages are mapped onto the same internal model as radio AIS, so global
vessels behave exactly like locally received ones (tracks, trails, GFW
enrichment on click). A worldwide subscription is a firehose, so OceanSentinel
protects itself:

* `fusion.max_tracks` (default 2500) caps the live picture — the least recently
  seen tracks are evicted;
* trails shrink to 0–20 points and the update cadence stretches to 1–3 s as the
  track count grows.

For detailed work, narrow the box — for example
`bounding_boxes = [[[35.0, -7.0], [37.0, -4.0]]]` for the Strait of Gibraltar.

---

## Global coverage

The map is not confined to the configured area of interest; `[aoi]` only sets
the opening view and where the demo traffic is generated. You can pan and zoom
anywhere on earth, look up any vessel by MMSI/IMO/name in the GFW tab, and pull
apparent fishing activity for whatever is on screen — or for the whole planet
with **GLOBAL FISHING EVENTS (7d)**. Clicking any fishing-event dot on the map
opens that vessel's Global Fishing Watch record.

---

## Global Fishing Watch enrichment

1. Register free at <https://globalfishingwatch.org/our-apis/tokens> and copy
   your API token (non-commercial use only, per their terms).
2. Put it next to the exe in `.env`:

```
GFW_API_TOKEN=your_token_here
```

3. Restart. The topbar chip shows `GFW LIVE`.

What OceanSentinel uses:

| Endpoint | Purpose in the app |
|---|---|
| `GET /v3/vessels/search` | identity match by MMSI/IMO/name (registry name, flag, gear type, owner) |
| `GET /v3/vessels/{id}` | full registry record incl. ownership and authorizations |
| `POST /v3/insights/vessels` | apparent fishing events (incl. in no-take MPAs and without known RFMO authorization), AIS coverage, AIS-gap events, IUU vessel list, flag changes |
| `POST /v3/events` | apparent fishing events inside the current map view (`public-global-fishing-events:latest`) |

Selecting any track with an MMSI auto-fetches its GFW record into the detail
drawer; the **GFW** tab queries regional fishing activity for the current map
view. All responses are cached (default 6 h) and raw JSON is available in the
drawer. Without a token everything else still works.

---

## The map

* **Basemap switcher** (LAYERS tab): **Satellite** (Esri World Imagery, the
  default — coastlines and shoals are always visible), **Dark** (CARTO dark,
  minimal and very quiet over open ocean) and **Streets** (OpenStreetMap). The
  choice is remembered between sessions. A local lat/lon graticule is always
  drawn, so the map keeps its bearings even with no tiles at all.
* **Live tracks** — triangle icons rotate with heading/course, coloured by
  source: cyan = AIS, orange = sonar/radar, green = LiDAR, white = AIS
  confirmed by a physical sensor, pulsing red = dark contact (no AIS).
* **Trails** — per-track history (kept in memory, exportable).
* **Alerts tab** — dark contact / AIS lost / sensor corroboration / geofence
  entry, click to fly to it.
* **Layers tab** — toggle sources and layers, **DRAW ZONE** to arm a geofence
  (click vertices, double-click to finish) and get entry alerts.
* **GFW tab** — fishing-events-in-view query, or lookup any MMSI/IMO/name.
* **Detail drawer** — full live state plus GFW identity, fishing effort, AIS
  coverage, gap events and IUU status.
* **EXPORT** — current picture as GeoJSON (drop into QGIS/kepler.gl).

## Detection logic

* Sensor contacts associate to the nearest track within the gate
  (`fusion.gate_m`, default 900 m); unmatched contacts become new sensor-only
  tracks.
* A sensor-only track older than `dark_alert_after_s` raises a **DARK CONTACT**
  alert: something is on the water that is not transmitting AIS.
* When a sensor contact lands within 1.5 km of an AIS track and the identity
  then arrives, the AIS track is marked **sensor-corroborated** (a real
  "AIS matches reality" check; spoofed AIS rarely survives an independent
  physical sensor).
* AIS tracks that fall silent for `ais_lost_after_s` raise **AIS LOST**.
* Geofence zones raise entry alerts with a per-track cooldown.

## HTTP API

| Route | Description |
|---|---|
| `GET /api/health` | liveness |
| `GET /api/state` | full snapshot (tracks, alerts, zones, feeds, stats) |
| `GET /ws` | live WebSocket stream (`{"type":"state"}` every tick, `{"type":"alert"}`) |
| `GET /api/gfw/vessel?mmsi=` (or `imo=`, `name=`, `id=`) | GFW identity + insights |
| `GET /api/gfw/events?bbox=w,s,e,n&days=14` | apparent fishing events in a region |
| `POST /api/zones` `{name, polygon:[[lon,lat],…]}` | create a geofence |
| `DELETE /api/zones?id=` | remove a geofence |

## Configuration

`config.toml` is created from `config.example.toml` on first run; every section
is optional. Key knobs: `[aoi]` (where the map opens), `[fusion]` (association
gate, alert timings, trail length), `[sources.*]`, `[gfw]`, `[server]`.
Environment overrides: `OS_PORT`, `OS_HOST`, `OS_SIM`, `GFW_API_TOKEN`.

## Tests

```bat
cargo test
```

Covers AIS encode/decode round-trips (class A positions, static data, class B,
multi-fragment assembly), NMEA parsing (GGA/RMC/TLL/TTM), great-circle maths,
point-in-polygon, LiDAR JSON ingest and GFW response parsing.

## Legal and ethical use

* AIS reception and marine VHF monitoring are public radio; how you may record
  and use it still depends on your jurisdiction. Follow local law.
* Active sonar/LiDAR must comply with local regulations and never be aimed to
  interfere with other vessels' equipment or navigation.
* Global Fishing Watch data is licensed for **non-commercial** use; respect
  their terms and attribution requirements.
* Do not use this to harass, intercept or target people or vessels. It is a
  situational-awareness and research tool; you are responsible for how you
  deploy it.

## Project layout

```
src/
  main.rs         CLI, wiring, browser launch
  config.rs       config.toml + env
  model.rs        Contact / Track / Alert / Zone / Feed types
  geo.rs          haversine, destination point, point-in-polygon
  nmea.rs         NMEA 0183 framing, own-ship nav, TLL/TTM targets
  ais.rs          !AIVDM 6-bit decode (types 1/2/3/4/5/11/18/19/24) + encoder
  store.rs        track store and JSON snapshot
  fusion.rs       association, corroboration, dark/AIS-lost/zone alerts
  gfw.rs          Global Fishing Watch client with cache
  server.rs       axum REST + WebSocket + embedded UI
  sources/        ingest router, TCP/UDP feeds, traffic simulator
ui/               MapLibre dark map UI (embedded into the exe)
```
