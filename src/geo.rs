//! Small geodesy helpers. Spherical earth is plenty for surface traffic.

const R_M: f64 = 6_371_008.8;

pub fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * R_M * a.sqrt().asin()
}

/// Compute the destination point given a start, bearing (deg true) and distance.
pub fn destination_point(lat: f64, lon: f64, bearing_deg: f64, dist_m: f64) -> (f64, f64) {
    let d = dist_m / R_M;
    let br = bearing_deg.to_radians();
    let p1 = lat.to_radians();
    let l1 = lon.to_radians();
    let p2 = (p1.sin() * d.cos() + p1.cos() * d.sin() * br.cos()).asin();
    let l2 = l1 + (br.sin() * d.sin() * p1.cos()).atan2(d.cos() - p1.sin() * p2.sin());
    let lon_out = (l2.to_degrees() + 540.0) % 360.0 - 180.0;
    (p2.to_degrees(), lon_out)
}

/// Initial bearing from point 1 to point 2, degrees true in [0, 360).
pub fn bearing_deg(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let p1 = lat1.to_radians();
    let p2 = lat2.to_radians();
    let dl = (lon2 - lon1).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    (y.atan2(x).to_degrees() + 360.0) % 360.0
}

/// Ray-casting point-in-polygon. Polygon vertices are [lon, lat] (GeoJSON order).
pub fn point_in_poly(lat: f64, lon: f64, poly: &[[f64; 2]]) -> bool {
    if poly.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = (poly[i][0], poly[i][1]);
        let (xj, yj) = (poly[j][0], poly[j][1]);
        // (yj - yi) cannot be zero here: the sign test above guarantees it.
        let intersect =
            ((yi > lat) != (yj > lat)) && (lon < (xj - xi) * (lat - yi) / (yj - yi) + xi);
        if intersect {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_known_distance() {
        // Gibraltar to Ceuta is roughly 30 km.
        let d = haversine_m(36.02, -5.36, 35.89, -5.32);
        assert!((d - 14_500.0).abs() < 2_000.0, "got {d}");
    }

    #[test]
    fn destination_round_trip() {
        let (lat, lon) = destination_point(36.0, -5.0, 45.0, 5_000.0);
        let d = haversine_m(36.0, -5.0, lat, lon);
        assert!((d - 5_000.0).abs() < 5.0, "got {d}");
        let brg = bearing_deg(36.0, -5.0, lat, lon);
        assert!((brg - 45.0).abs() < 0.5, "got {brg}");
    }

    #[test]
    fn polygon_contains() {
        let square = [[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]];
        assert!(point_in_poly(1.0, 1.0, &square));
        assert!(!point_in_poly(3.0, 1.0, &square));
    }
}
