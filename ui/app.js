'use strict';

/* OceanSentinel live map. Data arrives over /ws as:
   {"type":"state", tracks, alerts, zones, feeds, stats, own_ship, gfw_events, app}
   {"type":"alert", alert}                                            */

const COLORS = {
  ais: '#22d3ee',
  sonar: '#fb923c',
  lidar: '#4ade80',
  radar: '#a78bfa',
  fused: '#f8fafc',
  dark: '#ef4444',
};

const state = {
  tracks: new Map(),
  alerts: [],
  zones: [],
  feeds: [],
  stats: {},
  app: {},
  ownShip: null,
  gfwEvents: [],
  selected: null,
  standalone: null,
  search: '',
  filters: { ais: true, sonar: true, lidar: true, dark: true },
  layers: { tracks: true, labels: true, trails: true, own: true, zones: true, gfw: true },
  wsOk: false,
  firstState: true,
  userMoved: false,
  drawing: null,
  drawerBusy: false,
  darkMarkers: new Map(),
  sawTileErrors: false,
};

const esc = (s) =>
  String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

const num = (v, d = 4) => (v === null || v === undefined || Number.isNaN(Number(v)) ? '—' : Number(v).toFixed(d));

function ageStr(iso) {
  if (!iso) return '—';
  const s = Math.max(0, (Date.now() - Date.parse(iso)) / 1000);
  if (s < 60) return `${Math.round(s)}s`;
  if (s < 3600) return `${Math.round(s / 60)}m`;
  return `${(s / 3600).toFixed(1)}h`;
}

function banner(msg, kind = '') {
  const el = document.getElementById('banner');
  el.textContent = msg;
  el.className = kind;
  clearTimeout(banner._t);
  if (msg) banner._t = setTimeout(() => el.classList.add('hidden'), 6500);
  else el.classList.add('hidden');
}

/* ------------------------------------------------------------------ map */

// Basemap options. The CARTO "dark" style is nearly featureless over open
// ocean (a flat grey plate), so satellite imagery is the default: coastlines,
// land and shoals are always visible, and vessel colours read well on it.
const BASEMAPS = {
  satellite: {
    label: 'SATELLITE',
    tiles: [
      'https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}',
    ],
    tileSize: 256,
    attribution: 'Imagery © Esri, Maxar, Earthstar Geographics',
    paint: { 'raster-opacity': 0.9, 'raster-saturation': -0.1, 'raster-brightness-max': 0.9 },
  },
  dark: {
    label: 'DARK',
    tiles: ['a', 'b', 'c', 'd'].map((s) => `https://${s}.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}.png`),
    tileSize: 256,
    attribution: '© OpenStreetMap contributors © CARTO',
    paint: { 'raster-opacity': 0.95, 'raster-brightness-max': 1.0 },
  },
  streets: {
    label: 'STREETS',
    tiles: ['https://tile.openstreetmap.org/{z}/{x}/{y}.png'],
    tileSize: 256,
    attribution: '© OpenStreetMap contributors',
    paint: { 'raster-opacity': 0.85 },
  },
};

let activeBasemap = (() => {
  try {
    const saved = localStorage.getItem('os.basemap');
    if (saved && BASEMAPS[saved]) return saved;
  } catch (e) {
    /* ignore */
  }
  return 'satellite';
})();

const baseSources = {};
const baseLayers = [];
for (const [key, b] of Object.entries(BASEMAPS)) {
  baseSources['basemap-' + key] = {
    type: 'raster',
    tiles: b.tiles,
    tileSize: b.tileSize,
    attribution: b.attribution,
  };
  baseLayers.push({
    id: 'basemap-' + key,
    type: 'raster',
    source: 'basemap-' + key,
    layout: { visibility: key === activeBasemap ? 'visible' : 'none' },
    paint: b.paint,
  });
}

const map = new maplibregl.Map({
  container: 'map',
  style: {
    version: 8,
    // Open Sans Regular lives on openmaptiles; the demotiles path 404s and
    // silently kills every text label.
    glyphs: 'https://fonts.openmaptiles.org/{fontstack}/{range}.pbf',
    sources: baseSources,
    layers: [{ id: 'bg', type: 'background', paint: { 'background-color': '#04070d' } }, ...baseLayers],
  },
  center: [-5.36, 36.02],
  zoom: 9,
  attributionControl: { compact: true },
});

function setBasemap(key) {
  if (!BASEMAPS[key]) return;
  activeBasemap = key;
  try {
    localStorage.setItem('os.basemap', key);
  } catch (e) {
    /* ignore */
  }
  for (const k of Object.keys(BASEMAPS)) {
    if (map.getLayer('basemap-' + k)) {
      map.setLayoutProperty('basemap-' + k, 'visibility', k === key ? 'visible' : 'none');
    }
  }
  const row = document.getElementById('basemap-row');
  if (row) {
    row.querySelectorAll('.fchip').forEach((el) => el.classList.toggle('on', el.dataset.b === key));
  }
}

/// Lat/lon grid, drawn locally so the map always has visible structure even
/// with no basemap tiles at all.
function graticuleFC() {
  const b = map.getBounds();
  const z = map.getZoom();
  const step = z < 2.5 ? 30 : z < 4.5 ? 10 : z < 7 ? 5 : z < 9.5 ? 1 : 0.25;
  const w = Math.max(-180, b.getWest());
  const e = Math.min(180, b.getEast());
  const s = Math.max(-85, b.getSouth());
  const n = Math.min(85, b.getNorth());
  const feats = [];
  const isMajor = (v) => Math.abs(v % (step * 2)) < step * 1e-6;
  for (let lon = Math.floor(w / step) * step; lon <= e; lon += step) {
    feats.push({
      type: 'Feature',
      geometry: { type: 'LineString', coordinates: [[lon, s], [lon, n]] },
      properties: { major: isMajor(lon) ? 1 : 0 },
    });
  }
  for (let lat = Math.floor(s / step) * step; lat <= n; lat += step) {
    feats.push({
      type: 'Feature',
      geometry: { type: 'LineString', coordinates: [[w, lat], [e, lat]] },
      properties: { major: isMajor(lat) ? 1 : 0 },
    });
  }
  return { type: 'FeatureCollection', features: feats };
}

function updateGraticule() {
  const src = map.getSource('graticule');
  if (src) src.setData(graticuleFC());
}

map.on('dragstart', () => (state.userMoved = true));
map.on('zoomstart', () => (state.userMoved = true));

map.on('error', (e) => {
  const msg = (e && e.error && e.error.message) || '';
  if (/tile|fetch|Failed to load|NetworkError/i.test(msg) && !state.sawTileErrors) {
    state.sawTileErrors = true;
    banner('Basemap tiles unreachable — showing the local lat/lon grid instead (tracks stay live). Switch basemap in the LAYERS tab.', 'warn');
  }
});

function shipIcon(color, size = 34) {
  const c = document.createElement('canvas');
  c.width = size;
  c.height = size;
  const ctx = c.getContext('2d');
  const s = size;
  ctx.beginPath();
  ctx.moveTo(s / 2, 2.5);
  ctx.lineTo(s * 0.84, s - 5);
  ctx.lineTo(s / 2, s * 0.7);
  ctx.lineTo(s * 0.16, s - 5);
  ctx.closePath();
  ctx.fillStyle = color;
  ctx.fill();
  ctx.lineWidth = 2;
  ctx.strokeStyle = 'rgba(2,6,12,0.9)';
  ctx.stroke();
  return ctx.getImageData(0, 0, s, s);
}

function ownIcon() {
  const s = 40;
  const c = document.createElement('canvas');
  c.width = s;
  c.height = s;
  const ctx = c.getContext('2d');
  ctx.beginPath();
  ctx.arc(s / 2, s / 2, 8, 0, Math.PI * 2);
  ctx.fillStyle = '#facc15';
  ctx.fill();
  ctx.lineWidth = 2;
  ctx.strokeStyle = '#111';
  ctx.stroke();
  ctx.beginPath();
  ctx.moveTo(s / 2, 2);
  ctx.lineTo(s / 2, 12);
  ctx.moveTo(s / 2, s - 12);
  ctx.lineTo(s / 2, s - 2);
  ctx.moveTo(2, s / 2);
  ctx.lineTo(12, s / 2);
  ctx.moveTo(s - 12, s / 2);
  ctx.lineTo(s - 2, s / 2);
  ctx.strokeStyle = '#facc15';
  ctx.stroke();
  return ctx.getImageData(0, 0, s, s);
}

const EMPTY_FC = { type: 'FeatureCollection', features: [] };

map.on('load', () => {
  map.addImage('v-ais', shipIcon(COLORS.ais), { pixelRatio: 2 });
  map.addImage('v-sonar', shipIcon(COLORS.sonar), { pixelRatio: 2 });
  map.addImage('v-lidar', shipIcon(COLORS.lidar), { pixelRatio: 2 });
  map.addImage('v-radar', shipIcon(COLORS.radar), { pixelRatio: 2 });
  map.addImage('v-fused', shipIcon(COLORS.fused), { pixelRatio: 2 });
  map.addImage('v-dark', shipIcon(COLORS.dark), { pixelRatio: 2 });
  map.addImage('v-own', ownIcon(), { pixelRatio: 2 });

  for (const id of ['zones', 'gfw', 'trails', 'tracks', 'own', 'draw', 'graticule']) {
    map.addSource(id, { type: 'geojson', data: EMPTY_FC });
  }

  // graticule sits directly above the basemap, below all data layers
  map.addLayer({
    id: 'graticule',
    type: 'line',
    source: 'graticule',
    paint: {
      'line-color': [
        'case',
        ['==', ['get', 'major'], 1],
        'rgba(148,163,184,0.32)',
        'rgba(100,116,139,0.15)',
      ],
      'line-width': ['case', ['==', ['get', 'major'], 1], 1, 0.6],
    },
  });
  updateGraticule();
  map.on('moveend', updateGraticule);

  map.addLayer({
    id: 'zones-fill',
    type: 'fill',
    source: 'zones',
    paint: { 'fill-color': ['get', 'color'], 'fill-opacity': 0.08 },
  });
  map.addLayer({
    id: 'zones-line',
    type: 'line',
    source: 'zones',
    paint: { 'line-color': ['get', 'color'], 'line-width': 1.2, 'line-dasharray': [3, 3], 'line-opacity': 0.8 },
  });

  map.addLayer({
    id: 'gfw-circles',
    type: 'circle',
    source: 'gfw',
    paint: {
      'circle-radius': ['interpolate', ['linear'], ['zoom'], 3, 3, 10, 7],
      'circle-color': '#f59e0b',
      'circle-opacity': 0.55,
      'circle-stroke-color': '#fbbf24',
      'circle-stroke-width': 1,
    },
  });

  map.addLayer({
    id: 'trails-line',
    type: 'line',
    source: 'trails',
    layout: { 'line-cap': 'round', 'line-join': 'round' },
    paint: {
      'line-color': ['get', 'color'],
      'line-width': ['interpolate', ['linear'], ['zoom'], 3, 1, 10, 2],
      'line-opacity': 0.55,
    },
  });

  map.addLayer({
    id: 'sel-ring',
    type: 'circle',
    source: 'tracks',
    filter: ['==', ['get', 'id'], ''],
    paint: {
      'circle-radius': 17,
      'circle-color': 'rgba(0,0,0,0)',
      'circle-stroke-color': '#22d3ee',
      'circle-stroke-width': 2,
    },
  });

  map.addLayer({
    id: 'tracks-icons',
    type: 'symbol',
    source: 'tracks',
    layout: {
      'icon-image': [
        'match',
        ['get', 'style'],
        'ais', 'v-ais',
        'sonar', 'v-sonar',
        'lidar', 'v-lidar',
        'radar', 'v-radar',
        'dark', 'v-dark',
        'v-fused',
      ],
      'icon-size': ['interpolate', ['linear'], ['zoom'], 3, 0.42, 8, 0.62, 12, 0.95],
      'icon-rotate': ['coalesce', ['get', 'heading'], 0],
      'icon-rotation-alignment': 'map',
      'icon-allow-overlap': true,
      'icon-ignore-placement': true,
    },
    paint: { 'icon-opacity': 0.96 },
  });

  map.addLayer({
    id: 'tracks-labels',
    type: 'symbol',
    source: 'tracks',
    minzoom: 7.2,
    layout: {
      // must match a font that actually exists on the glyph server, otherwise
      // MapLibre silently renders no text at all
      'text-font': ['Open Sans Regular'],
      'text-field': ['get', 'name'],
      'text-size': 10,
      'text-offset': [0, 1.5],
      'text-anchor': 'top',
      'text-optional': true,
      'text-allow-overlap': false,
    },
    paint: {
      'text-color': ['case', ['get', 'dark'], '#fca5a5', '#dbe9f7'],
      'text-halo-color': '#03060b',
      'text-halo-width': 1.6,
    },
  });

  map.addLayer({
    id: 'own-ring',
    type: 'circle',
    source: 'own',
    paint: {
      'circle-radius': ['interpolate', ['linear'], ['zoom'], 3, 5, 12, 16],
      'circle-color': 'rgba(250,204,21,0.10)',
      'circle-stroke-color': 'rgba(250,204,21,0.45)',
      'circle-stroke-width': 1,
    },
  });
  map.addLayer({
    id: 'own-icon',
    type: 'symbol',
    source: 'own',
    layout: { 'icon-image': 'v-own', 'icon-size': 0.7, 'icon-allow-overlap': true },
  });

  map.addLayer({
    id: 'draw-fill',
    type: 'fill',
    source: 'draw',
    paint: { 'fill-color': '#38bdf8', 'fill-opacity': 0.12 },
  });
  map.addLayer({
    id: 'draw-line',
    type: 'line',
    source: 'draw',
    paint: { 'line-color': '#38bdf8', 'line-width': 1.6, 'line-dasharray': [4, 3] },
  });

  applyLayerVisibility();
  pushData();
});

map.on('mousemove', (e) => {
  document.getElementById('cursor-readout').textContent =
    `${e.lngLat.lat.toFixed(5)}, ${e.lngLat.lng.toFixed(5)}`;
});

map.on('click', (e) => {
  if (state.drawing) {
    state.drawing.push([e.lngLat.lng, e.lngLat.lat]);
    pushDraw();
    return;
  }
  const f = map.queryRenderedFeatures(e.point, { layers: ['tracks-icons'] })[0];
  if (f && f.properties && f.properties.id) selectTrack(f.properties.id);
});

map.on('dblclick', () => {
  if (state.drawing && state.drawing.length >= 3) finishZone();
});

map.on('mouseenter', 'tracks-icons', () => (map.getCanvas().style.cursor = state.drawing ? 'crosshair' : 'pointer'));
map.on('mouseleave', 'tracks-icons', () => (map.getCanvas().style.cursor = state.drawing ? 'crosshair' : ''));

// Clicking a GFW fishing-event dot pulls that vessel's registry record.
map.on('click', 'gfw-circles', (e) => {
  const p = e.features && e.features[0] && e.features[0].properties;
  if (p && p.ssvid) gfwLookup(String(p.ssvid));
});
map.on('mouseenter', 'gfw-circles', () => (map.getCanvas().style.cursor = 'pointer'));
map.on('mouseleave', 'gfw-circles', () => (map.getCanvas().style.cursor = ''));

/* ------------------------------------------------------------ data -> map */

function styleOf(t) {
  if (t.dark) return 'dark';
  const s = t.sources || [];
  const sensor = s.filter((x) => x !== 'ais');
  if (s.includes('ais') && sensor.length > 0) return 'fused';
  if (sensor.includes('lidar')) return 'lidar';
  if (sensor.includes('sonar')) return 'sonar';
  if (sensor.includes('radar')) return 'radar';
  return 'ais';
}

function labelOf(t) {
  if (t.name) return t.name;
  if (t.mmsi) return 'MMSI ' + t.mmsi;
  return t.id;
}

function visible(t) {
  const f = state.filters;
  const s = t.sources || [];
  if (t.dark) return f.dark;
  return (
    (s.includes('ais') && f.ais) ||
    (s.includes('sonar') && f.sonar) ||
    (s.includes('lidar') && f.lidar) ||
    (s.includes('radar') && f.sonar)
  );
}

function matchSearch(t) {
  const q = state.search.trim().toLowerCase();
  if (!q) return true;
  return (
    labelOf(t).toLowerCase().includes(q) ||
    String(t.mmsi || '').includes(q) ||
    String(t.mmsi || '').includes(q) ||
    String(t.imo || '').includes(q) ||
    (t.classification || '').toLowerCase().includes(q)
  );
}

function tracksFC() {
  const feats = [];
  for (const t of state.tracks.values()) {
    if (!visible(t) || !matchSearch(t)) continue;
    feats.push({
      type: 'Feature',
      geometry: { type: 'Point', coordinates: [t.lon, t.lat] },
      properties: {
        id: t.id,
        name: labelOf(t),
        style: styleOf(t),
        heading: t.heading ?? t.cog ?? 0,
        dark: !!t.dark,
        cls: t.classification || '',
      },
    });
  }
  return { type: 'FeatureCollection', features: feats };
}

function trailsFC() {
  const feats = [];
  for (const t of state.tracks.values()) {
    if (!visible(t) || !matchSearch(t)) continue;
    const trail = t.trail || [];
    if (trail.length < 2) continue;
    feats.push({
      type: 'Feature',
      geometry: { type: 'LineString', coordinates: trail.map((p) => [p.lon, p.lat]) },
      properties: { color: COLORS[styleOf(t)] || COLORS.ais, id: t.id },
    });
  }
  return { type: 'FeatureCollection', features: feats };
}

function zonesFC() {
  return {
    type: 'FeatureCollection',
    features: state.zones.map((z) => ({
      type: 'Feature',
      geometry: { type: 'Polygon', coordinates: [[...z.polygon, z.polygon[0]]] },
      properties: { color: z.color || '#38bdf8', name: z.name },
    })),
  };
}

function gfwFC() {
  return {
    type: 'FeatureCollection',
    features: state.gfwEvents.map((e) => ({
      type: 'Feature',
      geometry: { type: 'Point', coordinates: [e.lon, e.lat] },
      properties: {
        name: e.vessel_name || '',
        type: e.event_type,
        ssvid: e.ssvid || '',
        vessel_id: e.vessel_id || '',
      },
    })),
  };
}

function ownFC() {
  if (!state.ownShip) return EMPTY_FC;
  return {
    type: 'FeatureCollection',
    features: [
      {
        type: 'Feature',
        geometry: { type: 'Point', coordinates: [state.ownShip.lon, state.ownShip.lat] },
        properties: {},
      },
    ],
  };
}

function pushData() {
  const set = (id, data) => {
    const src = map.getSource(id);
    if (src) src.setData(data);
  };
  set('tracks', tracksFC());
  set('trails', trailsFC());
  set('zones', zonesFC());
  set('gfw', gfwFC());
  set('own', ownFC());
  updateDarkMarkers();
}

function pushDraw() {
  const src = map.getSource('draw');
  if (!src) return;
  const pts = state.drawing || [];
  if (pts.length === 0) {
    src.setData(EMPTY_FC);
    return;
  }
  if (pts.length === 1) {
    src.setData({
      type: 'FeatureCollection',
      features: [
        { type: 'Feature', geometry: { type: 'Point', coordinates: pts[0] }, properties: {} },
      ],
    });
    return;
  }
  src.setData({
    type: 'FeatureCollection',
    features: [
      { type: 'Feature', geometry: { type: 'LineString', coordinates: pts }, properties: {} },
    ],
  });
}

function updateDarkMarkers() {
  const keep = new Set();
  for (const t of state.tracks.values()) {
    if (!t.dark || !visible(t)) continue;
    keep.add(t.id);
    let m = state.darkMarkers.get(t.id);
    if (!m) {
      const el = document.createElement('div');
      el.className = 'dark-pulse';
      el.style.cssText =
        'width:26px;height:26px;border-radius:50%;border:2px solid rgba(239,68,68,.9);box-shadow:0 0 12px rgba(239,68,68,.8);animation:pulse 1.4s infinite';
      m = new maplibregl.Marker({ element: el }).setLngLat([t.lon, t.lat]).addTo(map);
      state.darkMarkers.set(t.id, m);
    } else {
      m.setLngLat([t.lon, t.lat]);
    }
  }
  for (const [id, m] of state.darkMarkers) {
    if (!keep.has(id)) {
      m.remove();
      state.darkMarkers.delete(id);
    }
  }
}

function applyLayerVisibility() {
  const v = (id, on) => {
    if (map.getLayer(id)) map.setLayoutProperty(id, 'visibility', on ? 'visible' : 'none');
  };
  v('tracks-icons', state.layers.tracks);
  v('tracks-labels', state.layers.labels);
  v('trails-line', state.layers.trails);
  v('zones-fill', state.layers.zones);
  v('zones-line', state.layers.zones);
  v('gfw-circles', state.layers.gfw);
  v('own-icon', state.layers.own);
  v('own-ring', state.layers.own);
}

/* ------------------------------------------------------------------- ws */

function connect() {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const ws = new WebSocket(`${proto}://${location.host}/ws`);
  ws.onopen = () => {
    state.wsOk = true;
    chip('chip-ws', 'WS LIVE', 'ok');
  };
  ws.onclose = () => {
    state.wsOk = false;
    chip('chip-ws', 'WS DOWN', 'bad');
    setTimeout(connect, 2000);
  };
  ws.onerror = () => ws.close();
  ws.onmessage = (ev) => {
    let msg;
    try {
      msg = JSON.parse(ev.data);
    } catch {
      return;
    }
    if (msg.type === 'state') applyState(msg);
    else if (msg.type === 'alert') pushAlert(msg.alert);
  };
}

function applyState(msg) {
  state.tracks = new Map((msg.tracks || []).map((t) => [t.id, t]));
  state.alerts = msg.alerts || [];
  state.zones = msg.zones || [];
  state.feeds = msg.feeds || [];
  state.stats = msg.stats || {};
  state.app = msg.app || {};
  state.ownShip = msg.own_ship || null;
  state.gfwEvents = msg.gfw_events || [];

  if (state.firstState && state.app.aoi && !state.userMoved) {
    state.firstState = false;
    const a = state.app.aoi;
    map.jumpTo({ center: [a.lon, a.lat], zoom: a.zoom || 9 });
  }

  pushData();
  renderTrackList();
  renderAlerts();
  renderZones();
  renderFeeds();
  renderStats();
  renderChips();
  if (state.selected || state.standalone) renderDrawer();
}

function pushAlert(a) {
  const idx = state.alerts.findIndex((x) => x.id === a.id);
  if (idx === -1) state.alerts.unshift(a);
  renderAlerts();
  if (a.kind === 'dark_contact') banner(a.message, 'bad');
  else if (a.kind === 'ais_lost') banner(a.message, 'warn');
}

/* --------------------------------------------------------------- panels */

function chip(id, text, kind = '', title = '') {
  const el = document.getElementById(id);
  el.textContent = text;
  el.className = 'chip ' + kind;
  el.title = title;
}

function renderChips() {
  const app = state.app || {};
  chip('ver', '');
  document.getElementById('ver').textContent = app.version ? 'v' + app.version : '';
  chip('chip-mode', app.simulation ? 'SIM + LIVE FEEDS' : 'LIVE FEEDS', app.simulation ? 'warn' : 'ok');
  const gfwTip =
    'Global Fishing Watch enrichment is optional. It adds registry identity, fishing effort, AIS-gap history and IUU-list status. ' +
    'Get a free non-commercial token at globalfishingwatch.org/our-apis/tokens and put GFW_API_TOKEN in a .env file next to the app, then restart.';
  if (app.gfw_enabled) {
    chip('chip-gfw', 'GFW LIVE', 'ok', 'Global Fishing Watch enrichment is active.');
  } else if (app.gfw_token) {
    chip('chip-gfw', 'GFW DISABLED', 'warn', 'GFW_API_TOKEN is set but [gfw] enabled = false in config.toml.');
  } else {
    chip('chip-gfw', 'GFW: OPTIONAL — NO TOKEN', 'warn', gfwTip);
  }
  // The AOI (config [aoi]) only sets the opening view; it is deliberately not
  // shown as a chip — with global feeds it read like a data limit.
  const hint = document.getElementById('gfw-hint');
  if (hint) {
    hint.textContent = app.gfw_enabled
      ? 'Global Fishing Watch enrichment is active. Select a vessel with an MMSI, or query fishing events in the current view.'
      : 'Optional: Global Fishing Watch adds registry identity, apparent fishing events, AIS-gap history and IUU-list status. Everything else on this map works without it. To enable it, get a free non-commercial token at globalfishingwatch.org/our-apis/tokens, put GFW_API_TOKEN=… in a .env file next to the app, and restart.';
  }
}

function renderTrackList() {
  const list = document.getElementById('track-list');
  const rows = [];
  for (const t of state.tracks.values()) {
    if (!visible(t) || !matchSearch(t)) continue;
    rows.push(t);
  }
  rows.sort((a, b) => (b.dark ? 1 : 0) - (a.dark ? 1 : 0) || String(b.last_seen).localeCompare(String(a.last_seen)));
  document.getElementById('track-count').textContent = `${rows.length} tracks`;
  list.innerHTML = rows
    .map((t) => {
      const s = styleOf(t);
      const sel = state.selected === t.id ? ' sel' : '';
      const bits = [];
      if (t.dark) bits.push('DARK');
      const motion = [];
      if (t.sog !== null && t.sog !== undefined) motion.push(num(t.sog, 1) + 'kn');
      if (t.cog !== null && t.cog !== undefined) motion.push(Math.round(t.cog) + '°');
      if (motion.length === 0) motion.push(t.dark ? 'sensor only' : 'static only');
      bits.push(...motion, ageStr(t.last_seen));
      const meta = bits.join(' · ');
      return `<div class="row${sel}" data-id="${esc(t.id)}">
        <span class="dot ${s}"></span>
        <span class="nm">${esc(labelOf(t))}</span>
        <span class="meta">${esc(meta)}</span>
      </div>`;
    })
    .join('');
  list.querySelectorAll('.row').forEach((el) =>
    el.addEventListener('click', () => selectTrack(el.dataset.id))
  );
}

function renderAlerts() {
  const list = document.getElementById('alert-list');
  document.getElementById('alert-badge').textContent = String(state.alerts.length);
  list.innerHTML = state.alerts
    .map(
      (a) => `<div class="row" data-lat="${a.lat}" data-lon="${a.lon}" data-track="${esc(a.track_id || '')}">
        <span class="sev ${esc(a.severity)}"></span>
        <span class="alert-msg">${esc(a.message)}</span>
        <span class="alert-time">${ageStr(a.ts)}</span>
      </div>`
    )
    .join('');
  list.querySelectorAll('.row').forEach((el) =>
    el.addEventListener('click', () => {
      map.flyTo({ center: [Number(el.dataset.lon), Number(el.dataset.lat)], zoom: Math.max(map.getZoom(), 10.5) });
      if (el.dataset.track) selectTrack(el.dataset.track);
    })
  );
}

function renderZones() {
  const el = document.getElementById('zone-list');
  if (!el) return;
  el.innerHTML = state.zones
    .map(
      (z) => `<div class="zrow"><span>◻ ${esc(z.name)}</span><span class="zx" data-id="${esc(z.id)}" title="delete">✕</span></div>`
    )
    .join('');
  el.querySelectorAll('.zx').forEach((x) =>
    x.addEventListener('click', async (ev) => {
      ev.stopPropagation();
      await fetch(`/api/zones?id=${encodeURIComponent(x.dataset.id)}`, { method: 'DELETE' });
    })
  );
}

function renderFeeds() {
  document.getElementById('feeds').innerHTML = state.feeds
    .map((f) => {
      const kind = /error|disconnected/.test(f.state) ? 'bad' : /listening|connected|running/.test(f.state) ? 'ok' : 'warn';
      return `<span class="feed"><span class="fstate ${kind}">●</span>${esc(f.name)} <span style="color:#3f5566">${esc(
        f.state
      )} ${f.pps ? f.pps.toFixed(0) + '/s' : ''}</span></span>`;
    })
    .join('');
}

function renderStats() {
  const s = state.stats || {};
  document.getElementById('stats').innerHTML = [
    `TRACKS <b>${s.tracks ?? 0}</b>`,
    `AIS <b>${s.ais ?? 0}</b>`,
    `DARK <b style="color:#ef4444">${s.dark ?? 0}</b>`,
    `SENSOR <b>${s.sensor_tracked ?? 0}</b>`,
    `CORROBORATED <b>${s.corroborated ?? 0}</b>`,
    `ALERTS <b>${s.alerts ?? 0}</b>`,
    `UPTIME <b>${Math.round((s.uptime_s ?? 0) / 60)}m</b>`,
  ].join('');
}

/* --------------------------------------------------------------- drawer */

function selectTrack(id) {
  state.standalone = null;
  state.selected = id;
  if (map.getLayer('sel-ring')) map.setFilter('sel-ring', ['==', ['get', 'id'], id]);
  const t = state.tracks.get(id);
  if (t && t.mmsi && state.app.gfw_enabled && !t.gfw) fetchGfwForTrack(t);
  renderDrawer();
  renderTrackList();
}

function closeDrawer() {
  state.selected = null;
  state.standalone = null;
  document.getElementById('drawer').classList.add('hidden');
  if (map.getLayer('sel-ring')) map.setFilter('sel-ring', ['==', ['get', 'id'], '']);
}

async function fetchGfwForTrack(t) {
  state.drawerBusy = true;
  renderDrawer();
  const url = `/api/gfw/vessel?mmsi=${encodeURIComponent(t.mmsi)}`;
  try {
    let r, data;
    try {
      r = await fetch(url);
      data = await r.json();
    } catch (netErr) {
      // one retry: the app may have been restarting when the click landed
      await new Promise((res) => setTimeout(res, 1500));
      r = await fetch(url);
      data = await r.json();
    }
    const cur = state.tracks.get(t.id);
    if (r.ok) {
      if (cur) cur.gfw = data;
    } else if (r.status === 404) {
      // Not in GFW's registry. That is a normal result, not an error:
      // simulated contacts and vessels with no AIS history have no record.
      if (cur) cur.gfw = { matched: false, note: data.error, fetched_at: new Date().toISOString() };
    } else {
      throw new Error(data.error || r.statusText);
    }
  } catch (e) {
    banner(`GFW lookup failed: ${e.message} — if the app was just restarted, try again in a moment.`, 'warn');
  } finally {
    state.drawerBusy = false;
    renderDrawer();
  }
}

/// Renders the GFW area of the drawer, including the "no registry match" case.
function gfwBlock(gfw, opts = {}) {
  if (!gfw) return '';
  if (gfw.matched === false) {
    return `<div class="sec-title">GLOBAL FISHING WATCH</div>
      <div class="gfw-badge no">NO REGISTRY MATCH</div>
      <div class="hint">${esc(gfw.note || 'Not present in the GFW identity dataset.')}${
        opts.mmsi ? ` (MMSI ${esc(opts.mmsi)})` : ''
      } This is normal for simulated contacts and for vessels with no AIS history — tracking and alerts are unaffected.</div>`;
  }
  return gfwSection(gfw);
}

function jsonBlock(v) {
  if (v === null || v === undefined || (typeof v === 'object' && Object.keys(v).length === 0)) return '';
  return `<details><summary class="mini" style="cursor:pointer">raw response</summary><pre class="json">${esc(
    JSON.stringify(v, null, 1)
  )}</pre></details>`;
}

function gfwSection(gfw) {
  if (!gfw) return '';
  const s = gfw.summary || {};
  const ins = gfw.insights || {};
  const rows = [];
  const push = (k, v) => {
    if (v !== null && v !== undefined && v !== '') rows.push(`<tr><td>${esc(k)}</td><td>${esc(v)}</td></tr>`);
  };
  push('GFW vessel id', gfw.vessel_id || s.vessel_id);
  push('Name', s.name);
  push('Flag', s.flag);
  push('MMSI', s.mmsi);
  push('IMO', s.imo);
  push('Call sign', s.callsign);
  push('Gear type', s.geartype);
  push('Ship type', s.shiptype);
  push('Registry', s.registry);
  push('Owner', s.owner);
  push('Transmitting', s.transmission_from && s.transmission_to ? `${s.transmission_from} → ${s.transmission_to}` : null);
  push('Tracked positions', s.positions);
  const hasInsights = ins && Object.keys(ins).length > 0;
  return `
    <div class="sec-title">GLOBAL FISHING WATCH</div>
    <div class="gfw-badge yes">MATCHED IN GFW</div>
    ${ins && ins.iuu_listed ? '<div class="gfw-badge iuu">ON RFMO IUU VESSEL LIST</div>' : ''}
    <table class="kv">${rows.join('')}</table>
    ${
      hasInsights
        ? `<table class="kv">
            <tr><td>Apparent fishing events (12 mo)</td><td>${esc(ins.fishing_events ?? '—')}</td></tr>
            <tr><td>… in no-take MPAs</td><td>${esc(ins.fishing_events_in_no_take_mpas ?? '—')}</td></tr>
            <tr><td>… without known RFMO authorization</td><td>${esc(ins.fishing_events_in_rfmo_without_authorization ?? '—')}</td></tr>
            <tr><td>AIS coverage</td><td>${ins.ais_coverage_percentage !== null && ins.ais_coverage_percentage !== undefined ? esc(num(ins.ais_coverage_percentage, 1)) + '%' : '—'}</td></tr>
            <tr><td>AIS gap events</td><td>${esc(ins.ais_gap_events ?? '—')}</td></tr>
            <tr><td>Flag changes</td><td>${esc(ins.flag_changes ?? '—')}</td></tr>
          </table>`
        : ''
    }
    ${jsonBlock({ search: gfw.search, detail: gfw.detail, insights: gfw.insights_raw })}
  `;
}

function renderDrawer() {
  const drawer = document.getElementById('drawer');
  const body = document.getElementById('drawer-body');

  if (state.standalone) {
    drawer.classList.remove('hidden');
    body.innerHTML = `
      <h2 class="vname">${esc(state.standalone.title)}</h2>
      <div class="vsub">Global Fishing Watch lookup${state.standalone.gfw && state.standalone.gfw.matched === false ? '' : ' — not a live track'}</div>
      ${gfwBlock(state.standalone.gfw)}
    `;
    return;
  }

  const t = state.selected ? state.tracks.get(state.selected) : null;
  if (!t) {
    drawer.classList.add('hidden');
    return;
  }
  drawer.classList.remove('hidden');

  const s = styleOf(t);
  const badges = (t.sources || [])
    .map((x) => `<span class="vbadge ${esc(x)}">${esc(x.toUpperCase())}</span>`)
    .join('');
  const rows = [];
  const push = (k, v) => {
    if (v !== null && v !== undefined && v !== '' && v !== '—') rows.push(`<tr><td>${esc(k)}</td><td>${esc(v)}</td></tr>`);
  };
  push('MMSI', t.mmsi);
  push('IMO', t.imo);
  push('Call sign', t.callsign);
  push('Type', `${t.classification || 'unknown'}${t.ship_type ? ` (code ${t.ship_type})` : ''}`);
  push('Nav status', t.nav_status);
  push('Position', `${num(t.lat, 5)}, ${num(t.lon, 5)}`);
  push('Speed', t.sog !== null && t.sog !== undefined ? num(t.sog, 1) + ' kn' : null);
  push('Course', t.cog !== null && t.cog !== undefined ? num(t.cog, 1) + '°' : null);
  push('Heading', t.heading !== null && t.heading !== undefined ? num(t.heading, 0) + '°' : null);
  push('Length / beam', t.length ? `${t.length} m / ${t.beam || '?'} m` : null);
  push('Draught', t.draught ? num(t.draught, 1) + ' m' : null);
  push('Destination', t.destination);
  push('Last seen', ageStr(t.last_seen) + ' ago');
  push('First seen', ageStr(t.first_seen) + ' ago');
  push('Confidence', num(t.confidence * 100, 0) + '%');

  const gfwVesselId = t.gfw && t.gfw.vessel_id;
  const gfwHtml = t.gfw
    ? gfwBlock(t.gfw, { mmsi: t.mmsi })
    : t.mmsi && state.app.gfw_enabled
    ? `<div class="sec-title">GLOBAL FISHING WATCH</div><div class="gfw-badge no">${
        state.drawerBusy ? 'QUERYING…' : 'NOT FETCHED'
      }</div><button class="btn" id="gfw-fetch">FETCH GFW DATA</button>`
    : `<div class="sec-title">GLOBAL FISHING WATCH</div><div class="gfw-badge no">OPTIONAL — NOT ENABLED</div>
       <div class="hint">${
         t.mmsi
           ? 'Add GFW_API_TOKEN to a .env file next to the app to pull registry identity, fishing effort, AIS gaps and IUU status for this MMSI.'
           : 'This contact has no MMSI, so no registry identity can be matched. GFW enrichment is optional and does not affect tracking or alerts.'
       }</div>`;

  body.innerHTML = `
    <h2 class="vname">${esc(labelOf(t))}</h2>
    <div class="vsub">${esc(t.id)}</div>
    <div class="badges">
      ${badges}
      ${t.dark ? '<span class="vbadge dark">DARK CONTACT — NO AIS</span>' : ''}
      ${t.corroborated ? '<span class="vbadge ok">SENSOR-CORROBORATED</span>' : ''}
    </div>
    <table class="kv">${rows.join('')}</table>
    <div style="display:flex;gap:6px;flex-wrap:wrap">
      <button class="btn" id="d-fly">FLY TO</button>
      <button class="btn ghost" id="d-copy">COPY JSON</button>
      ${gfwVesselId ? `<a class="btn ghost" style="text-decoration:none" target="_blank" rel="noopener" href="https://globalfishingwatch.org/map/?vesselId=${encodeURIComponent(gfwVesselId)}">GFW MAP ↗</a>` : ''}
    </div>
    ${gfwHtml}
  `;

  document.getElementById('d-fly').onclick = () =>
    map.flyTo({ center: [t.lon, t.lat], zoom: Math.max(map.getZoom(), 11) });
  document.getElementById('d-copy').onclick = () => {
    navigator.clipboard.writeText(JSON.stringify(t, null, 2));
    banner('Track JSON copied to clipboard');
  };
  const fetchBtn = document.getElementById('gfw-fetch');
  if (fetchBtn) fetchBtn.onclick = () => fetchGfwForTrack(t);
}

document.getElementById('drawer-close').onclick = closeDrawer;

/* --------------------------------------------------------------- zones */

function startDraw() {
  state.drawing = [];
  pushDraw();
  map.getCanvas().style.cursor = 'crosshair';
  map.doubleClickZoom.disable();
  banner('Zone drawing: click to add vertices, double-click to finish.');
}

function finishZone() {
  if (!state.drawing || state.drawing.length < 3) {
    state.drawing = null;
    pushDraw();
    map.getCanvas().style.cursor = '';
    map.doubleClickZoom.enable();
    return;
  }
  const poly = state.drawing.slice();
  // drop near-duplicate points left by the double-click
  const cleaned = [];
  for (const p of poly) {
    const last = cleaned[cleaned.length - 1];
    if (!last || Math.abs(last[0] - p[0]) > 1e-4 || Math.abs(last[1] - p[1]) > 1e-4) cleaned.push(p);
  }
  if (cleaned.length < 3) cleaned.push(...poly.slice(0, 3 - cleaned.length));
  const name = prompt('Zone name:', `Zone ${state.zones.length + 1}`) || `Zone ${state.zones.length + 1}`;
  fetch('/api/zones', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ name, polygon: cleaned, color: '#38bdf8' }),
  })
    .then((r) => r.json())
    .then(() => banner(`Zone "${name}" armed — entries will raise alerts.`))
    .catch((e) => banner('Zone create failed: ' + e.message, 'bad'));
  state.drawing = null;
  pushDraw();
  map.getCanvas().style.cursor = '';
  map.doubleClickZoom.enable();
}

/* ----------------------------------------------------------- gfw panel */

async function queryGfwEvents(bboxOverride = null, days = 14, limit = 250, label = null) {
  const el = document.getElementById('gfw-status');
  let bbox;
  if (bboxOverride) {
    bbox = bboxOverride.map((v) => v.toFixed(4)).join(',');
  } else {
    const b = map.getBounds();
    bbox = [b.getWest(), b.getSouth(), b.getEast(), b.getNorth()].map((v) => v.toFixed(4)).join(',');
  }
  el.textContent = bboxOverride
    ? 'querying the whole globe — this can take about a minute…'
    : 'querying Global Fishing Watch…';
  try {
    const r = await fetch(`/api/gfw/events?bbox=${bbox}&days=${days}&limit=${limit}`);
    const data = await r.json();
    if (!r.ok) throw new Error(data.error || r.statusText);
    state.gfwEvents = data.events || [];
    pushData();
    const scope = label || (bboxOverride ? 'worldwide' : 'for the current view');
    el.textContent = `loaded ${data.count} fishing events (total ${data.total ?? '?'}) ${scope}, last ${days} days — click a dot for the vessel`;
  } catch (e) {
    el.textContent = 'query failed: ' + e.message;
  }
}

async function gfwLookup(queryArg) {
  const q = (queryArg || document.getElementById('gfw-lookup').value || '').trim();
  if (!q) return;
  const isNum = /^\d{6,9}$/.test(q);
  const param = isNum ? `mmsi=${q}` : `name=${encodeURIComponent(q)}`;
  const url = `/api/gfw/vessel?${param}`;
  try {
    let r, data;
    try {
      r = await fetch(url);
      data = await r.json();
    } catch (netErr) {
      await new Promise((res) => setTimeout(res, 1500));
      r = await fetch(url);
      data = await r.json();
    }
    state.selected = null;
    if (r.ok) {
      state.standalone = { title: (data.summary && data.summary.name) || q, gfw: data };
    } else if (r.status === 404) {
      state.standalone = { title: q, gfw: { matched: false, note: data.error } };
    } else {
      throw new Error(data.error || r.statusText);
    }
    renderDrawer();
  } catch (e) {
    banner(`GFW lookup failed: ${e.message}${/fetch/i.test(e.message) ? ' — the app may be restarting, try again shortly.' : ''}`, 'warn');
  }
}

/* ----------------------------------------------------------- ui wiring */

document.querySelectorAll('.tab').forEach((tab) =>
  tab.addEventListener('click', () => {
    document.querySelectorAll('.tab').forEach((t) => t.classList.remove('active'));
    document.querySelectorAll('.panel').forEach((p) => p.classList.remove('active'));
    tab.classList.add('active');
    document.getElementById('panel-' + tab.dataset.tab).classList.add('active');
  })
);

const filterDefs = [
  ['ais', 'AIS', COLORS.ais],
  ['sonar', 'SONAR/RADAR', COLORS.sonar],
  ['lidar', 'LIDAR', COLORS.lidar],
  ['dark', 'DARK', COLORS.dark],
];
document.getElementById('filters').innerHTML = filterDefs
  .map(([k, label]) => `<span class="fchip on" data-f="${k}">${label}</span>`)
  .join('');
document.querySelectorAll('.fchip').forEach((el) =>
  el.addEventListener('click', () => {
    const k = el.dataset.f;
    state.filters[k] = !state.filters[k];
    el.classList.toggle('on', state.filters[k]);
    pushData();
    renderTrackList();
  })
);

document.querySelectorAll('input[data-layer]').forEach((el) =>
  el.addEventListener('change', () => {
    state.layers[el.dataset.layer] = el.checked;
    applyLayerVisibility();
  })
);
document.querySelectorAll('input[data-filter]').forEach((el) =>
  el.addEventListener('change', () => {
    state.filters[el.dataset.filter] = el.checked;
    pushData();
    renderTrackList();
  })
);

document.getElementById('search').addEventListener('input', (e) => {
  state.search = e.target.value;
  pushData();
  renderTrackList();
});

// basemap switcher (satellite / dark / streets), remembered across reloads
const basemapRow = document.getElementById('basemap-row');
if (basemapRow) {
  basemapRow.innerHTML = Object.entries(BASEMAPS)
    .map(
      ([k, b]) =>
        `<span class="fchip ${k === activeBasemap ? 'on' : ''}" data-b="${k}">${b.label}</span>`
    )
    .join('');
  basemapRow.querySelectorAll('.fchip').forEach((el) =>
    el.addEventListener('click', () => setBasemap(el.dataset.b))
  );
}

document.getElementById('clear-alerts').onclick = () => {
  state.alerts = [];
  renderAlerts();
};

document.getElementById('draw-zone').onclick = startDraw;
document.getElementById('clear-zones').onclick = async () => {
  for (const z of [...state.zones]) {
    await fetch(`/api/zones?id=${encodeURIComponent(z.id)}`, { method: 'DELETE' });
  }
};
document.getElementById('gfw-query').onclick = () => queryGfwEvents();
document.getElementById('gfw-query-global').onclick = () =>
  queryGfwEvents([-180, -70, 180, 70], 7, 500, 'worldwide');
document.getElementById('gfw-clear').onclick = () => {
  state.gfwEvents = [];
  pushData();
  document.getElementById('gfw-status').textContent = 'cleared';
};
document.getElementById('gfw-lookup-btn').onclick = () => gfwLookup();
document.getElementById('gfw-lookup').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') gfwLookup();
});

document.getElementById('export-geojson').onclick = () => {
  const fc = tracksFC();
  const blob = new Blob([JSON.stringify(fc, null, 2)], { type: 'application/geo+json' });
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = `oceansentinel-tracks-${new Date().toISOString().slice(0, 19).replace(/[:T]/g, '-')}.geojson`;
  a.click();
  URL.revokeObjectURL(a.href);
};

setInterval(() => {
  document.getElementById('clock').textContent = new Date().toISOString().slice(11, 19) + 'Z';
}, 1000);

// handles for in-page diagnostics (used by tests and by hand while debugging)
window.__map = map;
window.__osState = state;

connect();
