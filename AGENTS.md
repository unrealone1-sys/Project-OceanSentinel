# AGENTS.md — OceanSentinel

Agent-facing context for this project. Human documentation lives in `README.md`;
this file is the working knowledge a new session needs to be productive fast.

## What this is

A Windows desktop application (single Rust binary + embedded web UI) for live
maritime domain awareness. It ingests AIS radio (NMEA `!AIVDM`), sonar/radar/ARPA
target sentences (`TLL`, `TTM`) and LiDAR contact JSON, fuses them into live
vessel tracks, raises alerts for vessels that are physically present but not
transmitting AIS ("dark contacts"), enriches identities via the Global Fishing
Watch API, and renders an OSIRIS-style dark live map.

Zero-hardware demo is built in and on by default (traffic simulator around the
configured AOI). Real sensors are documented in `README.md`.

## Build / run / test

The toolchain is `stable-x86_64-pc-windows-gnu` (rustc 1.99). MSVC is **not**
installed; linking goes through MinGW-w64 gcc, so **gcc must be on PATH or every
build fails**. Always build like this:

```bash
export PATH="$HOME/.cargo/bin:/c/Users/ignun/AppData/Local/Microsoft/WinGet/Packages/BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe/mingw64/bin:$PATH"
cargo build   --manifest-path 'C:/Users/ignun/.zcode/workspace/default/oceansentinel/Cargo.toml'
cargo test    --manifest-path '.../Cargo.toml'      # 38 tests, all passing
cargo fmt --all -- --check                                  # CI gate
cargo clippy --all-targets -- -D warnings                   # CI gate
cargo build --release --manifest-path '.../Cargo.toml'
```

**`cargo test` does not relink `target/debug/oceansentinel.exe`.** If you run the
exe by hand after only running tests, you are testing a stale binary — this has
already produced two false debugging conclusions. Always `cargo build` before
running a manual check, or chain them (`cargo build && ./target/debug/…`).

`build.bat` / `run.bat` in this directory do the same thing for humans.
Run with `--no-open` when testing so no browser window is spawned.
`target/release/oceansentinel.exe` is the deliverable (~7.5 MB, self-contained).

## Architecture map

| File | Responsibility |
|---|---|
| `src/main.rs` | CLI (clap), wiring, Edge/Chrome `--app` window launch |
| `src/config.rs` | `config.toml` (+ `OS_PORT`/`OS_HOST`/`OS_SIM`/`GFW_API_TOKEN` env overrides) |
| `src/model.rs` | `Contact`, `Track`, `Alert`, `Zone`, `FeedStatus`, `OwnShipFix`, ship-type classification |
| `src/geo.rs` | haversine, destination point, bearing, point-in-polygon |
| `src/nmea.rs` | NMEA 0183 framing + checksum, own-ship nav (GGA/GLL/RMC/HDT/VTG), `TLL`/`TTM` parsers |
| `src/ais.rs` | AIS 6-bit decode (types 1/2/3/4/5/11/18/19/24), multi-fragment assembler, encoder (sim + tests) |
| `src/fusion.rs` | Track association, AIS↔sensor corroboration, dark/AIS-lost/zone alerts, 1 Hz snapshot broadcast |
| `src/store.rs` | In-memory track store, MMSI index, alerts, zones, snapshot JSON |
| `src/gfw.rs` | Global Fishing Watch v3 client (search / detail / insights / multi-dataset events) with TTL cache |
| `src/persist.rs` | Atomic JSON state (zones/watchlist), JSONL alert log, history recording + retention |
| `src/notify.rs` | Out-of-band alert delivery: webhooks + Telegram, severity floor, rate limit |
| `src/server.rs` | axum REST + WebSocket + embedded UI (`rust-embed`) |
| `src/sources/ingest.rs` | Per-feed router: one `Router` per feed owns the AIS assembler + own-ship state |
| `src/sources/aisstream.rs` | Global live AIS over WebSocket (AISStream.io), mapped onto `AisBody` |
| `src/sources/nmea_tcp.rs`, `nmea_udp.rs` | Feed transports with reconnect/backoff and feed status |
| `src/sources/simulator.rs` | Built-in traffic generator (emits real NMEA/JSON through the real ingest path) |
| `ui/` | MapLibre dark map UI, vendored `ui/vendor/maplibre-gl.js`, embedded into the exe |

Data flow: feed → `Router` → `Event` (mpsc) → `Fusion` → `Store` → broadcast →
WebSocket/`/api/state` → UI. Never bypass `Router` when adding a feed: it holds
per-feed AIS fragment assembly and own-ship state.

## Ports

| Port | Use |
|---|---|
| 8787 | HTTP + WebSocket (change with `--port` / `OS_PORT`) |
| 10110 | AIS NMEA over TCP (AIS-catcher style) |
| 10111 | AIS NMEA/JSON over UDP |
| 10112 | sonar/radar `TLL`/`TTM` over UDP |
| 10113 | LiDAR contact JSON over UDP |

## Invariants and hard-won gotchas

* **AIS field order**: `type(6)` → `repeat(2)` → `mmsi(30)`. Omitting the 2-bit
  repeat indicator shifts every field and silently produces plausible-looking
  garbage (this bug happened; round-trip tests alone did not catch it — the
  independent `pyais` cross-check did).
* **AIS fill bits**: type 5 is 424 bits → 71 six-bit chars with 2 fill bits. The
  assembler must carry the *last* fragment's fill value into the decoder.
* **6-bit armoring**: `48..=87 → v-48`, `96..=119 → v-56`. Values 0..39 map to
  `0`–`W`, 40..63 to `` ` ``–`w`.
* **`TTM` distance** is nautical miles unless the sentence declares `K`/`S`.
  `TTM` needs own-ship position (and `HDT` heading for relative bearings); with
  no own ship the event is dropped by design.
* **`TLL` status `L`** means target lost → do not track it (handled in ingest).
* **Sensor contact association** (`fusion.rs`): nearest non-AIS track within
  `gate_m` (900 m) first, then any track within 400 m, else create a new
  sensor-only track. AIS position is authoritative: sensor hits near an AIS
  track only mark it corroborated, they never move it.
* **`rust-embed` behaviour differs by profile**: debug builds read `ui/` from
  disk (UI edits visible on browser reload); release builds embed at compile
  time, so **UI changes need a release rebuild** before shipping.
* **`config.toml` is generated** from `config.example.toml` on first run and is
  gitignored. Keep the example file authoritative for defaults. Runtime state
  (zones, watchlist, alert log, history, logs) lives under `data/` — also
  gitignored, deleted freely.
* **Precedence is config < environment < CLI** in `apply_overrides`. CLI flags
  used to lose to a stale `OS_PORT` in `.env`; don't reorder the blocks.
* **The broadcast channel carries `ServerMsg`** (`State(Arc<Value>)`,
  `Alert(Value)`, `Zones`), not `String`. Each WebSocket connection filters the
  snapshot to its viewport (`filter_snapshot`) — snapshots are shared Arcs, so
  per-connection trimming must clone the value, never mutate the shared one.
* **Alerts flow through the notify channel**: `Fusion::flush_alerts` sends every
  alert to `notify.rs`, which owns `data/alerts.jsonl` and out-of-band delivery.
  Never write the alert log from fusion directly (double writes).
* **Recording** (`[storage] record_tracks`) writes `HistoryPoint` batches (one
  per interval, `ts` shared) to `data/history/<day>.jsonl`; `/api/history`
  caches the parsed day and must invalidate on mtime. Pruning runs once per day
  key change.
* **Server auth**: `guard()` checks `Authorization: Bearer` or `?token=` against
  `[server] api_token` with a constant-time compare. The UI stores it in
  localStorage and appends it to every fetch + the WebSocket URL.
* **JSON ingest precedence**: an explicit `"sensor":"lidar"` marker is evaluated
  *before* the mmsi heuristic, because LiDAR contacts may carry an `mmsi` to pin
  them to an AIS track. Getting this order wrong silently turns sensor contacts
  into AIS position reports (the `lidar_json_with_mmsi_is_a_sensor_contact` test
  guards it).
* **AIS-lost detection uses `last_ais`**, not `last_seen` — sensor contacts must
  not mask a silent transponder. A silent AIS track that a sensor is currently
  holding raises a **high** alert ("STILL VISIBLE … possible AIS-off"); if no
  sensor is holding it, the alert is **medium** ("no current sensor contact").
  The two variants have separate cooldown keys so a track can escalate.
* **AISStream uses `native-tls`** (schannel), deliberately matching reqwest: the
  rustls defaults pull `ring`/`aws-lc-rs`, which need extra build tooling on the
  GNU toolchain. Keep TLS choices native-tls unless the toolchain changes.
* **Global feeds need the scaling guards** in `fusion.rs`: `max_tracks`
  eviction (default 2500), trail thinning (0–20 points past 400/1500 tracks) and
  broadcast stride 1/2/3 s. Without them a worldwide AIS subscription balloons
  memory and the WebSocket payload.
* **Land mask** (`src/land.rs` + `assets/land-50m.geojson`, Natural Earth 50m,
  public domain): the simulator spawns and steers on water only. Do not downgrade
  to the 110m dataset — it closes the Strait of Gibraltar (the default scenario
  area), which turns the whole demo into gridlock. The mask treats unknown/broken
  data as water so the sim never stalls.
* **Simulator stands down automatically** when a real AIS source is configured
  (AISStream key present, or `[sources.ais]` enabled) — unless `--sim` is forced.
  This is deliberate production behaviour; do not "fix" it away.
* **Basemap default is satellite** (Esri World Imagery): CARTO "dark" is a
  nearly featureless grey plate over open ocean, which users read as a broken
  map. Switcher + graticule live in `ui/app.js` (`BASEMAPS`, `graticuleFC`).
  Glyphs must come from `fonts.openmaptiles.org` AND symbol layers must set
  `'text-font': ['Open Sans Regular']` — the demotiles path 404s, and a wrong
  font stack silently renders zero labels (both bit us).
* **`iuuVesselList` is an object with counters**
  (`{"totalTimesListed": n, "valuesInThePeriod": […]}`), never infer an IUU
  listing from the field merely being present — that produced a false "IUU
  listed" badge on every vessel until `iuu_listed()` required a positive count.
* **Simulator intent**: dark vessels spawn inside own-ship sensor range on
  purpose (so dark-contact detection is demonstrable); AIS vessels scatter over
  the whole AOI. Vessel index 3 goes AIS-silent for 300 s after 5 minutes to
  exercise the AIS-lost alert (threshold 180 s).
* **Keep the simulation `Send`**: use `StdRng`, not `rand::thread_rng()`
  (ThreadRng is not `Send` and breaks `tokio::spawn`).
* **Secrets** go in `.env` (`GFW_API_TOKEN`), never in `config.toml` or the repo.

## Expected baseline (healthy run)

Within ~30 s of starting with defaults you should see roughly: 20 tracks
(15 AIS including own ship `R/V SENTINEL`, 5 dark sensor-only), 5 `dark_contact`
alerts, 2–3 `sensor_corroborated` (AIS tracks inside sonar/lidar range), feed
status `simulator running ~57/s`. `/api/state` `stats` is the quickest check.

GFW without a token must degrade cleanly: `/api/gfw/vessel` returns HTTP 400
with an actionable message, nothing else breaks.

## How to verify a change

* `cargo test` — AIS encode/decode round-trips, multi-fragment assembly, NMEA
  parsers, geodesy, point-in-polygon, LiDAR JSON ingest, GFW parsers.
* **AIS codec changes**: re-run the conformance check. Print samples with
  `cargo test print_samples -- --nocapture` and decode them with `pyais`
  (`pip install pyais`) — an external decoder must agree field-for-field. Do not
  trust round-trip tests alone for codec work.
* **Feed changes**: inject real datagrams. Use Python `socket.sendto`, never a
  PowerShell double-quoted string (`"$SDTLL,..."` expands `$SDTLL` as an empty
  variable and silently sends a mangled line — this cost an hour once).
  Verify TTM lands where own-ship geometry says it should.
* **UI changes**: with the debug build, reload `http://127.0.0.1:8787/` and read
  DOM facts (`#track-list .row` count, chip texts, drawer contents) rather than
  eyeballing pixels; then rebuild release.
* **API**: `curl /api/health`, `/api/state`, `POST /api/zones`,
  `/api/gfw/vessel` (error path without a token is itself worth checking).

## Where to extend

* New sensor transport → add a module under `src/sources/`, route every line
  through `Router::line`, send `Event::Feed` status every ~5 s.
* New alert type → construct it in `fusion.rs` (respect the per-key cooldown via
  `cooldown_ok`) and add a colour/severity case in `ui/app.js`.
* New map layer → add a GeoJSON source + layer in `ui/app.js`'s `map.on('load')`,
  a toggle in the LAYERS panel (`index.html`), and wire it in
  `applyLayerVisibility()`.
* New API route → `server.rs::router` (use query params, not path params).

## Known gaps / next steps

* No serial (COM port) reader yet — hardware goes through TCP/UDP bridges today.
* AIS message types 9/21/27 are ignored (`AisBody::Other`), no ATON/DSC support.
* No persistence: tracks/alerts/zones are in-memory only; nothing survives a restart.
* GFW insights request shape is best-effort against the v3 docs; it fails soft
  (returns null insights) rather than erroring the whole lookup.
* Native window is Edge/Chrome `--app` mode, not an embedded webview.
* No authentication on the HTTP API — bind to `127.0.0.1` unless the LAN is trusted.

## Legal / ethics

AIS and marine VHF are public radio but local recording law still applies;
sonar/LiDAR use must comply with local regulations and must never interfere with
other vessels; GFW data is licensed for **non-commercial** use. This is a
situational-awareness instrument, not a targeting tool. See `README.md`.
