'use strict';

/* Project Icarus — live air picture. Data arrives over /api/icarus/ws as:
     {"type":"state", aircraft, alerts, watchlist, feeds, stats, app}
     {"type":"alert", alert}          {"type":"watch"}
   Aircraft carry the fields the server normalized (hex, callsign, alt_ft,
   gs_kt, track_deg, squawk, flags, shape, age_s, ...). */

const BAND = {
  ground: '#64748b',
  low: '#fbbf24',
  mid: '#4ade80',
  high: '#22d3ee',
  ultra: '#c084fc',
};
const SHAPES = ['air', 'heli', 'glider', 'balloon', 'ground'];

/// MapLibre 4 refuses a <canvas> element as an image source once the map is
/// rendering ("mismatched image size"): a canvas carries no pixel buffer, so
/// the style's resize path sees a zero-length image. Hand it real pixels.
function iconData(canvas) {
  return canvas.getContext('2d').getImageData(0, 0, canvas.width, canvas.height);
}
// Reference altitude bands for the legend and the drawer tape (feet).
const BAND_EDGES = [0, 5000, 20000, 35000];

const RING = {
  emergency: '#ef4444',
  watchlist: '#f59e0b',
  military: '#e2e8f0',
  lost: '#94a3b8',
};

const SQUAWK_MEANING = {
  '7500': 'unlawful interference (hijack)',
  '7600': 'radio failure',
  '7700': 'general emergency',
  '1200': 'VFR, United States',
  '2000': 'oceanic / no code assigned',
  '7000': 'VFR, Europe',
  '7010': 'VFR, United Kingdom',
};

const state = {
  aircraft: new Map(),
  alerts: [],
  watchlist: [],
  feeds: [],
  stats: {},
  app: {},
  circles: [],
  surface: [],
  selected: null,
  selectedSurface: null,
  search: '',
  listFilter: 'all',
  sortBy: 'priority',
  detections: { emergency: true, military: true, watchlist: true, lost: true },
  flightState: { airborne: true, ground: true },
  layers: {
    icons: true,
    labels: true,
    trails: true,
    rings: true,
    circles: true,
    surface: true,
    night: true,
    meridians: true,
  },
  wsOk: false,
  firstState: true,
  userMoved: false,
  hidden: 0,
  receivedAt: Date.now(),
};

/* ---------------------------------------------------------------- helpers */

function apiToken() {
  try {
    return localStorage.getItem('os.token') || '';
  } catch (e) {
    return '';
  }
}

function promptToken() {
  const t = window.prompt('This server requires an API token:', apiToken());
  if (t !== null) {
    try {
      localStorage.setItem('os.token', t.trim());
    } catch (e) {
      /* ignore */
    }
    location.reload();
  }
}

async function api(path, opts) {
  const headers = Object.assign({}, (opts && opts.headers) || {});
  const t = apiToken();
  if (t) headers['authorization'] = 'Bearer ' + t;
  const r = await fetch(path, Object.assign({}, opts || {}, { headers }));
  if (r.status === 401) {
    banner('This server requires an API token.', 'warn');
    promptToken();
    throw new Error('unauthorized');
  }
  return r;
}

const esc = (s) =>
  String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

const num = (v, d = 0) => (v === null || v === undefined || Number.isNaN(Number(v)) ? '—' : Number(v).toFixed(d));

function ageStr(sec) {
  if (sec === null || sec === undefined || Number.isNaN(Number(sec))) return '—';
  const s = Math.max(0, Number(sec));
  if (s < 60) return `${Math.round(s)}s`;
  if (s < 3600) return `${Math.round(s / 60)}m`;
  return `${(s / 3600).toFixed(1)}h`;
}

/// Flight level from a barometric altitude ("FL297").
function flightLevel(alt) {
  if (alt === null || alt === undefined) return '—';
  return 'FL' + String(Math.round(Number(alt) / 100)).padStart(3, '0');
}

const nf = (v, d = 0) => (v === null || v === undefined || Number.isNaN(Number(v)) ? '—' : Number(v).toLocaleString('en-US', { maximumFractionDigits: d }));

function banner(msg, kind = '') {
  const el = document.getElementById('banner');
  el.textContent = msg;
  el.className = kind;
  clearTimeout(banner._t);
  if (msg) banner._t = setTimeout(() => el.classList.add('hidden'), 6500);
  else el.classList.add('hidden');
}

function chip(id, text, kind = '', title = '') {
  const el = document.getElementById(id);
  if (!el) return;
  el.textContent = text;
  el.className = 'chip ' + kind;
  el.title = title || '';
}

/* -------------------------------------------------------------------- map */

const BASEMAPS = window.OS_BASEMAPS;
let activeBasemap = (() => {
  try {
    const saved = localStorage.getItem('os.basemap');
    if (saved && BASEMAPS[saved]) return saved;
  } catch (e) {
    /* ignore */
  }
  return 'dark';
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
    glyphs: 'https://fonts.openmaptiles.org/{fontstack}/{range}.pbf',
    sources: baseSources,
    layers: [{ id: 'bg', type: 'background', paint: { 'background-color': '#04070d' } }, ...baseLayers],
  },
  center: [-0.45, 51.47],
  zoom: 8,
  attributionControl: { compact: true },
});

map.on('dragstart', () => (state.userMoved = true));
map.on('zoomstart', () => (state.userMoved = true));

map.on('error', (e) => {
  const msg = (e && e.error && e.error.message) || '';
  if (/tile|fetch|Failed to load|NetworkError/i.test(msg)) {
    banner('Basemap tiles unreachable — switch basemap in the LAYERS tab; aircraft data keeps flowing.', 'warn');
  }
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
  if (row) row.querySelectorAll('.fchip').forEach((el) => el.classList.toggle('on', el.dataset.b === key));
}

/// Aircraft silhouettes, drawn nose-up so the layer can rotate them by track.
function planeIcon(color, shape, size = 36) {
  const c = document.createElement('canvas');
  c.width = size;
  c.height = size;
  const ctx = c.getContext('2d');
  const s = size;
  const k = s / 36;
  ctx.lineJoin = 'round';
  ctx.fillStyle = color;
  ctx.strokeStyle = 'rgba(3,7,12,.9)';
  ctx.lineWidth = 1.1 * k;
  if (shape === 'heli') {
    ctx.beginPath();
    ctx.moveTo(s * 0.5, s * 0.28);
    ctx.lineTo(s * 0.61, s * 0.5);
    ctx.lineTo(s * 0.57, s * 0.66);
    ctx.lineTo(s * 0.43, s * 0.66);
    ctx.lineTo(s * 0.39, s * 0.5);
    ctx.closePath();
    ctx.fill();
    ctx.stroke();
    ctx.beginPath();
    ctx.moveTo(s * 0.46, s * 0.64);
    ctx.lineTo(s * 0.34, s * 0.82);
    ctx.lineTo(s * 0.43, s * 0.86);
    ctx.lineTo(s * 0.54, s * 0.66);
    ctx.closePath();
    ctx.fill();
    ctx.stroke();
    ctx.strokeStyle = color;
    ctx.lineWidth = 1.7 * k;
    ctx.beginPath();
    ctx.moveTo(s * 0.15, s * 0.34);
    ctx.lineTo(s * 0.85, s * 0.34);
    ctx.stroke();
    ctx.beginPath();
    ctx.arc(s * 0.36, s * 0.85, s * 0.08, 0, Math.PI * 2);
    ctx.stroke();
  } else if (shape === 'glider') {
    ctx.beginPath();
    ctx.moveTo(s * 0.5, s * 0.06);
    ctx.lineTo(s * 0.56, s * 0.42);
    ctx.lineTo(s * 0.97, s * 0.52);
    ctx.lineTo(s * 0.56, s * 0.6);
    ctx.lineTo(s * 0.54, s * 0.94);
    ctx.lineTo(s * 0.46, s * 0.94);
    ctx.lineTo(s * 0.44, s * 0.6);
    ctx.lineTo(s * 0.03, s * 0.52);
    ctx.lineTo(s * 0.44, s * 0.42);
    ctx.closePath();
    ctx.fill();
    ctx.stroke();
  } else if (shape === 'balloon') {
    ctx.beginPath();
    ctx.arc(s * 0.5, s * 0.38, s * 0.27, 0, Math.PI * 2);
    ctx.fill();
    ctx.stroke();
    ctx.beginPath();
    ctx.rect(s * 0.42, s * 0.68, s * 0.16, s * 0.16);
    ctx.fill();
    ctx.stroke();
  } else if (shape === 'ground') {
    ctx.beginPath();
    ctx.moveTo(s * 0.5, s * 0.22);
    ctx.lineTo(s * 0.8, s * 0.78);
    ctx.lineTo(s * 0.2, s * 0.78);
    ctx.closePath();
    ctx.fill();
    ctx.stroke();
  } else {
    ctx.beginPath();
    ctx.moveTo(s * 0.5, s * 0.05);
    ctx.lineTo(s * 0.56, s * 0.34);
    ctx.lineTo(s * 0.95, s * 0.55);
    ctx.lineTo(s * 0.95, s * 0.63);
    ctx.lineTo(s * 0.57, s * 0.55);
    ctx.lineTo(s * 0.57, s * 0.78);
    ctx.lineTo(s * 0.78, s * 0.9);
    ctx.lineTo(s * 0.78, s * 0.96);
    ctx.lineTo(s * 0.5, s * 0.9);
    ctx.lineTo(s * 0.22, s * 0.96);
    ctx.lineTo(s * 0.22, s * 0.9);
    ctx.lineTo(s * 0.43, s * 0.78);
    ctx.lineTo(s * 0.43, s * 0.55);
    ctx.lineTo(s * 0.05, s * 0.63);
    ctx.lineTo(s * 0.05, s * 0.55);
    ctx.lineTo(s * 0.44, s * 0.34);
    ctx.closePath();
    ctx.fill();
    ctx.stroke();
  }
  return c;
}

/// Small hull glyph for the naval surface picture, drawn pointing up so the
/// layer can rotate it by course.
function shipGlyph(color, size = 30) {
  const c = document.createElement('canvas');
  c.width = size;
  c.height = size;
  const ctx = c.getContext('2d');
  const s = size;
  ctx.beginPath();
  ctx.moveTo(s * 0.5, 2);
  ctx.lineTo(s * 0.72, s * 0.3);
  ctx.lineTo(s * 0.68, s - 3);
  ctx.lineTo(s * 0.32, s - 3);
  ctx.lineTo(s * 0.28, s * 0.3);
  ctx.closePath();
  ctx.fillStyle = color;
  ctx.fill();
  ctx.lineWidth = 1.4;
  ctx.strokeStyle = 'rgba(3,7,12,.9)';
  ctx.stroke();
  ctx.fillRect(s * 0.42, s * 0.44, s * 0.16, s * 0.18);
  ctx.strokeRect(s * 0.42, s * 0.44, s * 0.16, s * 0.18);
  return c;
}

function iconExpr() {
  const expr = ['match', ['get', 'shape']];
  for (const shape of SHAPES) {
    const inner = ['match', ['get', 'band']];
    for (const band of Object.keys(BAND)) inner.push(band, `ac-${band}-${shape}`);
    inner.push(`ac-ground-${shape}`);
    expr.push(shape, inner);
  }
  expr.push('ac-ground-air');
  return expr;
}

const bandColorExpr = (() => {
  const e = ['match', ['get', 'band']];
  for (const [b, c] of Object.entries(BAND)) e.push(b, c);
  e.push(BAND.ground);
  return e;
})();

/// Lat/lon grid, drawn locally so the map has structure even with no tiles.
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

const EMPTY_FC = { type: 'FeatureCollection', features: [] };

function setupAirLayers() {
  for (const [band, color] of Object.entries(BAND)) {
    for (const shape of SHAPES) {
      map.addImage(`ac-${band}-${shape}`, iconData(planeIcon(color, shape)), { pixelRatio: 2 });
    }
  }

  for (const id of ['graticule', 'circles', 'trails', 'surface']) {
    map.addSource(id, { type: 'geojson', data: EMPTY_FC });
  }
  map.addImage('surface-carrier', iconData(shipGlyph('#fbbf24')), { pixelRatio: 2 });
  map.addImage('surface-naval', iconData(shipGlyph('#93c5fd')), { pixelRatio: 2 });
  // Clustered so a country-sized view does not paint thousands of icons; the
  // aircraft move fast, so the cluster radius is smaller than the ship map's.
  map.addSource('aircraft', {
    type: 'geojson',
    data: EMPTY_FC,
    cluster: true,
    clusterRadius: 38,
    clusterMaxZoom: 6,
  });

  map.addLayer({
    id: 'graticule',
    type: 'line',
    source: 'graticule',
    paint: {
      'line-color': ['case', ['==', ['get', 'major'], 1], 'rgba(148,163,184,0.3)', 'rgba(100,116,139,0.14)'],
      'line-width': ['case', ['==', ['get', 'major'], 1], 1, 0.6],
    },
  });
  updateGraticule();
  map.on('moveend', updateGraticule);

  // The circles actually being queried upstream: this is the receiver-network
  // coverage the picture is built from, which makes the gaps self-explanatory.
  map.addLayer({
    id: 'circles-fill',
    type: 'fill',
    source: 'circles',
    paint: { 'fill-color': '#f59e0b', 'fill-opacity': 0.025 },
  });
  map.addLayer({
    id: 'circles-line',
    type: 'line',
    source: 'circles',
    paint: {
      'line-color': 'rgba(245,158,11,.45)',
      'line-width': 1,
      'line-dasharray': [3, 4],
    },
  });

  // Naval surface picture: the carriers and warships the maritime map is
  // tracking, with a ring showing roughly where their air operations happen.
  map.addLayer({
    id: 'surface-ops',
    type: 'fill',
    source: 'surface',
    filter: ['==', ['geometry-type'], 'Polygon'],
    paint: { 'fill-color': '#fbbf24', 'fill-opacity': 0.06 },
  });
  map.addLayer({
    id: 'surface-ops-line',
    type: 'line',
    source: 'surface',
    filter: ['==', ['geometry-type'], 'Polygon'],
    paint: {
      'line-color': 'rgba(251,191,36,.5)',
      'line-width': 1,
      'line-dasharray': [3, 3],
    },
  });

  map.addLayer({
    id: 'trails-line',
    type: 'line',
    source: 'trails',
    layout: { 'line-cap': 'round', 'line-join': 'round' },
    paint: {
      'line-color': ['get', 'color'],
      'line-width': ['interpolate', ['linear'], ['zoom'], 3, 0.9, 9, 1.8],
      'line-opacity': 0.5,
    },
  });

  map.addLayer({
    id: 'clusters',
    type: 'circle',
    source: 'aircraft',
    filter: ['has', 'point_count'],
    paint: {
      'circle-color': ['step', ['get', 'point_count'], 'rgba(245,158,11,.2)', 25, 'rgba(251,191,36,.26)', 200, 'rgba(252,211,77,.3)'],
      'circle-radius': ['step', ['get', 'point_count'], 14, 25, 20, 200, 28],
      'circle-stroke-color': 'rgba(245,158,11,.7)',
      'circle-stroke-width': 1,
    },
  });
  map.addLayer({
    id: 'cluster-count',
    type: 'symbol',
    source: 'aircraft',
    filter: ['has', 'point_count'],
    layout: {
      'text-field': '{point_count_abbreviated}',
      'text-font': ['Open Sans Regular'],
      'text-size': 11,
      'text-allow-overlap': true,
    },
    paint: { 'text-color': '#fdf3e3' },
  });

  // Detection rings: one layer, colour chosen by the most serious flag.
  map.addLayer({
    id: 'ac-rings',
    type: 'circle',
    source: 'aircraft',
    filter: ['!', ['has', 'point_count']],
    paint: {
      'circle-radius': ['interpolate', ['linear'], ['zoom'], 3, 6, 8, 13, 12, 22],
      'circle-color': 'rgba(0,0,0,0)',
      'circle-stroke-width': 1.6,
      'circle-stroke-color': [
        'case',
        ['in', 'emergency', ['get', 'flags']], RING.emergency,
        ['in', 'watchlist', ['get', 'flags']], RING.watchlist,
        ['in', 'military', ['get', 'flags']], RING.military,
        ['in', 'lost', ['get', 'flags']], RING.lost,
        'rgba(0,0,0,0)',
      ],
    },
  });

  map.addLayer({
    id: 'surface-icons',
    type: 'symbol',
    source: 'surface',
    filter: ['==', ['geometry-type'], 'Point'],
    layout: {
      'icon-image': ['case', ['get', 'carrier'], 'surface-carrier', 'surface-naval'],
      'icon-size': ['interpolate', ['linear'], ['zoom'], 2, 0.7, 8, 1.0, 12, 1.3],
      'icon-rotate': ['coalesce', ['get', 'cog'], 0],
      'icon-rotation-alignment': 'map',
      'icon-allow-overlap': true,
      'icon-ignore-placement': true,
    },
  });
  map.addLayer({
    id: 'surface-labels',
    type: 'symbol',
    source: 'surface',
    filter: ['==', ['geometry-type'], 'Point'],
    minzoom: 5,
    layout: {
      'text-font': ['Open Sans Regular'],
      'text-field': ['get', 'label'],
      'text-size': 10,
      'text-offset': [0, 1.6],
      'text-anchor': 'top',
      'text-optional': true,
    },
    paint: {
      'text-color': ['case', ['get', 'carrier'], '#fde68a', '#cbd5e1'],
      'text-halo-color': '#03060b',
      'text-halo-width': 1.6,
    },
  });

  map.addLayer({
    id: 'sel-ring',
    type: 'circle',
    source: 'aircraft',
    filter: ['==', ['get', 'id'], ''],
    paint: {
      'circle-radius': 18,
      'circle-color': 'rgba(0,0,0,0)',
      'circle-stroke-color': '#fbbf24',
      'circle-stroke-width': 2,
    },
  });

  map.addLayer({
    id: 'ac-icons',
    type: 'symbol',
    source: 'aircraft',
    filter: ['!', ['has', 'point_count']],
    layout: {
      'icon-image': iconExpr(),
      'icon-size': ['interpolate', ['linear'], ['zoom'], 2, 0.4, 7, 0.6, 12, 0.92],
      'icon-rotate': ['coalesce', ['get', 'track_deg'], 0],
      'icon-rotation-alignment': 'map',
      'icon-allow-overlap': true,
      'icon-ignore-placement': true,
    },
    paint: { 'icon-opacity': ['case', ['in', 'lost', ['get', 'flags']], 0.55, 0.96] },
  });

  map.addLayer({
    id: 'ac-labels',
    type: 'symbol',
    source: 'aircraft',
    filter: ['!', ['has', 'point_count']],
    minzoom: 6.5,
    layout: {
      'text-font': ['Open Sans Regular'],
      'text-field': ['get', 'label'],
      'text-size': 10,
      'text-offset': [0, 1.4],
      'text-anchor': 'top',
      'text-optional': true,
      'text-allow-overlap': false,
      'symbol-sort-key': ['get', 'priority'],
    },
    paint: {
      'text-color': ['case', ['in', 'emergency', ['get', 'flags']], '#fca5a5', '#e8eef6'],
      'text-halo-color': '#03060b',
      'text-halo-width': 1.6,
    },
  });

  // Day/night marking and UTC hour meridians: their own stack, inserted right
  // above the basemap so they show over every basemap style without dimming
  // the aircraft drawn on top.
  if (window.OSDaylight) OSDaylight.attach(map, { before: 'graticule' });

  applyLayerVisibility();
  pushData();
}

/// A failure here used to be invisible: the map rendered the basemap with no
/// aircraft at all and nothing said why. Fail loudly instead.
function setupAirLayersSafe() {
  try {
    setupAirLayers();
  } catch (e) {
    window.__osLayerError = String((e && e.message) || e);
    banner('Aircraft layers failed to initialise: ' + window.__osLayerError, 'bad');
    throw e;
  }
}

if (map.loaded()) {
  setupAirLayersSafe();
} else {
  map.on('load', setupAirLayersSafe);
}

map.on('mousemove', (e) => {
  let when = '';
  if (window.OSDaylight) {
    const s = OSDaylight.localSolar(new Date(), e.lngLat.lng);
    when = ` · solar ${s.time} · ${s.zone}`;
  }
  document.getElementById('cursor-readout').textContent =
    `${e.lngLat.lat.toFixed(5)}, ${e.lngLat.lng.toFixed(5)}${when}`;
});

map.on('click', (e) => {
  const f = map.queryRenderedFeatures(e.point, { layers: ['ac-icons'] })[0];
  if (f && f.properties && f.properties.id) selectAircraft(f.properties.id);
});

map.on('click', 'clusters', (e) => {
  const f = map.queryRenderedFeatures(e.point, { layers: ['clusters'] })[0];
  if (!f) return;
  map
    .getSource('aircraft')
    .getClusterExpansionZoom(f.properties.cluster_id)
    .then((zoom) => map.easeTo({ center: f.geometry.coordinates, zoom: zoom + 0.2 }));
});
map.on('click', 'surface-icons', (e) => {
  const f = e.features && e.features[0] && e.features[0].properties;
  if (f && f.id) selectSurface(f.id);
});
map.on('mouseenter', 'surface-icons', () => (map.getCanvas().style.cursor = 'pointer'));
map.on('mouseleave', 'surface-icons', () => (map.getCanvas().style.cursor = ''));
map.on('mouseenter', 'ac-icons', () => (map.getCanvas().style.cursor = 'pointer'));
map.on('mouseleave', 'ac-icons', () => (map.getCanvas().style.cursor = ''));
map.on('mouseenter', 'clusters', () => (map.getCanvas().style.cursor = 'pointer'));
map.on('mouseleave', 'clusters', () => (map.getCanvas().style.cursor = ''));
map.on('moveend', () => {
  sendViewport(state.ws);
  if (document.getElementById('in-view').checked) renderList();
});

/* --------------------------------------------------------- data -> map */

function bandOf(a) {
  if (a.on_ground) return 'ground';
  // unknown altitude is drawn neutral rather than guessed into a band
  if (a.alt_ft === null || a.alt_ft === undefined) return 'ground';
  const alt = Number(a.alt_ft);
  if (alt < 5000) return 'low';
  if (alt < 20000) return 'mid';
  if (alt < 35000) return 'high';
  return 'ultra';
}

const hasFlag = (a, f) => (a.flags || []).indexOf(f) !== -1;
const isEmergency = (a) => hasFlag(a, 'emergency') || !!(a.emergency && a.emergency.length);

function labelOf(a) {
  return a.callsign || a.registration || (a.hex || '').toUpperCase();
}

function priorityOf(a) {
  let r = 0;
  if (isEmergency(a)) r += 100;
  if (hasFlag(a, 'watchlist')) r += 50;
  if (hasFlag(a, 'lost')) r += 25;
  if (hasFlag(a, 'military')) r += 10;
  return r;
}

function visible(a) {
  // each detection switch hides exactly the aircraft carrying that flag
  if (isEmergency(a) && !state.detections.emergency) return false;
  if (hasFlag(a, 'military') && !state.detections.military) return false;
  if (hasFlag(a, 'watchlist') && !state.detections.watchlist) return false;
  if (hasFlag(a, 'lost') && !state.detections.lost) return false;
  if (a.on_ground ? !state.flightState.ground : !state.flightState.airborne) return false;
  const f = state.listFilter;
  if (f === 'emergency' && !isEmergency(a)) return false;
  if (f === 'military' && !hasFlag(a, 'military')) return false;
  if (f === 'watchlist' && !hasFlag(a, 'watchlist')) return false;
  if (f === 'lost' && !hasFlag(a, 'lost')) return false;
  if (f === 'airborne' && a.on_ground) return false;
  if (f === 'ground' && !a.on_ground) return false;
  if (state.search && !matchSearch(a)) return false;
  return true;
}

function matchSearch(a) {
  const q = state.search.toLowerCase();
  return [a.callsign, a.registration, a.hex, a.type_code, a.country, a.airline, a.operator]
    .filter(Boolean)
    .some((v) => String(v).toLowerCase().includes(q));
}

function destination(lat, lon, brg, distM) {
  const R = 6371008.8;
  const d = distM / R;
  const b = (brg * Math.PI) / 180;
  const p1 = (lat * Math.PI) / 180;
  const l1 = (lon * Math.PI) / 180;
  const p2 = Math.asin(Math.sin(p1) * Math.cos(d) + Math.cos(p1) * Math.sin(d) * Math.cos(b));
  const l2 =
    l1 + Math.atan2(Math.sin(b) * Math.sin(d) * Math.cos(p1), Math.cos(d) - Math.sin(p1) * Math.sin(p2));
  // Named results: building this tuple inline invited a paren-counting bug
  // that silently collapsed the pair into a single value.
  const lonOut = (((l2 * 180) / Math.PI + 540) % 360) - 180;
  const latOut = (p2 * 180) / Math.PI;
  return [lonOut, latOut];
}

/// Dead-reckon between polls so traffic glides instead of stepping. The
/// server's `age_s` is the reference, so a skewed browser clock cannot warp it.
function positionOf(a) {
  const base = [a.lon, a.lat];
  if (a.lost || a.on_ground) return base;
  if (a.gs_kt === null || a.gs_kt === undefined || a.track_deg === null || a.track_deg === undefined) return base;
  const dt = Math.min((a.age_s || 0) + (Date.now() - state.receivedAt) / 1000, 90);
  if (dt <= 0) return base;
  const [lon, lat] = destination(a.lat, a.lon, a.track_deg, a.gs_kt * 0.514444 * dt);
  // a position the map cannot use is worse than a stale one
  if (!Number.isFinite(lon) || !Number.isFinite(lat)) return base;
  return [lon, lat];
}

function aircraftFC() {
  const zoom = map.getZoom();
  const feats = [];
  for (const a of state.aircraft.values()) {
    if (!visible(a)) continue;
    const [lon, lat] = positionOf(a);
    feats.push({
      type: 'Feature',
      geometry: { type: 'Point', coordinates: [lon, lat] },
      properties: {
        id: a.hex,
        label: labelOf(a),
        shape: a.shape || 'air',
        band: bandOf(a),
        flags: a.flags || [],
        priority: priorityOf(a),
        track_deg: a.track_deg ?? 0,
        alt: a.alt_ft ?? null,
      },
    });
  }
  return { type: 'FeatureCollection', features: feats };
}

function trailsFC() {
  const zoom = map.getZoom();
  if (zoom < 4.5) return EMPTY_FC;
  const crowded = state.aircraft.size > 400;
  const feats = [];
  for (const a of state.aircraft.values()) {
    if (!visible(a)) continue;
    const trail = a.trail || [];
    if (trail.length < 2) continue;
    const pts = crowded ? trail.slice(-12) : trail;
    feats.push({
      type: 'Feature',
      geometry: { type: 'LineString', coordinates: pts.map((p) => [p.lon, p.lat]) },
      properties: { color: BAND[bandOf(a)] || BAND.ground },
    });
  }
  return { type: 'FeatureCollection', features: feats };
}

function circlesFC() {
  const feats = [];
  for (const c of state.circles || []) {
    const rKm = (c.radius_nm || 0) * 1.852;
    const pts = [];
    for (let i = 0; i <= 64; i++) {
      pts.push(destination(c.lat, c.lon, (i * 360) / 64, rKm * 1000));
    }
    feats.push({
      type: 'Feature',
      geometry: { type: 'Polygon', coordinates: [pts] },
      properties: {},
    });
  }
  return { type: 'FeatureCollection', features: feats };
}

/// Naval contacts (points) plus a ring for each carrier, which is the inner
/// air-control area an air wing works inside — an approximation, labelled as
/// such in the legend.
function surfaceFC() {
  const feats = [];
  for (const c of state.surface || []) {
    feats.push({
      type: 'Feature',
      geometry: { type: 'Point', coordinates: [c.lon, c.lat] },
      properties: {
        id: c.id,
        label: (c.carrier ? '★ ' : '') + (c.name || (c.mmsi ? 'MMSI ' + c.mmsi : c.id)),
        carrier: !!c.carrier,
        naval: !!c.naval,
        cog: c.cog ?? 0,
        stale: (c.age_s || 0) > 1800,
      },
    });
    if (c.carrier) {
      const pts = [];
      for (let i = 0; i <= 72; i++) {
        pts.push(destination(c.lat, c.lon, (i * 360) / 72, 50 * 1852));
      }
      feats.push({ type: 'Feature', geometry: { type: 'Polygon', coordinates: [pts] }, properties: {} });
    }
  }
  return { type: 'FeatureCollection', features: feats };
}

function applyLayerVisibility() {
  const v = (id, on) => {
    if (map.getLayer(id)) map.setLayoutProperty(id, 'visibility', on ? 'visible' : 'none');
  };
  v('ac-icons', state.layers.icons);
  const labelsUseful = state.aircraft.size <= 600 || map.getZoom() >= 8;
  v('ac-labels', state.layers.labels && labelsUseful);
  v('trails-line', state.layers.trails);
  v('ac-rings', state.layers.rings);
  v('circles-fill', state.layers.circles);
  v('circles-line', state.layers.circles);
  v('surface-ops', state.layers.surface);
  v('surface-ops-line', state.layers.surface);
  v('surface-icons', state.layers.surface);
  v('surface-labels', state.layers.surface);
  // day/night marking owns its own layers (daylight.js)
  if (window.OSDaylight) {
    OSDaylight.setVisible({ night: state.layers.night, meridians: state.layers.meridians });
  }
}

function pushData() {
  const set = (id, data) => {
    const src = map.getSource(id);
    if (src) src.setData(data);
  };
  set('aircraft', aircraftFC());
  set('trails', trailsFC());
  set('circles', circlesFC());
  set('surface', surfaceFC());
}

/// Cheap 1 Hz refresh of positions only (trails change slowly).
let tickCount = 0;
function pushPositions() {
  const src = map.getSource('aircraft');
  if (src) src.setData(aircraftFC());
  tickCount++;
  if (tickCount % 3 === 0) {
    const t = map.getSource('trails');
    if (t) t.setData(trailsFC());
  }
}
setInterval(() => {
  if (state.aircraft.size) pushPositions();
}, 1000);

/* ------------------------------------------------------------------- ws */

function sendViewport(ws) {
  if (!ws || ws.readyState !== WebSocket.OPEN) return;
  const b = map.getBounds();
  ws.send(
    JSON.stringify({
      type: 'viewport',
      bounds: { w: b.getWest(), s: b.getSouth(), e: b.getEast(), n: b.getNorth() },
    })
  );
}

function connect() {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const token = apiToken();
  const url = `${proto}://${location.host}/api/icarus/ws${token ? '?token=' + encodeURIComponent(token) : ''}`;
  const ws = new WebSocket(url);
  state.ws = ws;
  ws.onopen = () => {
    state.wsOk = true;
    chip('chip-ws', 'WS LIVE', 'ok');
    sendViewport(ws);
    pageProgress(0.25);
  };
  ws.onclose = () => {
    state.wsOk = false;
    chip('chip-ws', 'WS DOWN', 'bad');
    const pp = document.getElementById('page-progress');
    if (pp) {
      pp.classList.remove('hidden');
      pageProgress(0.05);
    }
    setTimeout(connect, 2000);
  };
  ws.onerror = () => ws.close();
  ws.onmessage = (ev) => {
    let msg;
    try {
      msg = JSON.parse(ev.data);
    } catch (e) {
      return;
    }
    if (msg.type === 'state') applyState(msg);
    else if (msg.type === 'alert') pushAlert(msg.alert);
  };
}

function applyState(msg) {
  bootFirstState = true;
  const pp = document.getElementById('page-progress');
  if (pp && !pp.classList.contains('hidden')) {
    pageProgress(1);
    setTimeout(() => {
      const p = document.getElementById('page-progress');
      if (p) p.classList.add('hidden');
    }, 600);
  }
  state.receivedAt = Date.now();
  state.aircraft = new Map((msg.aircraft || []).map((a) => [a.hex, a]));
  state.alerts = msg.alerts || [];
  state.watchlist = msg.watchlist || [];
  state.feeds = msg.feeds || [];
  state.stats = msg.stats || {};
  state.app = msg.app || {};
  state.circles = (msg.app && msg.app.query_circles) || msg.query_circles || [];
  state.surface = msg.surface || [];
  state.hidden = msg.hidden_aircraft || 0;

  if (state.firstState && state.app.home && !state.userMoved) {
    state.firstState = false;
    let wanted = null;
    try {
      wanted = new URLSearchParams(location.search).get('surface');
    } catch (e) {
      /* ignore */
    }
    const hit = wanted && (state.surface || []).find((c) => c.id === wanted);
    if (hit) {
      map.jumpTo({ center: [hit.lon, hit.lat], zoom: 7 });
      selectSurface(hit.id);
    } else {
      map.jumpTo({
        center: [state.app.home.lon, state.app.home.lat],
        zoom: state.app.home.zoom || 8,
      });
    }
  }

  pushData();
  renderList();
  renderAlerts();
  renderWatch();
  renderFeeds();
  renderStats();
  renderChips();
  renderFeedDetail();
  if (state.selected) renderDrawer();
}

function pushAlert(a) {
  const idx = state.alerts.findIndex((x) => x.id === a.id);
  if (idx === -1) state.alerts.unshift(a);
  renderAlerts();
  if (a.kind === 'aircraft_emergency') banner(a.message, 'bad');
  else if (a.kind === 'aircraft_watchlist') banner(a.message, 'bad');
  else if (a.kind === 'aircraft_lost') banner(a.message, 'warn');
}

/* --------------------------------------------------------------- panels */

function renderChips() {
  const app = state.app || {};
  const ver = document.getElementById('ver');
  if (ver) ver.textContent = app.version ? 'v' + app.version : '';

  const feeds = state.feeds || [];
  const f = feeds[0];
  if (f && f.state === 'throttled') {
    // not an outage: the client is staying inside the feed's request budget
    chip('chip-feed', 'RATE LIMITED · RESTING', 'warn', f.detail || '');
  } else if (f && f.state === 'error') {
    chip('chip-feed', `${String(f.name).toUpperCase()} ERROR`, 'bad', f.detail || '');
  } else if (f && f.state === 'ok') {
    chip('chip-feed', `${String(f.name).toUpperCase()} LIVE`, 'ok', f.detail || '');
  } else {
    chip('chip-feed', 'FEED WAITING', 'warn', (f && f.detail) || '');
  }

  const mode = app.mode === 'global' ? 'GLOBAL VIEW · MIL SWEEP' : `REGIONAL · ${app.circles || 0} CIRCLES`;
  chip(
    'chip-mode',
    app.degraded ? mode + ' · COARSE' : mode,
    app.mode === 'global' ? 'warn' : 'ok',
    app.mode === 'global'
      ? 'Zoomed out: the regional receiver query cannot cover this view, so the worldwide military sweep carries it. Zoom in for all civil traffic.'
      : `Querying ${app.circles || 0} circles of ${app.radius_nm} nm around what you are looking at.`
  );
}

function renderFeedDetail() {
  const el = document.getElementById('feed-detail');
  if (!el) return;
  const app = state.app || {};
  const st = state.stats || {};
  const parts = [];
  parts.push(
    `<b>${esc(app.provider || 'feed')}</b> · ${esc(app.mode || '—')} mode · ${app.circles || 0} circles × ${app.radius_nm || 0} nm`
  );
  parts.push(`${st.aircraft || 0} aircraft tracked, ${st.updated || 0} in the last sweep`);
  parts.push(`${(st.queries || 0).toLocaleString('en-US')} upstream queries since start`);
  if (app.opensky) parts.push('OpenSky global provider: enabled');
  if (app.global_mil) parts.push('global military sweep: on');
  parts.push(
    `lost contact after ${app.lost_after_s || 300}s of silence — stretched automatically when the sweep is slow`
  );
  if (app.recording) parts.push('aircraft archive: recording to data/icarus/history');
  el.innerHTML = parts.join('<br />');
}

const filterDefs = [
  ['all', 'ALL'],
  ['airborne', 'AIRBORNE'],
  ['ground', 'GROUND'],
  ['emergency', 'EMERGENCY'],
  ['military', 'MILITARY'],
  ['watchlist', 'WATCHED'],
  ['lost', 'LOST CONTACT'],
];

function renderFilters() {
  const row = document.getElementById('air-filters');
  row.innerHTML = filterDefs
    .map(([k, label]) => `<span class="fchip${state.listFilter === k ? ' on' : ''}" data-f="${k}">${label}</span>`)
    .join('');
  row.querySelectorAll('.fchip').forEach((el) =>
    el.addEventListener('click', () => {
      state.listFilter = el.dataset.f;
      renderFilters();
      renderList(true);
      pushData();
    })
  );
}

let lastListRender = 0;
function renderList(force) {
  const now = Date.now();
  if (!force && now - lastListRender < 1500) return;
  lastListRender = now;
  const list = document.getElementById('ac-list');
  const bounds = map.getBounds();
  const inViewOnly = document.getElementById('in-view').checked;
  const rows = [];
  let total = 0;
  for (const a of state.aircraft.values()) {
    total++;
    if (!visible(a)) continue;
    if (inViewOnly && !bounds.contains([a.lon, a.lat])) continue;
    rows.push(a);
  }
  rows.sort((a, b) => {
    const p = priorityOf(b) - priorityOf(a);
    if (p) return p;
    const s = state.sortBy;
    if (s === 'altitude') return (b.alt_ft ?? -1) - (a.alt_ft ?? -1);
    if (s === 'speed') return (b.gs_kt ?? -1) - (a.gs_kt ?? -1);
    if (s === 'age') return (a.age_s ?? 0) - (b.age_s ?? 0);
    if (s === 'callsign') return labelOf(a).localeCompare(labelOf(b));
    return (a.age_s ?? 0) - (b.age_s ?? 0);
  });

  const cap = 400;
  const shown = rows.slice(0, cap);
  const html = shown.map((a) => {
    const flags = [];
    if (isEmergency(a)) flags.push(`<em class="tag emergency">${esc((a.emergency || 'EMERG').toUpperCase())}</em>`);
    if (hasFlag(a, 'watchlist')) flags.push('<em class="tag watchlist">WATCH</em>');
    if (hasFlag(a, 'military')) flags.push('<em class="tag military">MIL</em>');
    if (hasFlag(a, 'lost')) flags.push('<em class="tag lost">LOST</em>');
    const sub = [a.airline, a.operator, a.type_code, a.registration, a.country]
      .filter(Boolean)
      .slice(0, 3)
      .map(esc)
      .join(' · ');
    const alt = a.on_ground ? 'GND' : flightLevel(a.alt_ft);
    const spd = a.gs_kt === null || a.gs_kt === undefined ? '—' : `${Math.round(a.gs_kt)}kt`;
    return `<div class="arow${state.selected === a.hex ? ' sel' : ''}" data-id="${esc(a.hex)}">
      <span class="dot" style="background:${BAND[bandOf(a)]}"></span>
      <span class="amain">
        <span class="atitle">${esc(labelOf(a))}${flags.join('')}</span>
        <span class="asub">${sub || esc(a.hex.toUpperCase())} · ${ageStr(a.age_s)}</span>
      </span>
      <span class="ametrics"><b>${esc(alt)}</b><i>${esc(spd)}</i></span>
    </div>`;
  });
  list.innerHTML = html || '<p class="hint" style="padding:10px">No aircraft match. Zoom the map — the receiver query follows your view — or clear the filters.</p>';
  list.querySelectorAll('.arow').forEach((el) =>
    el.addEventListener('click', () => selectAircraft(el.dataset.id))
  );

  const hiddenNote = state.hidden ? ` (${state.hidden} outside view)` : '';
  const capped = rows.length > cap ? ` · showing ${cap}` : '';
  document.getElementById('ac-count').textContent = `${rows.length} of ${total} tracked${capped}${hiddenNote}`;
}

function renderAlerts() {
  const list = document.getElementById('alert-list');
  const badge = document.getElementById('alert-badge');
  badge.textContent = String(state.alerts.length);
  list.innerHTML = (state.alerts || [])
    .slice(0, 120)
    .map(
      (a) => `<div class="row" data-id="${esc(a.track_id || '')}">
        <span class="sev ${esc(a.severity)}"></span>
        <span class="alert-msg">${esc(a.message)}</span>
        <span class="alert-time">${new Date(a.ts).toISOString().slice(11, 19)}Z</span>
      </div>`
    )
    .join('') || '<p class="hint" style="padding:10px">No air alerts yet. Emergency squawks, watched aircraft and lost contacts raise one.</p>';
  list.querySelectorAll('.row').forEach((el) =>
    el.addEventListener('click', () => el.dataset.id && selectAircraft(el.dataset.id))
  );
}

function renderWatch() {
  const list = document.getElementById('watch-list');
  const items = state.watchlist || [];
  document.getElementById('watch-count').textContent = `${items.length} watched`;
  list.innerHTML = items
    .map((w) => {
      const what = w.hex ? w.hex.toUpperCase() : w.callsign ? w.callsign : w.registration || '—';
      const kind = w.hex ? 'hex' : w.callsign ? 'callsign' : 'tail';
      const active = [...state.aircraft.values()].some(
        (a) =>
          (w.hex && a.hex === w.hex) ||
          (w.callsign && a.callsign && a.callsign.toUpperCase() === w.callsign.toUpperCase()) ||
          (w.registration && a.registration && a.registration.toUpperCase() === w.registration.toUpperCase())
      );
      return `<div class="zrow">
        <span>${active ? '🟢' : '⚪'} ${esc(what)} <em class="mini">${kind}${w.note ? ' · ' + esc(w.note) : ''}</em></span>
        <span class="zx" data-id="${esc(w.id)}" title="remove">✕</span>
      </div>`;
    })
    .join('') || '<p class="hint">Nothing watched yet.</p>';
  list.querySelectorAll('.zx').forEach((el) =>
    el.addEventListener('click', () => deleteWatch(el.dataset.id))
  );
}

function renderFeeds() {
  const el = document.getElementById('feeds');
  el.innerHTML = (state.feeds || [])
    .map((f) => {
      const state_ = f.state === 'ok' ? 'ok' : f.state === 'error' ? 'bad' : 'warn';
      const label =
        f.state === 'ok' ? 'LIVE' : f.state === 'error' ? 'ERROR' : f.state === 'throttled' ? 'THROTTLED' : 'WAIT';
      return `<span class="feed" title="${esc(f.detail || '')}">
        <span class="fstate ${state_}">●</span> ${esc(f.name)} <span class="mini">${label} · ${f.lines || 0} req</span>
      </span>`;
    })
    .join('') || '<span class="mini">no feed</span>';
}

function renderStats() {
  const s = state.stats || {};
  document.getElementById('stats').innerHTML = [
    `<span><b>${s.aircraft || 0}</b> aircraft</span>`,
    `<span><b>${s.airborne || 0}</b> airborne</span>`,
    `<span><b>${s.military || 0}</b> military</span>`,
    `<span class="${s.emergency ? 'bad' : ''}"><b>${s.emergency || 0}</b> emergency</span>`,
    `<span><b>${s.lost || 0}</b> lost contact</span>`,
    `<span><b>${s.watchlist || 0}</b> watched</span>`,
    `<span><b>${s.carriers || 0}</b> carriers</span>`,
    `<span><b>${s.alerts || 0}</b> alerts</span>`,
  ].join('');
}

/* --------------------------------------------------------------- drawer */

function selectAircraft(hex) {
  state.selected = hex;
  renderDrawer();
  renderList(true);
  const src = map.getSource('aircraft');
  if (src) {
    // keep the selection ring on the right row
    if (map.getLayer('sel-ring')) map.setFilter('sel-ring', ['==', ['get', 'id'], hex]);
  }
}

function closeDrawer() {
  state.selected = null;
  document.getElementById('drawer').classList.add('hidden');
  if (map.getLayer('sel-ring')) map.setFilter('sel-ring', ['==', ['get', 'id'], '']);
  renderList(true);
}

function centerOn(hex) {
  const a = state.aircraft.get(hex);
  if (!a) return;
  const [lon, lat] = positionOf(a);
  map.easeTo({ center: [lon, lat], zoom: Math.max(map.getZoom(), 9) });
}

function renderDrawer() {
  const drawer = document.getElementById('drawer');
  const a = state.selected ? state.aircraft.get(state.selected) : null;
  if (!a) {
    drawer.classList.add('hidden');
    return;
  }
  drawer.classList.remove('hidden');
  const flags = [];
  if (isEmergency(a)) flags.push(`<span class="vbadge dark">${esc((a.emergency || 'EMERGENCY').toUpperCase())}</span>`);
  if (hasFlag(a, 'watchlist')) flags.push('<span class="vbadge warn">WATCHLIST</span>');
  if (hasFlag(a, 'military')) flags.push('<span class="vbadge ok" style="color:#e2e8f0;border-color:#334155">MILITARY</span>');
  if (hasFlag(a, 'lost')) flags.push('<span class="vbadge">LOST CONTACT</span>');
  flags.push(`<span class="vbadge ais">${esc(a.source || 'adsb')}</span>`);
  if (a.country) flags.push(`<span class="vbadge">${esc(a.country)}</span>`);
  if (a.pia) flags.push('<span class="vbadge">PIA</span>');
  if (a.ladd) flags.push('<span class="vbadge">LADD</span>');
  if (a.spi) flags.push('<span class="vbadge warn">IDENT</span>');

  const alt = a.on_ground ? 'on ground' : `${nf(a.alt_ft)} ft`;
  const tape = a.on_ground || a.alt_ft == null ? '' : altTape(Number(a.alt_ft));
  const sq = a.squawk ? `${esc(a.squawk)}${SQUAWK_MEANING[a.squawk] ? ' — ' + SQUAWK_MEANING[a.squawk] : ''}` : '—';
  const vrate = a.vert_rate_fpm == null ? '—' : `${a.vert_rate_fpm > 0 ? '▲' : a.vert_rate_fpm < 0 ? '▼' : '–'} ${nf(Math.abs(a.vert_rate_fpm))} ft/min`;

  const rows = [
    ['callsign', a.callsign || '—'],
    ['ICAO hex', (a.hex || '').toUpperCase()],
    ['registration', a.registration || '—'],
    ['type', a.type_code || '—'],
    ['operator / airline', a.airline ? `${esc(a.airline)}${a.operator ? ' · ' + esc(a.operator) : ''}` : a.operator ? esc(a.operator) : '—'],
    ['nav modes', a.nav_modes ? esc(a.nav_modes) : '—'],
    ['emitter category', a.category || '—'],
    ['country', a.country || '—'],
    ['altitude', `${alt}${a.alt_geom_ft != null && !a.on_ground ? ` (geom ${nf(a.alt_geom_ft)} ft)` : ''}`],
    ['flight level', a.on_ground ? '—' : flightLevel(a.alt_ft)],
    ['selected altitude', a.nav_alt_ft != null ? `${nf(a.nav_alt_ft)} ft` : '—'],
    ['vertical rate', vrate],
    ['ground speed', a.gs_kt != null ? `${nf(a.gs_kt, 1)} kt` : '—'],
    ['indicated / true', a.ias_kt != null || a.tas_kt != null ? `${nf(a.ias_kt)} / ${nf(a.tas_kt)} kt` : '—'],
    ['mach', a.mach != null ? Number(a.mach).toFixed(3) : '—'],
    ['track / heading', a.track_deg != null ? `${nf(a.track_deg, 1)}° / ${nf(a.true_heading ?? a.mag_heading, 1)}°` : '—'],
    ['squawk', sq],
    ['emergency', a.emergency ? esc(a.emergency) : 'none'],
    ['heard', `${ageStr(a.age_s)} ago (feed says ${nf(a.seen_s, 1)}s at last poll)`],
    ['messages', a.messages ? nf(a.messages) : '—'],
    ['signal', a.rssi != null ? `${nf(a.rssi, 1)} dBFS` : '—'],
    ['from receiver', a.dst_nm != null ? `${nf(a.dst_nm, 1)} nm${a.dir_deg != null ? ` @ ${nf(a.dir_deg)}°` : ''}` : '—'],
    ['first seen', new Date(a.first_seen).toISOString().slice(11, 19) + 'Z'],
    ['last heard', new Date(a.last_seen).toISOString().slice(11, 19) + 'Z'],
    ['trail', `${(a.trail || []).length} points`],
  ];

  const watchBtn = state.watchlist.some((w) => w.hex === a.hex)
    ? '<button class="btn ghost" id="d-watch" disabled>ALREADY WATCHED</button>'
    : '<button class="btn" id="d-watch">WATCH THIS AIRCRAFT</button>';

  document.getElementById('drawer-body').innerHTML = `
    <h2 class="vname">${esc(labelOf(a))}</h2>
    <div class="vsub">${esc(a.type_code || 'unknown type')} · ${esc((a.hex || '').toUpperCase())}${a.registration ? ' · ' + esc(a.registration) : ''}</div>
    <div class="badges">${flags.join('')}</div>
    ${tape}
    <div class="badges">
      ${watchBtn}
      <button class="btn ghost" id="d-center">CENTER</button>
      <button class="btn ghost" id="d-copy">COPY HEX</button>
    </div>
    <div class="sec-title">STATE VECTOR</div>
    <table class="kv">${rows
      .map(([k, v]) => `<tr><td>${esc(k)}</td><td>${v}</td></tr>`)
      .join('')}</table>
    <div class="sec-title">AIRFRAME RECORD</div>
    <div id="d-record" class="hint">Loading the network's airframe record…</div>
  `;

  document.getElementById('d-watch').addEventListener('click', () => addWatch({ hex: a.hex }, 'watching ' + labelOf(a)));
  document.getElementById('d-center').addEventListener('click', () => centerOn(a.hex));
  document.getElementById('d-copy').addEventListener('click', () => {
    navigator.clipboard?.writeText(a.hex.toUpperCase());
    banner(`Copied ${a.hex.toUpperCase()}`, '');
  });
  loadRecord(a.hex);
}

/// Altitude tape: where this aircraft sits against the usual flight bands.
function altTape(alt) {
  const max = 45000;
  const pct = Math.max(0, Math.min(100, (alt / max) * 100));
  return `<div class="alt-tape" title="${nf(alt)} ft"><i style="left:${pct}%"></i></div>`;
}

async function loadRecord(hex) {
  const el = document.getElementById('d-record');
  if (!el) return;
  try {
    const r = await api(`/api/icarus/aircraft?hex=${encodeURIComponent(hex)}`);
    if (!r.ok) throw new Error('lookup failed');
    const j = await r.json();
    if (!el.isConnected || state.selected !== hex) return;
    const ac = (j.detail && j.detail.ac && j.detail.ac[0]) || null;
    if (!ac) {
      el.textContent = 'The network has no extra record for this airframe (common for military and privately-registered aircraft).';
      return;
    }
    const picks = [
      ['owner / operator', ac.ownOp],
      ['manufacturer', ac.manufacturer],
      ['model', ac.model],
      ['year', ac.year],
      ['registration', ac.r],
      ['military flag', ac.dbFlags && ac.dbFlags & 1 ? 'yes' : null],
    ].filter(([, v]) => v !== undefined && v !== null && v !== '');
    el.innerHTML = picks.length
      ? `<table class="kv">${picks
          .map(([k, v]) => `<tr><td>${esc(k)}</td><td>${esc(v)}</td></tr>`)
          .join('')}</table>`
      : 'Only the state vector above is known for this airframe.';
  } catch (e) {
    if (el.isConnected) el.textContent = 'Airframe record unavailable.';
  }
}

/* --------------------------------------------------------------- actions */

function parseWatchInput(v) {
  const s = v.trim();
  if (!s) return null;
  const lower = s.toLowerCase();
  if (lower.startsWith('hex:')) return { hex: lower.slice(4).trim() };
  // A bare 6-character hex string is a transponder address; write it as a
  // callsign instead if that is what you meant.
  if (/^[0-9a-f]{6}$/i.test(s)) return { hex: lower };
  if (/^[A-Z0-9]{1,2}-[A-Z0-9]{1,5}$/i.test(s) || /^N\d{1,5}[A-Z]{0,2}$/i.test(s)) {
    return { registration: s.toUpperCase() };
  }
  return { callsign: s.toUpperCase() };
}

/// Naval contact detail: the same contact the ocean map holds, with its
/// position, course and speed, and a link across to that map.
function selectSurface(id) {
  const c = (state.surface || []).find((x) => x.id === id);
  if (!c) return;
  state.selectedSurface = id;
  const drawer = document.getElementById('drawer');
  drawer.classList.remove('hidden');
  const rows = [
    ['AIS name', c.name || '—'],
    ['MMSI', c.mmsi ? String(c.mmsi) : '—'],
    ['class', c.carrier ? 'aircraft carrier' : c.naval ? 'naval / warship' : c.classification || '—'],
    ['position', `${num(c.lat, 5)}, ${num(c.lon, 5)}`],
    ['course / speed', `${c.cog != null ? Math.round(c.cog) + '°' : '—'} / ${c.sog != null ? c.sog.toFixed(1) + ' kt' : '—'}`],
    ['last AIS heard', ageStr(c.age_s) + ' ago'],
    ['source', 'OceanSentinel AIS picture (same store as the sea map)'],
  ];
  document.getElementById('drawer-body').innerHTML = `
    <h2 class="vname">${esc(c.name || 'Naval contact')}</h2>
    <div class="vsub">${esc(c.id)}</div>
    <div class="badges">
      ${c.carrier ? '<span class="vbadge warn">AIRCRAFT CARRIER</span>' : '<span class="vbadge warn">NAVAL / WARSHIP</span>'}
      <span class="vbadge ok">SURFACE PICTURE</span>
    </div>
    <table class="kv">${rows.map(([k, v]) => `<tr><td>${esc(k)}</td><td>${esc(v)}</td></tr>`).join('')}</table>
    <p class="hint">A carrier shows here only while it transmits AIS — carriers routinely go dark at sea, so an empty
      list is not evidence of absence.</p>
    <div style="display:flex;gap:6px;flex-wrap:wrap">
      <button class="btn" id="s-center">CENTER</button>
      <a class="btn ghost" style="text-decoration:none" href="/?select=${encodeURIComponent(c.id)}" title="Open the maritime map on this contact">OCEAN MAP ↗</a>
    </div>`;
  document.getElementById('s-center').addEventListener('click', () => {
    map.easeTo({ center: [c.lon, c.lat], zoom: Math.max(map.getZoom(), 7) });
  });
}

async function addWatch(body, label) {
  try {
    const r = await api('/api/icarus/watchlist', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    });
    const j = await r.json();
    if (!r.ok) throw new Error(j.error || 'could not add');
    banner(`Watching ${label || 'aircraft'} — a high-severity alert fires the moment it is seen.`, '');
    if (!state.watchlist.some((w) => w.id === j.entry.id)) state.watchlist.push(j.entry);
    renderWatch();
  } catch (e) {
    banner(String(e.message || e), 'bad');
  }
}

async function deleteWatch(id) {
  try {
    await api(`/api/icarus/watchlist?id=${encodeURIComponent(id)}`, { method: 'DELETE' });
    state.watchlist = state.watchlist.filter((w) => w.id !== id);
    renderWatch();
  } catch (e) {
    banner(String(e.message || e), 'bad');
  }
}

function exportGeoJSON() {
  const feats = [];
  for (const a of state.aircraft.values()) {
    if (!visible(a)) continue;
    const [lon, lat] = positionOf(a);
    feats.push({
      type: 'Feature',
      geometry: { type: 'Point', coordinates: [lon, lat] },
      properties: {
        hex: a.hex,
        callsign: a.callsign,
        registration: a.registration,
        type: a.type_code,
        alt_ft: a.alt_ft,
        gs_kt: a.gs_kt,
        track_deg: a.track_deg,
        squawk: a.squawk,
        emergency: a.emergency,
        military: !!a.military,
        country: a.country,
        flags: (a.flags || []).join(','),
      },
    });
  }
  const blob = new Blob([JSON.stringify({ type: 'FeatureCollection', features: feats }, null, 1)], {
    type: 'application/geo+json',
  });
  const url = URL.createObjectURL(blob);
  const link = document.createElement('a');
  link.href = url;
  link.download = `icarus-aircraft-${new Date().toISOString().slice(0, 19).replace(/[:T]/g, '-')}.geojson`;
  link.click();
  URL.revokeObjectURL(url);
}

/* -------------------------------------------------------------- wiring */

document.querySelectorAll('.tab').forEach((tab) =>
  tab.addEventListener('click', () => {
    document.querySelectorAll('.tab').forEach((t) => t.classList.toggle('active', t === tab));
    document.querySelectorAll('.panel').forEach((p) =>
      p.classList.toggle('active', p.id === 'panel-' + tab.dataset.tab)
    );
  })
);

document.getElementById('search').addEventListener('input', (e) => {
  state.search = e.target.value.trim();
  renderList(true);
  pushData();
});
document.getElementById('sort-by').addEventListener('change', (e) => {
  state.sortBy = e.target.value;
  renderList(true);
});
document.getElementById('in-view').addEventListener('change', () => renderList(true));
document.getElementById('clear-alerts').addEventListener('click', () => {
  state.alerts = [];
  renderAlerts();
});
document.getElementById('export-geojson').addEventListener('click', exportGeoJSON);
document.getElementById('drawer-close').addEventListener('click', closeDrawer);
document.getElementById('watch-add').addEventListener('click', () => {
  const raw = document.getElementById('watch-input').value;
  const body = parseWatchInput(raw);
  if (!body) return banner('Enter a hex, callsign or tail number first.', 'warn');
  addWatch(body, raw.trim());
  document.getElementById('watch-input').value = '';
  document.getElementById('watch-note').value = '';
});

document.querySelectorAll('#panel-layers input[data-filter]').forEach((el) =>
  el.addEventListener('change', () => {
    state.detections[el.dataset.filter] = el.checked;
    renderList(true);
    pushData();
    applyLayerVisibility();
  })
);
document.querySelectorAll('#panel-layers input[data-state]').forEach((el) =>
  el.addEventListener('change', () => {
    state.flightState[el.dataset.state] = el.checked;
    renderList(true);
    pushData();
  })
);
document.querySelectorAll('#panel-layers input[data-layer]').forEach((el) =>
  el.addEventListener('change', () => {
    state.layers[el.dataset.layer] = el.checked;
    applyLayerVisibility();
  })
);

const basemapRow = document.getElementById('basemap-row');
basemapRow.innerHTML = Object.entries(BASEMAPS)
  .map(([k, b]) => `<span class="fchip${k === activeBasemap ? ' on' : ''}" data-b="${k}">${b.label}</span>`)
  .join('');
basemapRow.querySelectorAll('.fchip').forEach((el) =>
  el.addEventListener('click', () => setBasemap(el.dataset.b))
);
map.on('load', () => setBasemap(activeBasemap));

document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') {
    closeDrawer();
    state.selected = null;
  }
});

/* ------------------------------------------------------------ boot intro */

const BOOT_MESSAGES = [
  'POWERING RECEIVER ARRAY',
  'ACQUIRING 1090 MHZ UPLINK',
  'DECODING ADS-B FRAMES',
  'LOADING AIRSPACE DATABASE',
  'SYNCING GLOBAL MILITARY SWEEP',
  'ARMING EMERGENCY WATCH',
  'PAINTING AIR PICTURE',
];

let bootDone = false;
let bootState = 0;
let bootFirstState = false;
/// When this page started loading, so the intro can report its own duration
/// (`window.__bootMs`) instead of the timing being a matter of opinion.
const bootStartMs = typeof performance !== 'undefined' ? performance.now() : Date.now();

/// Slim loading strip along the top edge (mirrors boot %, then WS state).
function pageProgress(frac) {
  const el = document.getElementById('page-progress');
  if (el) el.style.width = `${Math.round(frac * 100)}%`;
}

function bootFinish() {
  if (bootDone) return;
  bootDone = true;
  try {
    sessionStorage.setItem('os.boot.air', '1');
  } catch (e) {
    /* ignore */
  }
  try {
    window.__bootMs = Math.round(
      (typeof performance !== 'undefined' ? performance.now() : Date.now()) - bootStartMs
    );
  } catch (e) {
    /* ignore */
  }
  const el = document.getElementById('boot');
  if (el) {
    el.classList.add('boot-done');
    setTimeout(() => el.remove(), 900);
  }
  document.removeEventListener('keydown', bootSkip);
  document.removeEventListener('click', bootSkip);
}

function bootSkip() {
  bootState = Math.max(bootState, 97);
}

/* Both maps run this same sequence — same tick, same easing, same message
   dwell, same data-gated finish — so the two pages feel like one product.
   Keep the constants in step with the other page when tuning. */
function runBoot() {
  const el = document.getElementById('boot');
  if (!el) return;
  let fast = false;
  try {
    fast = sessionStorage.getItem('os.boot.air') === '1';
  } catch (e) {
    /* ignore */
  }
  if (fast) el.classList.add('boot-fast');
  if (window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches) {
    bootFinish();
    return;
  }
  document.addEventListener('keydown', bootSkip);
  document.addEventListener('click', bootSkip);

  const fill = document.getElementById('boot-fill');
  const pct = document.getElementById('boot-pct');
  const status = document.getElementById('boot-status');
  const chips = [...el.querySelectorAll('.boot-chips span')];
  const tickMs = fast ? 16 : 55;
  const factor = fast ? 0.08 : 0.03;
  const floor = fast ? 1 : 0.55;
  let msgIdx = 0;

  const msgTimer = setInterval(() => {
    if (bootDone || !status) return clearInterval(msgTimer);
    status.textContent = BOOT_MESSAGES[msgIdx % BOOT_MESSAGES.length];
    msgIdx += 1;
  }, fast ? 90 : 650);

  // Safety net, not a shortcut: a background tab gets roughly one timer call
  // per minute, and an intro must never outlive the map it covers. It fires
  // well after the normal sequence, so the ordinary timing is unaffected.
  const net = setTimeout(
    () => {
      bootState = Math.max(bootState, 99.5);
    },
    fast ? 2000 : 12000
  );

  const tick = setInterval(() => {
    if (bootDone) return clearInterval(tick);
    // the bar stalls just short of done until real data arrives, so the
    // animation ends when the map is actually live — never before
    const cap = bootFirstState ? 100 : fast ? 80 : 88;
    // once real data has arrived, finish quickly even in throttled background
    // tabs (browsers clamp background timers to ~1/min)
    const effFloor = bootFirstState ? Math.max(floor, 3) : floor;
    bootState = Math.min(cap, bootState + Math.max(effFloor, (cap - bootState) * factor));
    if (fill) fill.style.width = `${bootState.toFixed(1)}%`;
    if (pct) pct.textContent = `${Math.floor(bootState)}%`;
    pageProgress(bootState / 100);
    chips.forEach((c, i) => {
      if (bootState >= (i + 1) * (100 / chips.length) - 8) c.classList.add('on');
    });
    if (bootState >= 100) {
      clearInterval(tick);
      clearInterval(msgTimer);
      clearTimeout(net);
      bootFinish();
    }
  }, tickMs);
}

runBoot();

renderFilters();
connect();
setInterval(() => {
  document.getElementById('clock').textContent = new Date().toISOString().slice(11, 19) + 'Z';
}, 1000);
setInterval(() => {
  if (state.aircraft.size) renderList();
}, 3000);
