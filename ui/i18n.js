'use strict';

/* Interface languages for both maps (OceanSentinel and Project Icarus).
 *
 * English is the source language and the fallback: any key missing from a
 * language pack renders its English string rather than a blank or a key name,
 * so a partially translated language still produces a usable page.
 *
 * How to use it:
 *   - static markup carries data-i18n="key" (plus data-i18n-title and
 *     data-i18n-placeholder for attributes) and is re-rendered on every
 *     language change;
 *   - scripts call t('key', {n: 5}) for anything built at runtime.
 *
 * Adding a language is one file: ui/lang/<code>.js calling
 * OSi18n.add('<code>', {name, dir, strings}). Nothing else changes.
 *
 * Not translated on purpose: server-generated alert sentences and the long
 * explanatory hints. Those are content, not interface, and machine-translating
 * an operator alert is worse than leaving it in the language it was written in.
 * Alert *kinds* are localised where they are shown as labels. */

window.OSi18n = (function () {
  const STORE_KEY = 'os.lang';
  const packs = {};
  let current = 'en';

  const EN = {
    // ---- top bar and shared chrome
    'lang.label': 'Language',
    'nav.ocean': 'OCEAN',
    'nav.air': 'AEROSPACE',
    'nav.ocean.title': 'OceanSentinel — live AIS, sonar and LiDAR maritime picture',
    'nav.air.title': 'Project Icarus — live ADS-B aircraft picture',
    'chip.ws.connecting': 'WS CONNECTING',
    'chip.ws.live': 'WS LIVE',
    'chip.ws.down': 'WS DOWN',
    'chip.mode.sim': 'SIM + LIVE FEEDS',
    'chip.mode.live': 'LIVE FEEDS',
    'chip.mode': 'MODE ?',
    'chip.feed.waiting': 'FEED WAITING',
    'chip.feed.live': 'LIVE',
    'chip.feed.error': 'ERROR',
    'chip.feed.throttled': 'RATE LIMITED · RESTING',
    'chip.mode.regional': 'REGIONAL',
    'chip.mode.global': 'GLOBAL VIEW · MIL SWEEP',
    'chip.mode.coarse': 'COARSE',
    'state.live': 'LIVE',
    'state.wait': 'WAIT',
    'state.error': 'ERROR',
    'state.throttled': 'THROTTLED',

    // ---- tabs
    'tab.tracks': 'TRACKS',
    'tab.aircraft': 'AIRCRAFT',
    'tab.alerts': 'ALERTS',
    'tab.watch': 'WATCH',
    'tab.layers': 'LAYERS',
    'tab.gfw': 'GFW',

    // ---- search and filters
    'search.ships': 'search name, MMSI, class…',
    'search.air': 'search callsign, hex, type, tail…',
    'filter.all': 'ALL',
    'filter.airborne': 'AIRBORNE',
    'filter.ground': 'GROUND',
    'filter.emergency': 'EMERGENCY',
    'filter.military': 'MILITARY',
    'filter.watchlist': 'WATCHED',
    'filter.lost': 'LOST CONTACT',
    'class.any': 'all classes',
    'class.fishing': 'fishing / sailing',
    'class.cargo': 'cargo / tanker / tug',
    'class.passenger': 'passenger / patrol / SAR',
    'class.naval': 'naval / carrier',
    'class.other': 'other / unknown',
    'sort.alerts': 'sort: alerts first',
    'sort.priority': 'sort: detections first',
    'sort.distance': 'sort: nearest to view',
    'sort.altitude': 'sort: highest',
    'sort.speed': 'sort: fastest',
    'sort.age': 'sort: most recently heard',
    'sort.callsign': 'sort: callsign',
    'sort.name': 'sort: name',
    'inview': 'in view only',
    'export': 'EXPORT',
    'clear': 'CLEAR',
    'loading': 'loading…',

    // ---- counts and status bar
    'count.tracks': '{n} tracks',
    'count.tracks.capped': '{shown} of {total} tracks',
    'count.aircraft': '{n} aircraft',
    'count.aircraft.capped': '{shown} of {total} tracked',
    'count.watched': '{n} watched',
    'stat.tracks': 'TRACKS',
    'stat.ais': 'AIS',
    'stat.dark': 'DARK',
    'stat.sensor': 'SENSOR',
    'stat.corroborated': 'CORROBORATED',
    'stat.alerts': 'ALERTS',
    'stat.uptime': 'UPTIME',
    'stat.aircraft': 'aircraft',
    'stat.airborne': 'airborne',
    'stat.military': 'military',
    'stat.emergency': 'emergency',
    'stat.lost': 'lost contact',
    'stat.watched': 'watched',
    'stat.carriers': 'carriers',
    'stat.requests': 'req',
    'stat.out.of.view': 'out of view',
    'stat.tracks': 'TRACKS',
    'stat.ais': 'AIS',
    'stat.dark': 'DARK',
    'stat.sensor': 'SENSOR',
    'stat.corroborated': 'CORROBORATED',
    'stat.uptime': 'UPTIME',
    'no.feeds': 'no feeds',
    'no.feed': 'no feed',

    // ---- list badges
    'badge.dark': 'DARK',
    'badge.carrier': '★ CARRIER',
    'badge.naval': 'NAVAL',
    'badge.mil': 'MIL',
    'badge.watch': 'WATCH',
    'badge.lost': 'LOST',
    'badge.sensor.only': 'sensor only',
    'badge.static.only': 'static only',
    'ground.short': 'GND',

    // ---- watch panel
    'watch.ships.hint': 'Watchlist hits raise a high-severity alert — in the UI and to any configured webhook or Telegram destination. Match by MMSI, IMO, or an exact vessel name.',
    'watch.air.hint': 'Watched aircraft raise a high-severity alert — in the UI and to any configured webhook or Telegram destination. Match by ICAO 24-bit hex, callsign, or tail number.',
    'watch.add': 'ADD TO WATCHLIST',
    'watch.input.ships': 'MMSI, IMO 9123456, or exact name',
    'watch.input.air': 'hex 40643d, callsign EZY21UM, or tail G-EZUC',
    'watch.note': 'note (optional)',
    'watch.button.ships': 'WATCH FOR THIS',
    'watch.button.air': 'WATCH FOR THIS',
    'watch.none': 'Nothing watched yet.',

    // ---- layers panel
    'group.domain': 'DATA DOMAIN',
    'group.basemap': 'BASEMAP',
    'group.sources': 'SOURCES',
    'group.layers': 'MAP LAYERS',
    'group.geofence': 'GEOFENCE',
    'group.detections': 'DETECTIONS',
    'group.flightstate': 'FLIGHT STATE',
    'group.feed': 'FEED',
    'group.eventtypes': 'EVENT TYPES',
    'group.regionquery': 'REGION QUERY',
    'group.lookup': 'VESSEL LOOKUP',
    'group.replay': 'REPLAY',
    'domain.hint': 'Both maps run from the same process: the maritime picture and the air picture are separate stores with separate feeds.',
    'domain.ocean': 'OCEAN · SHIPS',
    'domain.air': 'AEROSPACE · AIRCRAFT',
    'layer.vessels': 'Vessel icons',
    'layer.labels': 'Vessel labels',
    'layer.trails': 'Track history trails',
    'layer.own': 'Own ship',
    'layer.zones': 'Geofence zones',
    'layer.gfw': 'GFW fishing events',
    'layer.aircraft': 'Aircraft icons',
    'layer.callsigns': 'Callsign labels',
    'layer.flighttrails': 'Flight trails',
    'layer.rings': 'Detection rings',
    'layer.coverage': 'Receiver query coverage',
    'layer.surface': 'Naval surface picture (carriers)',
    'layer.night': 'Day / night + twilight bands',
    'layer.meridians': 'UTC hour meridians',
    'src.ais': 'AIS (radio)',
    'src.sonar': 'Sonar / radar targets',
    'src.lidar': 'LiDAR contacts',
    'src.dark': 'Dark contacts (no AIS)',
    'det.emergency': 'Emergency squawks',
    'det.military': 'Military / government',
    'det.watchlist': 'Watchlist contacts',
    'det.lost': 'Lost contact',
    'flight.airborne': 'Airborne',
    'flight.ground': 'On the ground',
    'btn.drawzone': 'DRAW ZONE',
    'btn.clearzones': 'CLEAR ALL ZONES',

    // ---- GFW panel
    'gfw.fishing': 'apparent fishing',
    'gfw.encounter': 'encounters (transshipment)',
    'gfw.loitering': 'loitering',
    'gfw.gap': 'AIS gaps',
    'gfw.portvisit': 'port visits',
    'gfw.query.view': 'EVENTS IN VIEW (14d)',
    'gfw.query.global': 'GLOBAL EVENTS (7d)',
    'gfw.clear': 'CLEAR EVENTS',
    'gfw.lookup': 'MMSI / IMO / name',
    'gfw.lookup.btn': 'LOOK UP VESSEL',
    'gfw.replay.load': 'LOAD LAST HOUR',
    'gfw.replay.hint': 'Requires [storage] record_tracks = true in config.toml.',

    // ---- legend
    'leg.ais': 'AIS',
    'leg.sonar': 'Sonar/radar',
    'leg.lidar': 'LiDAR',
    'leg.fused': 'AIS + sensor',
    'leg.dark': 'Dark contact',
    'leg.carrier': 'Aircraft carrier (ringed)',
    'leg.naval': 'Warship / naval',
    'leg.night': 'night (sun below horizon)',
    'leg.twilight': 'twilight · civil −6° / nautical −12° / astronomical −18°',
    'leg.terminator': 'terminator · solar noon',
    'leg.meridian': 'solar hour meridian (UTC±N, not political zones)',
    'leg.ground': 'on ground',
    'leg.band.low': 'below 5 000 ft',
    'leg.band.mid': '5 000–20 000 ft',
    'leg.band.high': '20 000–35 000 ft',
    'leg.band.ultra': 'above 35 000 ft',
    'leg.emergency': 'emergency ring',
    'leg.milring': 'military ring',
    'leg.watchring': 'watchlist ring',
    'leg.carrierops': 'aircraft carrier + ops area',
    'leg.surface': 'warship / naval',

    // ---- drawer
    'drawer.statevector': 'STATE VECTOR',
    'drawer.airframe': 'AIRFRAME RECORD',
    'drawer.airframe.loading': "Loading the network's airframe record…",
    'drawer.airframe.none': 'The network has no extra record for this airframe (common for military and privately-registered aircraft).',
    'drawer.airframe.unknown': 'Only the state vector above is known for this airframe.',
    'drawer.airframe.fail': 'Airframe record unavailable.',
    'btn.flyto': 'FLY TO',
    'btn.center': 'CENTER',
    'btn.copyjson': 'COPY JSON',
    'btn.copyhex': 'COPY HEX',
    'btn.watch': 'WATCH',
    'btn.watch.this': 'WATCH THIS AIRCRAFT',
    'btn.watched': 'ALREADY WATCHED',
    'btn.oceanmap': 'OCEAN MAP ↗',
    'btn.airmap': 'AEROSPACE MAP ↗',
    'btn.gfwmap': 'GFW MAP ↗',
    'btn.remove': 'remove',
    'badge.carrier.long': 'AIRCRAFT CARRIER',
    'badge.naval.long': 'NAVAL / WARSHIP',
    'badge.surface': 'SURFACE PICTURE',
    'badge.dark.long': 'DARK CONTACT — NO AIS',
    'badge.corroborated': 'SENSOR-CORROBORATED',
    'surface.hint': 'A carrier shows here only while it transmits AIS — carriers routinely go dark at sea, so an empty list is not evidence of absence.',
    'surface.source': 'OceanSentinel AIS picture (same store as the sea map)',

    // ---- boot overlay
    'boot.skip': 'CLICK OR PRESS ANY KEY TO SKIP',
    'boot.air.title': 'PROJECT ICARUS',
    'boot.air.sub': 'AEROSPACE DOMAIN AWARENESS',

    // ---- misc dynamic
    'solar.label': 'solar',
    'cursor.hint': '—',
    'alert.kind.emergency': 'EMERGENCY',
    'alert.kind.watchlist': 'WATCHLIST',
    'alert.kind.military': 'MILITARY',
    'alert.kind.lost': 'LOST CONTACT',
    'alert.kind.carrier': 'CARRIER',
    'alert.kind.dark': 'DARK CONTACT',
    'alert.kind.collision': 'COLLISION RISK',
    'alert.kind.aislost': 'AIS LOST',
    'alert.kind.corroborated': 'CORROBORATED',
    'alert.kind.zone': 'GEOFENCE',
    'alert.kind.feed': 'FEED',
    'alerts.empty.ships': 'No alerts yet.',
    'alerts.empty.air': 'No air alerts yet. Emergency squawks, watched aircraft and lost contacts raise one.',
    'panel.alerts.ships': 'Live alert feed',
    'panel.alerts.air': 'Air alert feed',
    'banner.copied': 'Copied {what}',
    'banner.handed': 'Showing {name} handed over from the air map',
    'banner.lang': 'Interface language set to {name}',
  };

  packs.en = { name: 'English', dir: 'ltr', strings: EN };

  function t(key, vars) {
    const pack = packs[current] || packs.en;
    let s = (pack.strings && pack.strings[key]) || EN[key] || key;
    if (vars) {
      for (const k of Object.keys(vars)) {
        s = s.split('{' + k + '}').join(String(vars[k]));
      }
    }
    return s;
  }

  /// Register a language pack. Called by ui/lang/<code>.js.
  function add(code, pack) {
    if (!pack || !pack.strings) return;
    packs[code] = pack;
  }

  function languages() {
    return Object.keys(packs)
      .filter((c) => packs[c].name)
      .map((c) => ({ code: c, name: packs[c].name, dir: packs[c].dir || 'ltr' }));
  }

  function stored() {
    try {
      return localStorage.getItem(STORE_KEY);
    } catch (e) {
      return null;
    }
  }

  /// Best starting language: an explicit choice, else the browser's preference,
  /// else English.
  function detect() {
    const saved = stored();
    if (saved && packs[saved]) return saved;
    const wanted = [navigator.language, ...(navigator.languages || [])].filter(Boolean);
    for (const tag of wanted) {
      const low = String(tag).toLowerCase();
      const two = low.split('-')[0];
      if (packs[low]) return low;
      if (packs[two]) return two;
    }
    return 'en';
  }

  function setLang(code, opts) {
    current = packs[code] ? code : 'en';
    const pack = packs[current];
    try {
      localStorage.setItem(STORE_KEY, current);
    } catch (e) {
      /* ignore */
    }
    document.documentElement.lang = current;
    document.documentElement.dir = pack.dir || 'ltr';
    apply();
    const sel = document.getElementById('lang');
    if (sel && sel.value !== current) sel.value = current;
    if (!(opts && opts.silent)) {
      window.dispatchEvent(new CustomEvent('os:lang', { detail: { code: current, name: pack.name } }));
    }
  }

  /// Render every element that carries a translation attribute.
  function apply(root) {
    const scope = root || document;
    for (const el of scope.querySelectorAll('[data-i18n]')) {
      // Translation targets must be leaves: assigning textContent would delete
      // any child markup (that is how the ALERTS tab once lost its badge).
      if (el.children.length > 0) {
        console.warn('i18n: [data-i18n] on a container, skipping', el);
        continue;
      }
      el.textContent = t(el.getAttribute('data-i18n'));
    }
    for (const el of scope.querySelectorAll('[data-i18n-title]')) {
      el.setAttribute('title', t(el.getAttribute('data-i18n-title')));
    }
    for (const el of scope.querySelectorAll('[data-i18n-placeholder]')) {
      el.setAttribute('placeholder', t(el.getAttribute('data-i18n-placeholder')));
    }
    for (const el of scope.querySelectorAll('[data-i18n-aria]')) {
      el.setAttribute('aria-label', t(el.getAttribute('data-i18n-aria')));
    }
  }

  /// Build the <select> in the top bar, listing every registered language in
  /// its own name so a speaker can find theirs without reading English.
  function mount(select) {
    if (!select) return;
    select.innerHTML = languages()
      .map((l) => `<option value="${l.code}">${l.name}</option>`)
      .join('');
    select.value = current;
    // setLang fires 'os:lang' itself, for dropdown changes and programmatic
    // ones alike, so pages only need the one listener.
    select.addEventListener('change', () => setLang(select.value));
  }

  function init() {
    setLang(detect(), { silent: true });
    mount(document.getElementById('lang'));
    return current;
  }

  return { t, add, setLang, apply, mount, init, languages, detect, get lang() { return current; } };
})();

/// Short alias used throughout the pages.
const t = (key, vars) => window.OSi18n.t(key, vars);
