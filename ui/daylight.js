'use strict';

/* Day / night mapping for both maps (OceanSentinel and Project Icarus), drawn
 * like the day-night charts used for solar eclipses: a shaded night side with
 * soft twilight bands rather than a single hard line.
 *
 * Four nested regions, each an exact solar-elevation threshold:
 *
 *   sun below   0°  night (sun below the horizon)
 *   sun below  -6°  civil twilight ends      (street lights come on)
 *   sun below -12°  nautical twilight ends   (horizon invisible at sea)
 *   sun below -18°  astronomical twilight    (sky fully dark)
 *
 * Stacked with increasing opacity they read as one gradient — the penumbra
 * look — while every boundary is a real threshold, so the picture tracks
 * sunlight honestly rather than decoratively.
 *
 * Method: the sub-solar point and each band boundary come from the standard
 * low-precision solar position algorithm. A boundary latitude is found by
 * scanning from the dark pole and bisecting the first crossing of the
 * threshold. That is deliberately not the closed form: the analytic roots flip
 * branches near the equinoxes (the first version of this file returned
 * latitudes outside ±90 there and quietly produced a self-intersecting polygon
 * that never rendered at all). Scanning for the crossing nearest the dark pole
 * is correct in every season, including the equinox and the polar day/night.
 *
 * Everything runs on the client: no server data, one recomputation a minute.
 * The overlay is its own layer stack inserted above the basemap and below all
 * data layers, so it works over all seven basemaps without dimming traffic.
 *
 * Labelling is honest: the meridians mark *solar* hours, one per 15° of
 * longitude. Political time zones are not 15° wide — India is UTC+5:30, China
 * runs one zone across five solar hours — so they say UTC±N. */

window.OSDaylight = (function () {
  const DEG = Math.PI / 180;
  // Mercator cannot draw the poles; the polar edge of each ring is flattened
  // here, where the shading is solid night in any case.
  const MAX_LAT = 85;
  const SUN_COLOR = '#facc15';

  /// The four thresholds, outermost first, each with its own tint and weight.
  const BANDS = [
    { h: 0, col: '#0e2148', op: 0.2, name: 'night' },
    { h: -6, col: '#0a1730', op: 0.14, name: 'civil twilight' },
    { h: -12, col: '#081226', op: 0.14, name: 'nautical twilight' },
    { h: -18, col: '#050b18', op: 0.16, name: 'astronomical twilight' },
  ];

  let map = null;
  let beforeId = null;
  let timer = null;
  const visible = { night: true, meridians: true };

  /* --------------------------------------------------------------- astronomy */

  /// Sub-solar point: where the sun is exactly overhead right now.
  function subsolar(date) {
    const jd = date.getTime() / 86400000 + 2440587.5;
    const n = jd - 2451545.0;
    const L = (280.46 + 0.9856474 * n) * DEG; // mean longitude
    const g = (357.528 + 0.9856003 * n) * DEG; // mean anomaly
    const lambda = L + (1.915 * Math.sin(g) + 0.02 * Math.sin(2 * g)) * DEG;
    const eps = (23.439 - 0.0000004 * n) * DEG; // obliquity of the ecliptic
    const dec = Math.asin(Math.sin(eps) * Math.sin(lambda));
    const ra = Math.atan2(Math.cos(eps) * Math.sin(lambda), Math.cos(lambda));
    const gmst = (280.46061837 + 360.98564736629 * n) * DEG;
    let lon = (ra - gmst) / DEG;
    lon = ((((lon + 180) % 360) + 360) % 360) - 180;
    return { lat: dec / DEG, lon };
  }

  /// Solar elevation in degrees at a position, given the sun's declination and
  /// longitude. Every band is a threshold of this quantity.
  function solarElevation(lat, lon, dec, subLon) {
    const H = (lon - subLon) * DEG;
    const p = lat * DEG;
    const d = dec * DEG;
    return Math.asin(Math.sin(p) * Math.sin(d) + Math.cos(p) * Math.cos(d) * Math.cos(H)) / DEG;
  }

  /// The latitude interval of a band on one meridian, or null if the meridian
  /// misses the band entirely.
  ///
  /// A band {sun below h0} is a spherical cap around the *antisolar* point of
  /// angular radius 90 + h0, so its intersection with a meridian is a single
  /// arc — solar elevation along a meridian is unimodal. Depending on the
  /// season that arc either hangs off the dark pole or floats free between two
  /// crossings: the deeper twilight bands do not reach the pole for most of the
  /// year, which is what a pole-anchored construction gets wrong (it made the
  /// nautical and astronomical bands vanish over the Americas). Both
  /// topologies fall out of simply collecting the crossings.
  function bandInterval(lon, dec, subLon, h0) {
    const f = (lat) => solarElevation(lat, lon, dec, subLon) - h0;
    const steps = 85; // 2° steps over the visible range
    const crossings = [];
    let prevLat = -MAX_LAT;
    let prevF = f(prevLat);
    for (let i = 1; i <= steps; i++) {
      const lat = -MAX_LAT + (2 * MAX_LAT * i) / steps;
      const cur = f(lat);
      if (prevF < 0 !== cur < 0) {
        let a = prevLat;
        let b = lat;
        let fa = prevF;
        for (let k = 0; k < 16; k++) {
          const mid = (a + b) / 2;
          const fm = f(mid);
          if (fa < 0 === fm < 0) {
            a = mid;
            fa = fm;
          } else {
            b = mid;
          }
        }
        crossings.push((a + b) / 2);
      }
      prevLat = lat;
      prevF = cur;
    }
    const lowInside = f(-MAX_LAT) < 0;
    if (crossings.length === 0) return lowInside ? [-MAX_LAT, MAX_LAT] : null;
    if (crossings.length === 1) {
      return lowInside ? [-MAX_LAT, crossings[0]] : [crossings[0], MAX_LAT];
    }
    // Two crossings, so the band is the arc between them. A third case cannot
    // occur: the elevation curve along a meridian has a single extremum.
    return [crossings[0], crossings[1]];
  }

  /// A point at a bearing and distance from another, for sampling circles.
  function destination(lat, lon, brg, distM) {
    const R = 6371008.8;
    const d = distM / R;
    const b = brg * DEG;
    const p1 = lat * DEG;
    const l1 = lon * DEG;
    const p2 = Math.asin(Math.sin(p1) * Math.cos(d) + Math.cos(p1) * Math.sin(d) * Math.cos(b));
    const l2 =
      l1 +
      Math.atan2(Math.sin(b) * Math.sin(d) * Math.cos(p1), Math.cos(d) - Math.sin(p1) * Math.sin(p2));
    return [((((l2 / DEG + 540) % 360) + 360) % 360) - 180, p2 / DEG];
  }

  /// The terminator as a great circle: by definition every point 90° from the
  /// sun. Sampled by azimuth from the antisolar point, then split wherever it
  /// crosses the antimeridian, since one unbroken ring would draw a streak
  /// straight across the map.
  function terminatorSegments(date) {
    const { lat, lon } = subsolar(date);
    const antiLon = ((((lon + 180 + 180) % 360) + 360) % 360) - 180;
    const segs = [];
    let cur = [];
    let prevLon = null;
    const steps = 360;
    for (let i = 0; i <= steps; i++) {
      const p = destination(-lat, antiLon, (i * 360) / steps, 90 * 111194.9);
      if (prevLon !== null && Math.abs(p[0] - prevLon) > 180) {
        if (cur.length > 1) segs.push(cur);
        cur = [];
      }
      cur.push(p);
      prevLon = p[0];
    }
    if (cur.length > 1) segs.push(cur);
    return segs;
  }

  /// All four bands plus the terminator, in one collection: the fills carry
  /// their own colour and opacity, so a single layer draws the gradient.
  function daylightFC(date) {
    const { lat: dec, lon: subLon } = subsolar(date);
    const lons = [];
    for (let lon = -180; lon <= 180; lon += 1.5) lons.push(lon);
    const feats = [];

    for (const band of BANDS) {
      const ivs = lons.map((lon) => bandInterval(lon, dec, subLon, band.h));
      // Group contiguous meridians into rings. A band may touch the map edge on
      // both sides of the antimeridian; that is two pieces, and drawing it as
      // two pieces is correct.
      let run = [];
      const flush = () => {
        if (run.length >= 2) {
          const coords = [];
          for (const i of run) coords.push([lons[i], ivs[i][0]]);
          for (let k = run.length - 1; k >= 0; k--) coords.push([lons[run[k]], ivs[run[k]][1]]);
          coords.push(coords[0]);
          feats.push({
            type: 'Feature',
            geometry: { type: 'Polygon', coordinates: [coords] },
            properties: { col: band.col, op: band.op, name: band.name, h: band.h },
          });
        }
        run = [];
      };
      for (let i = 0; i < lons.length; i++) {
        if (ivs[i]) run.push(i);
        else flush();
      }
      flush();
    }

    for (const seg of terminatorSegments(date)) {
      feats.push({
        type: 'Feature',
        geometry: { type: 'LineString', coordinates: seg },
        properties: { name: 'terminator' },
      });
    }
    return { type: 'FeatureCollection', features: feats };
  }

  /// One band's latitude interval on one meridian (exposed for self-checks).
  function bandIntervalAt(lon, date, h0) {
    const { lat: dec, lon: subLon } = subsolar(date);
    return bandInterval(lon, dec, subLon, h0);
  }

  /// One meridian per whole solar hour. Political zones differ; this is the
  /// geometry of the sun, not a timezone database.
  function meridiansFC() {
    const feats = [];
    for (let k = -11; k <= 11; k++) {
      const lon = k * 15;
      const label = k === 0 ? 'UTC' : `UTC${k > 0 ? '+' : ''}${k}`;
      feats.push({
        type: 'Feature',
        geometry: {
          type: 'LineString',
          coordinates: [
            [lon, -MAX_LAT],
            [lon, MAX_LAT],
          ],
        },
        properties: { label, k },
      });
    }
    return { type: 'FeatureCollection', features: feats };
  }

  /// The sun's own position, plus the meridian where it is solar noon.
  function sunFC(date) {
    const { lat, lon } = subsolar(date);
    const latC = Math.max(-MAX_LAT, Math.min(MAX_LAT, lat));
    return {
      type: 'FeatureCollection',
      features: [
        { type: 'Feature', geometry: { type: 'Point', coordinates: [lon, latC] }, properties: {} },
        {
          type: 'Feature',
          geometry: {
            type: 'LineString',
            coordinates: [
              [lon, latC],
              [lon, -MAX_LAT],
            ],
          },
          properties: {},
        },
      ],
    };
  }

  /* ----------------------------------------------------------------- wiring */

  const EMPTY = { type: 'FeatureCollection', features: [] };

  function attach(m, opts) {
    if (!m) return;
    map = m;
    beforeId = (opts && opts.before) || null;
    const add = (layer) => map.addLayer(layer, beforeId || undefined);

    map.addSource('day-night', { type: 'geojson', data: EMPTY });
    map.addSource('day-meridians', { type: 'geojson', data: EMPTY });
    map.addSource('day-sun', { type: 'geojson', data: EMPTY });

    add({
      id: 'day-night',
      type: 'fill',
      source: 'day-night',
      filter: ['==', ['geometry-type'], 'Polygon'],
      paint: {
        'fill-color': ['get', 'col'],
        'fill-opacity': ['get', 'op'],
        'fill-antialias': true,
      },
    });
    add({
      id: 'day-terminator',
      type: 'line',
      source: 'day-night',
      filter: ['==', ['geometry-type'], 'LineString'],
      paint: {
        'line-color': SUN_COLOR,
        'line-width': ['interpolate', ['linear'], ['zoom'], 1, 1, 6, 1.7],
        'line-opacity': 0.8,
      },
    });
    add({
      id: 'day-meridians',
      type: 'line',
      source: 'day-meridians',
      paint: {
        'line-color': 'rgba(148,163,184,.3)',
        'line-width': 0.7,
        'line-dasharray': [2, 4],
      },
    });
    add({
      id: 'day-meridians-label',
      type: 'symbol',
      source: 'day-meridians',
      layout: {
        'text-font': ['Open Sans Regular'],
        'text-field': ['get', 'label'],
        'text-size': 9,
        'symbol-placement': 'line',
        'symbol-spacing': 520,
      },
      paint: {
        'text-color': 'rgba(203,213,225,.7)',
        'text-halo-color': 'rgba(3,6,11,.85)',
        'text-halo-width': 1.4,
      },
    });
    add({
      id: 'day-sun-line',
      type: 'line',
      source: 'day-sun',
      filter: ['==', ['geometry-type'], 'LineString'],
      paint: {
        'line-color': SUN_COLOR,
        'line-width': 1,
        'line-opacity': 0.35,
        'line-dasharray': [3, 3],
      },
    });
    add({
      id: 'day-sun-dot',
      type: 'circle',
      source: 'day-sun',
      filter: ['==', ['geometry-type'], 'Point'],
      paint: {
        'circle-radius': 4,
        'circle-color': 'rgba(250,204,21,.85)',
        'circle-stroke-color': 'rgba(3,6,11,.8)',
        'circle-stroke-width': 1,
      },
    });

    applyVisibility();
    update();
    if (timer) clearInterval(timer);
    // the terminator moves ~15°/hour: a minute is plenty, and it costs nothing
    timer = setInterval(update, 60000);
  }

  function update() {
    if (!map || !map.getSource('day-night')) return;
    const now = new Date();
    const set = (id, data) => {
      const src = map.getSource(id);
      if (src) src.setData(data);
    };
    set('day-night', daylightFC(now));
    set('day-meridians', meridiansFC());
    set('day-sun', sunFC(now));
  }

  function setVisible(v) {
    if (v) Object.assign(visible, v);
    applyVisibility();
  }

  function applyVisibility() {
    if (!map) return;
    const v = (id, on) => {
      if (map.getLayer(id)) map.setLayoutProperty(id, 'visibility', on ? 'visible' : 'none');
    };
    v('day-night', visible.night);
    v('day-terminator', visible.night);
    v('day-sun-line', visible.night);
    v('day-sun-dot', visible.night);
    v('day-meridians', visible.meridians);
    v('day-meridians-label', visible.meridians);
  }

  /// Local mean solar time at a longitude, and the solar hour zone it sits in.
  function localSolar(date, lon) {
    const utcMinutes = date.getUTCHours() * 60 + date.getUTCMinutes();
    const solar = (((utcMinutes + lon * 4) % 1440) + 1440) % 1440;
    const hh = String(Math.floor(solar / 60)).padStart(2, '0');
    const mm = String(Math.floor(solar % 60)).padStart(2, '0');
    const zone = Math.round(lon / 15);
    return { time: `${hh}:${mm}`, zone: zone === 0 ? 'UTC' : `UTC${zone > 0 ? '+' : ''}${zone}` };
  }

  return {
    attach,
    update,
    setVisible,
    subsolar,
    solarElevation,
    bandInterval: bandIntervalAt,
    terminatorSegments,
    daylightFC,
    meridiansFC,
    localSolar,
    BANDS,
  };
})();
