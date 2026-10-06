# OceanSentinel — Roadmap

Future development plan. Nothing here is implemented yet; each item lists the
existing architecture hooks it would plug into, a design sketch, and the
honest constraints. Items are ordered by value-to-effort ratio.

| Phase | Theme | Status |
|---|---|---|
| 0 | Shipped (see [PROJECT_DOCUMENTATION.md](PROJECT_DOCUMENTATION.md)) | ✅ done |
| 1 | Aircraft data (ADS-B) | 📋 planned |
| 2 | Sensor-fusion cross-domain alerts | 📋 planned |
| 3 | Weather & notices overlay | 📋 planned |
| 4 | Local anomaly detection (ML) | 📋 planned |
| 5 | Ports, routes and ETA | 📋 planned |
| 6 | Multi-user operations | 📋 planned |
| 7 | Deployment & distribution | 📋 planned |

---

## Phase 1 — Aircraft data (ADS-B)

The aviation equivalent of AIS: aircraft constantly broadcast position on
1090 MHz (ADS-B), unencrypted, and the same multi-source pattern used for
ships applies almost one-to-one.

### 1.1 Data sources

| Source | Transport | Cost | Notes |
|---|---|---|---|
| **Local RTL-SDR + readsb/dump1090** | SBS-1 BaseStation CSV over TCP `:30003` (or Beast/RAW binary) | free | The mirror of AIS-catcher for radio; ~250–400 km range per antenna |
| **adsb.lol / airplanes.live / ADSB.fi** | Free HTTP/JSON APIs | free | Community networks; same coverage caveats as AISStream (Europe/US dense, other regions thin) |
| **OpenSky Network** | REST + live API | free tier (rate-limited) | Global research network; the best free option for regions with no local receiver — direct answer to the "no vessels near India" class of problem |
| **ADS-B Exchange** | REST API | free tier with feed-in requirement | Strong coverage incl. aircraft that block other trackers |
| **FlightAware / Flightradar24** | Commercial API | paid | Highest quality; last resort |

**Recommended order:** SBS-1 local ingest first (matches the existing
`nmea_tcp` pattern exactly), then an OpenSky poller, then adsb.lol.

### 1.2 Ingestion design

* New module `src/sources/adsb_sbs.rs`: SBS-1 lines (`MSG,3,1,1,4CA87C,...`)
  are comma-separated, line-oriented — they slot into the existing
  `Router::line` pattern alongside NMEA and JSON. TCP reconnect logic is
  already written (`nmea_tcp.rs`) and reusable as-is.
* New source type `adsb` in `[sources.adsb]` with `tcp`, `http_poll`, and an
  `ADSBCOLLECT_API_KEY`-style env var, mirroring `[sources.aisstream]`.
* New message type `Event::Aircraft(AircraftBody)` parallel to
  `Event::Ais(AisBody)` — the `Event` enum and `Router` were built for this.

### 1.3 Data model

```rust
pub struct AircraftBody {
    pub icao24: String,        // hex transponder address (the "MMSI" of the sky)
    pub callsign: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub alt_ft: Option<i32>,       // barometric or geometric
    pub vert_rate: Option<i32>,    // ft/min — climbing/descending matters
    pub speed_kt: Option<f32>,     // ground speed
    pub track_deg: Option<f32>,
    pub on_ground: bool,
    pub emergency: Option<String>, // squawk 7500/7600/7700
}
```

Decision to make early: **generalise `Track` into a `Platform` (domain:
sea/air)** vs a parallel `AircraftTrack`. Recommendation: a `domain` field on
`Track` with aircraft-specific fields optional — cheaper than duplicating the
association, alert, persistence and replay machinery, and cross-domain fusion
(Phase 2) needs them in one store anyway.

### 1.4 UI

* New aircraft icon shapes (jet/propeller/rotor silhouette) in the existing
  `shipIcon`/`SHAPES` system; altitude-based tint (low = amber, high = cyan).
* New sources filter chip `AIRCRAFT`, class filter option, and a sidebar
  domain toggle (SHIPS / AIRCRAFT / ALL).
* Altitude + vertical-rate column in the drawer; emergency squawks get their
  own alert banner.
* Performance work carries over unchanged: clustering, viewport filtering,
  dead-reckoning all operate on generic tracks already.

### 1.5 Effort and risks

* **Effort:** ~1 week of focused work for SBS-1 + OpenSky + UI (the fuselage
  of the system — association, alerting, replay — needs no changes).
* **Risks:** track counts double when a wide-area ADS-B feed is on
  (`max_tracks` may need a per-domain cap); aircraft move ~20× faster than
  ships, so dead-reckoning and trail budgets need per-domain tuning.

---

## Phase 2 — Cross-domain sensor fusion (the interesting one)

Once aircraft exist in the same store, novel detection falls out of pairing
domains:

* **Aircraft loitering over a dark contact** — an aircraft circling a position
  where a sonar/LiDAR-only track sits (surveillance, SAR, or smuggling
  indication). Requires only a "orbit detection" heuristic: low ground speed +
  high turn rate + small radius over N minutes.
* **Search-and-rescue pattern awareness** — expanding-square / parallel-track
  flight patterns detected near a distress or AIS-lost alert.
* **AIS-to-aircraft squawk correlation** — vessels and aircraft sharing a
  position over time (helicopter deck operations).
* New alert kinds: `aircraft_loiter`, `sar_pattern`, with the existing
  cooldown/delivery machinery.

**Prerequisite:** Phase 1. **Effort:** days, not weeks — it is alert logic on
data already in the store.

---

## Phase 3 — Weather and maritime notices overlay

* **Wind/currents** (NOAA GFS / Open-Meteo, both free): animated particle
  field or arrow grid layer; adds real value to collision and dark-vessel
  reasoning (drift).
* **NAVTEX / Notices to Mariners**: text feed parsed into geo-referenced
  warning markers.
* **Sea state** (wave height) from Open-Meteo marine API.
* Sources are poll-and-overlay — a new `Overlay` source type rendering a grid
  layer; no changes to tracking.
* **Effort:** ~1 week. **Risk:** low.

---

## Phase 4 — Local anomaly detection (ML-lite)

The recorded history (`data/history/`) is the training corpus:

* **Route anomaly scoring** — per-vessel historical routes vs current track
  (cheap: Haversine-to- historic-path); flag deviations.
* **Loitering detection** for ships (mirroring the aircraft heuristic).
* **AIS-spoof heuristics** — impossible speed between reports, position
  jumps against dead-reckoning residuals, MMSI/IMO inconsistency via GFW
  identity data (we already pull flag/gear/IMO).
* **Port-call prediction** once Phase 5 ports exist.
* Keep it explainable: every anomaly alert must show the evidence (expected
  vs observed). **Effort:** staged; the spoof heuristics alone are ~2 days.

---

## Phase 5 — Ports, routes and ETA

* **Port database** — UN/LOCODE (public domain) with coordinates; port-visit
  detection from track history (arrival within X km + speed < 1 kn for Y min).
* **ETA display** — great-circle or great-ellipse route from current position
  through the destination field (AIS voyage data already parsed) to the port.
* **Vessel pairwise encounter forecast** — extend CPA beyond TTM-reported
  values by projecting tracks forward (we already dead-reckon).
* **Effort:** ~1–2 weeks. Ports + visits give the biggest immediate payoff.

---

## Phase 6 — Multi-user operations

* **SQLite** replaces/augments the file store for zones, watchlists, users,
  and (optionally) track history — one `rusqlite` dependency, no server.
* **Roles** (viewer / operator / admin) behind the existing token auth.
* **Shared state**: watchlists and zones become per-user with a shared layer.
* **PWA/mobile**: the UI is already a web app; a responsive layout pass plus
  the replay scrubber on touch is most of the work.
* **Effort:** 2–3 weeks. Do after the data model is stable (Phase 1).

---

## Phase 7 — Deployment and distribution

* **Cross-platform builds** — the code is Windows-flavoured only in the
  browser-launch helper; everything else is portable. Linux build + systemd
  unit + Docker image for server use (headless, no UI embed needed).
* **GitHub Releases automation** already attaches Windows exe on tags
  (`.github/workflows/release.yml`); add Linux artifacts and a changelog.
* **Installer** (optional): single-file exe is already the distribution.

---

## Explicitly not planned

* Radar video integration beyond NMEA target sentences (proprietary vendor
  protocols, heavy DSP).
* Encrypted/military AIS (AIS military messages exist; reception is out of
  scope).
* Automated enforcement or reporting to authorities — OceanSentinel is an
  observation and alerting instrument; what an operator does with a dark
  contact is outside the software's remit.

---

## Sequencing rationale

Phase 1 is the gateway: it exercises every extension point the project will
ever need (new sensor domain, new event type, new icons, per-domain tuning)
with a data source that is free and well-documented. Phase 2 is the payoff
that no single-domain tool offers. Phases 3–5 each stand alone; 6 and 7 are
packaging. Nothing in this roadmap requires re-architecting — the
`Event`/`Router`/`Track`/alert pipeline was built to take new sources by
addition, not surgery.
