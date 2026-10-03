//! Land mask used by the simulator so demo traffic stays on water.
//!
//! Data: Natural Earth 1:50m land polygons (`assets/land-50m.geojson`),
//! public domain. 110m is too coarse here — it closes the Strait of Gibraltar,
//! which is exactly where the default scenario runs. 50m keeps it open.
//!
//! Lookup is a bounding-box prefilter over 1400-odd rings, then a ray cast;
//! the cost is negligible at the simulator's one-position-per-second cadence.

use std::sync::OnceLock;

use serde_json::Value;

use crate::geo;

struct Ring {
    bbox: [f64; 4], // west, south, east, north
    points: Vec<[f64; 2]>, // [lon, lat] as in GeoJSON
}

pub struct LandMask {
    rings: Vec<Ring>,
    loaded: bool,
}

static MASK: OnceLock<LandMask> = OnceLock::new();

pub fn get() -> &'static LandMask {
    MASK.get_or_init(|| LandMask::load(include_str!("../assets/land-50m.geojson")))
}

impl LandMask {
    fn load(text: &str) -> Self {
        let mut rings = Vec::new();
        let parsed: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("land mask could not be parsed ({e}); vessels may cross land");
                return LandMask {
                    rings,
                    loaded: false,
                };
            }
        };
        if let Some(features) = parsed.get("features").and_then(|f| f.as_array()) {
            for f in features {
                let g = f.get("geometry").unwrap_or(&Value::Null);
                match g.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "Polygon" => {
                        if let Some(ring) = g.pointer("/coordinates/0").and_then(|c| c.as_array()) {
                            push_ring(&mut rings, ring);
                        }
                    }
                    "MultiPolygon" => {
                        if let Some(polys) = g.get("coordinates").and_then(|c| c.as_array()) {
                            for p in polys {
                                if let Some(ring) = p.get(0).and_then(|c| c.as_array()) {
                                    push_ring(&mut rings, ring);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let loaded = !rings.is_empty();
        if !loaded {
            tracing::warn!("land mask is empty; vessels may cross land");
        }
        LandMask { rings, loaded }
    }

    /// Is this position on land (or in a lake/closed water the mask treats as land)?
    /// A missing or broken mask reports "water" so the simulator still runs.
    pub fn on_land(&self, lat: f64, lon: f64) -> bool {
        for r in &self.rings {
            if lon < r.bbox[0] || lon > r.bbox[2] || lat < r.bbox[1] || lat > r.bbox[3] {
                continue;
            }
            if geo::point_in_poly(lat, lon, &r.points) {
                return true;
            }
        }
        false
    }

    pub fn ring_count(&self) -> usize {
        self.rings.len()
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }
}

fn push_ring(rings: &mut Vec<Ring>, ring: &[Value]) {
    let mut points = Vec::with_capacity(ring.len());
    let mut bbox = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for p in ring {
        let (Some(lon), Some(lat)) = (
            p.get(0).and_then(|v| v.as_f64()),
            p.get(1).and_then(|v| v.as_f64()),
        ) else {
            continue;
        };
        points.push([lon, lat]);
        bbox[0] = bbox[0].min(lon);
        bbox[1] = bbox[1].min(lat);
        bbox[2] = bbox[2].max(lon);
        bbox[3] = bbox[3].max(lat);
    }
    if points.len() >= 3 {
        rings.push(Ring { bbox, points });
    }
}

pub fn on_land(lat: f64, lon: f64) -> bool {
    get().on_land(lat, lon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_loads_and_classifies_known_points() {
        let m = get();
        assert!(m.is_loaded(), "land mask failed to load");
        assert!(m.ring_count() > 100, "unexpectedly few rings: {}", m.ring_count());

        // the default scenario area must be water, or the demo is impossible
        assert!(!m.on_land(36.00, -5.40), "Strait of Gibraltar must be water");
        assert!(!m.on_land(36.04, -5.36), "own-ship start position must be water");
        assert!(!m.on_land(35.95, -4.00), "Alboran Sea must be water");
        assert!(!m.on_land(30.00, -40.00), "mid-Atlantic must be water");

        // and land must be land
        assert!(m.on_land(40.40, -3.70), "Madrid must be land");
        assert!(m.on_land(48.85, 2.35), "Paris must be land");
    }
}
