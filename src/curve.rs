//! Fan curves: `[temp °C, percent]` points, piecewise linear.

pub type Point = [f64; 2];
pub type Curve = Vec<Point>;

/// Piecewise-linear interpolation, flat beyond both ends.
pub fn interp(points: &[Point], x: f64) -> f64 {
    let Some(first) = points.first() else { return 0.0 };
    if x <= first[0] {
        return first[1];
    }
    for w in points.windows(2) {
        let ([x0, y0], [x1, y1]) = (w[0], w[1]);
        if x <= x1 {
            return if x1 == x0 { y1 } else { y0 + (y1 - y0) * (x - x0) / (x1 - x0) };
        }
    }
    points[points.len() - 1][1]
}

/// Clean and validate a user-supplied curve.
///
/// 1-8 points, temps 0-110 strictly increasing, percent 0-100 and
/// non-decreasing: a curve that slows fans as things heat up is a mistake.
pub fn validate(points: &[Point]) -> Result<Curve, String> {
    if points.is_empty() || points.len() > 8 {
        return Err("curve needs 1-8 points".into());
    }
    let mut clean = Vec::with_capacity(points.len());
    for &[t, p] in points {
        if !(0.0..=110.0).contains(&t) || !(0.0..=100.0).contains(&p) {
            return Err("temp must be 0-110 and percent 0-100".into());
        }
        clean.push([(t * 10.0).round() / 10.0, (p * 10.0).round() / 10.0]);
    }
    for w in clean.windows(2) {
        if w[1][0] <= w[0][0] {
            return Err("temperatures must increase".into());
        }
        if w[1][1] < w[0][1] {
            return Err("percent must not decrease as temperature rises".into());
        }
    }
    Ok(clean)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_and_clamps() {
        let pts = [[40.0, 10.0], [60.0, 30.0], [80.0, 100.0]];
        assert_eq!(interp(&pts, 20.0), 10.0);
        assert_eq!(interp(&pts, 50.0), 20.0);
        assert_eq!(interp(&pts, 90.0), 100.0);
        assert_eq!(interp(&[], 50.0), 0.0);
    }

    #[test]
    fn validates() {
        assert_eq!(validate(&[[40.0, 10.0], [80.0, 100.0]]).unwrap(), vec![[40.0, 10.0], [80.0, 100.0]]);
        for bad in [
            vec![],
            vec![[50.0, 10.0], [40.0, 20.0]],
            vec![[40.0, 50.0], [60.0, 20.0]],
            vec![[40.0, 120.0]],
            vec![[0.0, 0.0]; 9],
        ] {
            assert!(validate(&bad).is_err(), "{bad:?}");
        }
    }
}
