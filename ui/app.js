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
  hiddenTracks: 0,
  watchlist: [],
  classFilter: '',
  inViewOnly: false,
  sortBy: 'dark',
  history: null,
  replayTs: null,
  eventTypes: { FISHING: true, ENCOUNTER: true, LOITERING: true, GAP: true, PORT_VISIT: true },
};

// --- optional API token (kept in localStorage) -------------------------------
function apiToken() {
  try {
    return localStorage.getItem('os.token') || '';
  } catch (e) {
    return '';
  }
}

function promptToken() {
  const t = window.prompt('This OceanSentinel server requires an API token:', apiToken());
  if (t !== null) {
    try {
      localStorage.setItem('os.token', t.trim());
    } catch (e) {
      /* ignore */
    }
    location.reload();
  }
}

/// fetch wrapper that attaches the token and explains a 401.
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
    // Esri World Dark Gray Canvas: keyless and no rate-limit warnings. CARTO's
    // dark tiles now throttle unauthenticated browsers with "API KEY REQUIRED"
    // placeholder images.
    tiles: [
      'https://server.arcgisonline.com/ArcGIS/rest/services/Canvas/World_Dark_Gray_Base/MapServer/tile/{z}/{y}/{x}',
    ],
    tileSize: 256,
    attribution: 'Esri Dark Gray Canvas',
    paint: { 'raster-opacity': 0.95 },
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

/// Nested match: source colour (style) x vessel class (shape).
function iconExpr() {
  const expr = ['match', ['get', 'shape']];
  for (const shape of SHAPES) {
    const inner = ['match', ['get', 'style']];
    for (const style of Object.keys(COLORS)) inner.push(style, `v-${style}-${shape}`);
    inner.push(`v-ais-${shape}`);
    expr.push(shape, inner);
  }
  expr.push('v-ais-other');
  return expr;
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

const SHAPES = ['fishing', 'cargo', 'passenger', 'other'];

/// Vessel silhouettes by class, tinted by track source, so a trawler and a
/// tanker read differently at a glance (as on any serious MDA display).
function shipIcon(color, shape = 'other', size = 34) {
  const c = document.createElement('canvas');
  c.width = size;
  c.height = size;
  const ctx = c.getContext('2d');
  const s = size;
  ctx.beginPath();
  if (shape === 'fishing') {
    ctx.moveTo(s / 2, 2.5);
    ctx.lineTo(s * 0.66, s - 6);
    ctx.lineTo(s / 2, s * 0.8);
    ctx.lineTo(s * 0.34, s - 6);
  } else if (shape === 'cargo') {
    ctx.moveTo(s / 2, 2.5);
    ctx.lineTo(s * 0.74, s * 0.28);
    ctx.lineTo(s * 0.74, s - 5);
    ctx.lineTo(s * 0.26, s - 5);
    ctx.lineTo(s * 0.26, s * 0.28);
  } else if (shape === 'passenger') {
    const r = s * 0.22;
    ctx.moveTo(s / 2, 2.5);
    ctx.lineTo(s * 0.8, s * 0.35);
    ctx.quadraticCurveTo(s * 0.8, s - 5, s / 2 + r, s - 5);
    ctx.lineTo(s / 2 - r, s - 5);
    ctx.quadraticCurveTo(s * 0.2, s - 5, s * 0.2, s * 0.35);
  } else {
    ctx.moveTo(s / 2, 2.5);
    ctx.lineTo(s * 0.84, s - 5);
    ctx.lineTo(s / 2, s * 0.7);
    ctx.lineTo(s * 0.16, s - 5);
  }
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
  for (const style of Object.keys(COLORS)) {
    for (const shape of SHAPES) {
      map.addImage(`v-${style}-${shape}`, shipIcon(COLORS[style], shape), { pixelRatio: 2 });
    }
  }
  map.addImage('v-own', ownIcon(), { pixelRatio: 2 });
  map.addImage('v-history', shipIcon('#94a3b8', 'other'), { pixelRatio: 2 });

  for (const id of ['zones', 'gfw', 'trails', 'own', 'draw', 'graticule', 'history']) {
    map.addSource(id, { type: 'geojson', data: EMPTY_FC });
  }
  // tracks are clustered: at low zoom a worldwide feed would otherwise paint
  // thousands of overlapping icons
  map.addSource('tracks', {
    type: 'geojson',
    data: EMPTY_FC,
    cluster: true,
    clusterRadius: 46,
    clusterMaxZoom: 7,
  });

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
    id: 'clusters',
    type: 'circle',
    source: 'tracks',
    filter: ['has', 'point_count'],
    paint: {
      'circle-color': [
        'step', ['get', 'point_count'],
        'rgba(34, 211, 238, 0.22)', 25, 'rgba(56, 189, 248, 0.28)', 200, 'rgba(125, 211, 252, 0.32)',
      ],
      'circle-radius': ['step', ['get', 'point_count'], 15, 25, 21, 200, 30],
      'circle-stroke-color': 'rgba(34, 211, 238, 0.75)',
      'circle-stroke-width': 1,
    },
  });
  map.addLayer({
    id: 'cluster-count',
    type: 'symbol',
    source: 'tracks',
    filter: ['has', 'point_count'],
    layout: {
      'text-field': '{point_count_abbreviated}',
      'text-font': ['Open Sans Regular'],
      'text-size': 11,
      'text-allow-overlap': true,
    },
    paint: { 'text-color': '#e6f7fd' },
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
    filter: ['!', ['has', 'point_count']],
    layout: {
      'icon-image': iconExpr(),
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
    filter: ['!', ['has', 'point_count']],
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
      'symbol-sort-key': ['get', 'priority'],
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
map.on('click', 'clusters', (e) => {
  const f = map.queryRenderedFeatures(e.point, { layers: ['clusters'] })[0];
  if (!f) return;
  const src = map.getSource('tracks');
  src.getClusterExpansionZoom(f.properties.cluster_id).then((zoom) => {
    map.easeTo({ center: f.geometry.coordinates, zoom: zoom + 0.2 });
  });
});

map.on('click', 'gfw-circles', (e) => {
  const p = e.features && e.features[0] && e.features[0].properties;
  if (p && p.ssvid) gfwLookup(String(p.ssvid));
});
map.on('mouseenter', 'gfw-circles', () => (map.getCanvas().style.cursor = 'pointer'));
map.on('mouseleave', 'gfw-circles', () => (map.getCanvas().style.cursor = ''));
map.on('mouseenter', 'clusters', () => (map.getCanvas().style.cursor = 'pointer'));
map.on('mouseleave', 'clusters', () => (map.getCanvas().style.cursor = ''));

// keep the server's viewport in step with the map (debounced by moveend)
map.on('moveend', () => {
  sendViewport(state.ws);
  if (state.inViewOnly) renderTrackList();
});

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

/// Vessel class -> icon silhouette.
function shapeOf(t) {
  const c = (t.classification || '').toLowerCase();
  if (c === 'fishing' || c === 'sailing' || c === 'pleasure' || c === 'dredging') return 'fishing';
  if (c === 'cargo' || c === 'tanker' || c === 'towing' || c === 'highspeed') return 'cargo';
  if (c === 'passenger' || c === 'patrol' || c === 'sar' || c === 'medical' || c === 'military' || c === 'port' || c === 'pilot') {
    return 'passenger';
  }
  return 'other';
}

function visible(t) {
  const f = state.filters;
  const s = t.sources || [];
  const srcOk = t.dark
    ? f.dark
    : (s.includes('ais') && f.ais) ||
      (s.includes('sonar') && f.sonar) ||
      (s.includes('lidar') && f.lidar) ||
      (s.includes('radar') && f.sonar);
  if (!srcOk) return false;
  if (state.classFilter && shapeOf(t) !== state.classFilter) return false;
  if (state.inViewOnly && !map.getBounds().contains([t.lon, t.lat])) return false;
  return true;
}

/// Dead-reckon the icon between AIS reports so ships glide instead of jumping.
/// Extrapolation is capped at 45 s, so a stale track stops rather than flying.
function positionOf(t) {
  const trail = t.trail || [];
  if (trail.length < 2) return [t.lon, t.lat];
  const b = trail[trail.length - 1];
  const t1 = Date.parse(b.ts);
  if (!t1) return [b.lon, b.lat];
  const dt = (Date.now() - t1) / 1000;
  if (dt <= 0 || dt > 600) return [b.lon, b.lat];
  const sog = t.sog;
  const cog = t.cog;
  if (!sog || sog < 0.5 || cog === null || cog === undefined) return [b.lon, b.lat];
  const cap = Math.min(dt, 45);
  const dist = sog * 0.514444 * cap;
  const R = 6371000;
  const br = (cog * Math.PI) / 180;
  const lat1 = (b.lat * Math.PI) / 180;
  const lon1 = (b.lon * Math.PI) / 180;
  const d = dist / R;
  const lat2 = Math.asin(Math.sin(lat1) * Math.cos(d) + Math.cos(lat1) * Math.sin(d) * Math.cos(br));
  const lon2 =
    lon1 +
    Math.atan2(Math.sin(br) * Math.sin(d) * Math.cos(lat1), Math.cos(d) - Math.sin(lat1) * Math.sin(lat2));
  let lon = (lon2 * 180) / Math.PI;
  if (lon > 180) lon -= 360;
  if (lon < -180) lon += 360;
  return [lon, (lat2 * 180) / Math.PI];
}

function matchSearch(t) {
  const q = state.search.trim().toLowerCase();
  if (!q) return true;
  return (
    labelOf(t).toLowerCase().includes(q) ||
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
      geometry: { type: 'Point', coordinates: positionOf(t) },
      properties: {
        id: t.id,
        name: labelOf(t),
        style: styleOf(t),
        shape: shapeOf(t),
        heading: t.heading ?? t.cog ?? 0,
        dark: !!t.dark,
        cls: t.classification || '',
        // dark contacts and alerts win label collisions
        priority: t.dark ? 0 : 1,
      },
    });
  }
  return { type: 'FeatureCollection', features: feats };
}

/// Trails are the heaviest layer (points x tracks x refresh rate), so they are
/// trimmed to what can actually be seen: nothing at world zoom, only tracks in
/// view, at most TRAIL_MAX of them, and fewer points each when crowded.
function trailsFC() {
  const zoom = map.getZoom();
  if (zoom < 5) return EMPTY_FC; // sub-pixel at this scale
  const bounds = map.getBounds();
  const all = [...state.tracks.values()].filter(
    (t) => visible(t) && matchSearch(t) && (t.trail || []).length >= 2
  );
  const crowded = all.length > 600;
  const maxTracks = crowded ? 250 : 1200;
  const pointCap = crowded ? 25 : 90;
  const chosen = crowded
    ? all.sort((a, b) => String(b.last_seen).localeCompare(String(a.last_seen))).slice(0, maxTracks)
    : all;
  const feats = [];
  for (const t of chosen) {
    if (!bounds.contains([t.lon, t.lat])) continue;
    let trail = t.trail;
    if (trail.length > pointCap) trail = trail.slice(-pointCap);
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

const EVENT_COLORS = {
  FISHING: '#f59e0b',
  ENCOUNTER: '#ef4444',
  LOITERING: '#a855f7',
  GAP: '#22d3ee',
  PORT_VISIT: '#4ade80',
};

function gfwFC() {
  return {
    type: 'FeatureCollection',
    features: state.gfwEvents
      .filter((e) => state.eventTypes[(e.event_type || '').toUpperCase()] !== false)
      .map((e) => {
        const kind = (e.event_type || 'FISHING').toUpperCase();
        return {
          type: 'Feature',
          geometry: { type: 'Point', coordinates: [e.lon, e.lat] },
          properties: {
            name: e.vessel_name || '',
            type: kind,
            color: EVENT_COLORS[kind] || '#f59e0b',
            ssvid: e.ssvid || '',
            vessel_id: e.vessel_id || '',
          },
        };
      }),
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
  set('history', historyFC());
  updateDarkMarkers();
}

/// Recorded positions for the replay scrubber (dimmed, non-interactive).
function historyFC() {
  if (!state.history) return EMPTY_FC;
  return {
    type: 'FeatureCollection',
    features: state.history.map((p) => ({
      type: 'Feature',
      geometry: { type: 'Point', coordinates: [p.lon, p.lat] },
      properties: {
        name: p.name || p.id,
        style: p.dark ? 'dark' : 'ais',
        shape: shapeOf(p),
        heading: p.cog ?? 0,
        id: p.id,
      },
    })),
  };
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
  // thousands of labels would dominate frame time and are unreadable anyway
  const labelsUseful = state.tracks.size <= 600 || map.getZoom() >= 9;
  v('tracks-labels', state.layers.labels && labelsUseful);
  v('trails-line', state.layers.trails);
  v('zones-fill', state.layers.zones);
  v('zones-line', state.layers.zones);
  v('gfw-circles', state.layers.gfw);
  v('own-icon', state.layers.own);
  v('own-ring', state.layers.own);
}

/* ------------------------------------------------------------------- ws */

/// Tell the server what we can actually see, so it can stop streaming the
/// rest of the planet to this tab.
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
  const url = `${proto}://${location.host}/ws${token ? '?token=' + encodeURIComponent(token) : ''}`;
  const ws = new WebSocket(url);
  state.ws = ws;
  ws.onopen = () => {
    state.wsOk = true;
    chip('chip-ws', 'WS LIVE', 'ok');
    sendViewport(ws);
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
  state.watchlist = msg.watchlist || [];
  state.hiddenTracks = msg.hidden_tracks || 0;

  if (state.firstState && state.app.aoi && !state.userMoved) {
    state.firstState = false;
    const a = state.app.aoi;
    map.jumpTo({ center: [a.lon, a.lat], zoom: a.zoom || 9 });
  }

  pushData();
  renderTrackList();
  renderAlerts();
  renderZones();
  renderWatchlist();
  renderFeeds();
  renderStats();
  renderChips();
  if (state.selected || state.standalone) renderDrawer();
}

function pushAlert(a) {
  const idx = state.alerts.findIndex((x) => x.id === a.id);
  if (idx === -1) state.alerts.unshift(a);
  renderAlerts();
  if (a.kind === 'dark_contact' || a.kind === 'watchlist_hit' || a.kind === 'collision_risk') {
    banner(a.message, 'bad');
  } else if (a.kind === 'ais_lost' || a.kind === 'feed_stalled') {
    banner(a.message, 'warn');
  }
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
    const extra = [];
    if (app.recording) extra.push('track recording is ON (replay scrubber available)');
    if ((app.alert_destinations || []).length) {
      extra.push(`out-of-band alerts → ${app.alert_destinations.join(', ')}`);
    } else {
      extra.push('alerts are in-UI only (add [alerts] webhooks or Telegram in config.toml)');
    }
    hint.textContent = app.gfw_enabled
      ? 'Global Fishing Watch enrichment is active. Select a vessel with an MMSI, or query fishing events in the current view. Note: ' +
        extra.join('; ') + '.'
      : 'Optional: Global Fishing Watch adds registry identity, apparent fishing events, AIS-gap history and IUU-list status. Everything else on this map works without it. To enable it, get a free non-commercial token at globalfishingwatch.org/our-apis/tokens, put GFW_API_TOKEN=… in a .env file next to the app, and restart.';
  }
}

let lastListRender = 0;

function renderTrackList(force) {
  // rebuilding 400 rows every second is wasted work: the map is the live view
  const now = Date.now();
  if (!force && now - lastListRender < 2000) return;
  lastListRender = now;
  const list = document.getElementById('track-list');
  const rows = [];
  for (const t of state.tracks.values()) {
    if (!visible(t) || !matchSearch(t)) continue;
    rows.push(t);
  }
  const center = map.getCenter();
  if (state.sortBy === 'name') {
    rows.sort((a, b) => labelOf(a).localeCompare(labelOf(b)));
  } else if (state.sortBy === 'distance') {
    rows.sort((a, b) => distanceKm(a, center) - distanceKm(b, center));
  } else if (state.sortBy === 'speed') {
    rows.sort((a, b) => (b.sog || 0) - (a.sog || 0));
  } else {
    rows.sort((a, b) => (b.dark ? 1 : 0) - (a.dark ? 1 : 0) || String(b.last_seen).localeCompare(String(a.last_seen)));
  }
  const total = rows.length;
  const CAP = 400;
  const shown = rows.slice(0, CAP);
  document.getElementById('track-count').textContent =
    total > CAP ? `${shown.length} of ${total} tracks` : `${total} tracks`;
  list.innerHTML = shown
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
  if (total > CAP) {
    list.insertAdjacentHTML(
      'beforeend',
      `<div class="row" style="justify-content:center;color:#6b8299">showing first ${CAP} of ${total} — filter or zoom in to narrow</div>`
    );
  }
  list.querySelectorAll('.row[data-id]').forEach((el) =>
    el.addEventListener('click', () => selectTrack(el.dataset.id))
  );
}

/// Rough great-circle distance in km (sorting only).
function distanceKm(t, c) {
  const dLat = (t.lat - c.lat) * 111.32;
  const dLon = (t.lon - c.lng) * 111.32 * Math.cos((c.lat * Math.PI) / 180);
  return Math.sqrt(dLat * dLat + dLon * dLon);
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

function renderWatchlist() {
  const el = document.getElementById('watch-list');
  if (!el) return;
  const w = state.watchlist || [];
  document.getElementById('watch-count').textContent = `${w.length} watched`;
  el.innerHTML = w.length
    ? w
        .map(
          (e) => `<div class="zrow">
            <span>${e.mmsi ? 'MMSI ' + esc(e.mmsi) : e.imo ? 'IMO ' + esc(e.imo) : esc(e.name || '')}${
            e.name && (e.mmsi || e.imo) ? ' · ' + esc(e.name) : ''
          }${e.note ? ' <span style="color:#fbbf24">(' + esc(e.note) + ')</span>' : ''}</span>
            <span class="zx" data-id="${esc(e.id)}" title="remove">✕</span>
          </div>`
        )
        .join('')
    : '<div style="color:#6b8299">Nothing watched yet. Add an MMSI, IMO or exact name — or select a vessel and press WATCH.</div>';
  el.querySelectorAll('.zx').forEach((x) =>
    x.addEventListener('click', () => deleteWatch(x.dataset.id))
  );
}

async function addWatch(mmsi, imo, name, note) {
  const body = {};
  if (mmsi) body.mmsi = Number(mmsi);
  if (imo) body.imo = Number(imo);
  if (name) body.name = name;
  if (note) body.note = note;
  if (!body.mmsi && !body.imo && !body.name) {
    banner('Enter an MMSI, IMO or name to watch for.', 'warn');
    return;
  }
  try {
    const r = await api('/api/watchlist', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || r.statusText);
    banner(`Watching ${body.mmsi || body.imo || body.name} — you will be alerted when it appears.`);
  } catch (e) {
    banner('watchlist add failed: ' + e.message, 'warn');
  }
}

async function deleteWatch(id) {
  try {
    await api(`/api/watchlist?id=${encodeURIComponent(id)}`, { method: 'DELETE' });
    state.watchlist = (state.watchlist || []).filter((w) => w.id !== id);
    renderWatchlist();
  } catch (e) {
    banner('watchlist remove failed: ' + e.message, 'warn');
  }
}

// ---------------------------------------------------------------- replay

function updateReplayUI() {
  const bar = document.getElementById('replaybar');
  const label = document.getElementById('replay-label');
  const live = document.getElementById('replay-live');
  if (!bar) return;
  const on = !!state.history;
  bar.classList.toggle('hidden', !on && !state.app.recording);
  live.classList.toggle('hidden', !on);
  if (!on) {
    label.textContent = state.app.recording ? 'RECORDING — scrub to replay' : '';
    return;
  }
  const d = new Date(state.replayTs);
  label.textContent = `REPLAY ${d.toISOString().slice(0, 16).replace('T', ' ')}Z · ${state.history.length} vessels`;
}

async function loadHistory(ts) {
  try {
    const r = await api(`/api/history${ts ? '?ts=' + Math.floor(ts) : ''}`);
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || r.statusText);
    state.history = d.points || [];
    state.replayTs = Date.parse(d.ts);
    const slider = document.getElementById('replay-slider');
    if (slider) {
      const from = Date.parse(d.from) / 1000;
      const to = Date.parse(d.to) / 1000;
      slider.min = String(from);
      slider.max = String(to);
      slider.value = String(state.replayTs / 1000);
    }
    pushData();
    updateReplayUI();
  } catch (e) {
    banner('replay failed: ' + e.message, 'warn');
  }
}

function exitReplay() {
  state.history = null;
  state.replayTs = null;
  pushData();
  updateReplayUI();
}

function activeEventTypes() {
  return Object.entries(state.eventTypes)
    .filter(([, v]) => v)
    .map(([k]) => k)
    .join(',');
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
      await api(`/api/zones?id=${encodeURIComponent(x.dataset.id)}`, { method: 'DELETE' });
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
    `TRACKS <b>${s.tracks ?? 0}</b>${state.hiddenTracks ? ` (+${state.hiddenTracks} out of view)` : ''}`,
    `WATCH <b>${s.watchlist ?? 0}</b>`,
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
      r = await api(url);
      data = await r.json();
    } catch (netErr) {
      // one retry: the app may have been restarting when the click landed
      await new Promise((res) => setTimeout(res, 1500));
      r = await api(url);
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
  if (t.cpa_m !== null && t.cpa_m !== undefined) {
    push('Closest approach', `${num(t.cpa_m, 0)} m${t.tcpa_min !== null && t.tcpa_min !== undefined ? ` in ${num(t.tcpa_min, 1)} min` : ''}`);
  }

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
      ${
        t.mmsi || t.imo || t.name
          ? `<button class="btn ghost" id="d-watch">WATCH</button>`
          : ''
      }
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
  const watchBtn = document.getElementById('d-watch');
  if (watchBtn) {
    watchBtn.onclick = () => addWatch(t.mmsi, t.imo, t.name, 'from map');
  }
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
  api('/api/zones', {
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
    const r = await api(`/api/gfw/events?bbox=${bbox}&days=${days}&limit=${limit}&types=${activeEventTypes()}`);
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
      r = await api(url);
      data = await r.json();
    } catch (netErr) {
      await new Promise((res) => setTimeout(res, 1500));
      r = await api(url);
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
    renderTrackList(true);
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
  renderTrackList(true);
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
    await api(`/api/zones?id=${encodeURIComponent(z.id)}`, { method: 'DELETE' });
  }
};
document.getElementById('gfw-query').onclick = () => queryGfwEvents();

// --- watchlist panel --------------------------------------------------------
const watchAdd = document.getElementById('watch-add');
if (watchAdd) {
  watchAdd.onclick = () => {
    const v = document.getElementById('watch-input').value.trim();
    const note = document.getElementById('watch-note').value.trim();
    if (!v) return;
    document.getElementById('watch-input').value = '';
    document.getElementById('watch-note').value = '';
    if (/^\d{6,9}$/.test(v)) addWatch(v, null, null, note);
    else if (/^IMO\s*\d{5,9}$/i.test(v)) addWatch(null, v.replace(/\D/g, ''), null, note);
    else addWatch(null, null, v, note);
  };
  document.getElementById('watch-input').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') watchAdd.click();
  });
}

// --- list filters -----------------------------------------------------------
const classSelect = document.getElementById('class-filter');
if (classSelect) {
  classSelect.onchange = () => {
    state.classFilter = classSelect.value;
    pushData();
    renderTrackList(true);
  };
}
const inView = document.getElementById('in-view');
if (inView) {
  inView.onchange = () => {
    state.inViewOnly = inView.checked;
    pushData();
    renderTrackList(true);
  };
}
const sortSelect = document.getElementById('sort-by');
if (sortSelect) {
  sortSelect.onchange = () => {
    state.sortBy = sortSelect.value;
    renderTrackList(true);
  };
}

// --- GFW event type filters -------------------------------------------------
document.querySelectorAll('input[data-event]').forEach((el) =>
  el.addEventListener('change', () => {
    state.eventTypes[el.dataset.event] = el.checked;
    pushData();
    renderGfwLegend();
  })
);

function renderGfwLegend() {
  const el = document.getElementById('gfw-legend');
  if (!el) return;
  el.innerHTML = Object.entries(EVENT_COLORS)
    .filter(([k]) => state.eventTypes[k])
    .map(([k, c]) => `<span class="feed"><span class="fstate" style="color:${c}">●</span>${k.toLowerCase().replace('_', ' ')}</span>`)
    .join('');
}

// --- replay scrubber --------------------------------------------------------
const replaySlider = document.getElementById('replay-slider');
if (replaySlider) {
  replaySlider.addEventListener('input', () => {
    loadHistory(Number(replaySlider.value));
  });
}
const replayLive = document.getElementById('replay-live');
if (replayLive) {
  replayLive.onclick = () => {
    exitReplay();
    loadHistory(null);
  };
}
const replayStart = document.getElementById('replay-start');
if (replayStart) {
  replayStart.onclick = () => {
    const now = Date.now() / 1000;
    loadHistory(now - 3600);
  };
}
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
