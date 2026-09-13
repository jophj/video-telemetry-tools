//! Small numeric helpers shared by the telemetry, merge and moments code.
//!
//! Every one of these has a counterpart in the prototypes. Where a prototype's
//! choice was load-bearing — a window shape, a percentile estimator, an Earth
//! radius — it is reproduced exactly and the reason is noted, because the
//! constants were tuned against real footage.

/// IUGG mean Earth radius.
///
/// The prototypes used three different values: 6371000 with an equirectangular
/// approximation, 6371000 with haversine, and 6371008.8 with haversine. `gmax`
/// uses this one everywhere so two distances of the same track always agree.
pub const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// Standard gravity, as used to express accelerations in g.
pub const G: f64 = 9.806_65;

pub type Vec3 = [f64; 3];

pub fn dot(a: &Vec3, b: &Vec3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub fn cross(a: &Vec3, b: &Vec3) -> Vec3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub fn magnitude(v: &Vec3) -> f64 {
    dot(v, v).sqrt()
}

/// Normalise, treating a zero vector as unit-X rather than producing NaNs.
pub fn normalize(v: &Vec3) -> Vec3 {
    let m = magnitude(v);
    if m == 0.0 || !m.is_finite() {
        return [1.0, 0.0, 0.0];
    }
    [v[0] / m, v[1] / m, v[2] / m]
}

pub fn scale(v: &Vec3, k: f64) -> Vec3 {
    [v[0] * k, v[1] * k, v[2] * k]
}

pub fn sub(a: &Vec3, b: &Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// Remove the component of `v` along the unit vector `axis`.
pub fn reject(v: &Vec3, axis: &Vec3) -> Vec3 {
    let d = dot(v, axis);
    sub(v, &scale(axis, d))
}

/// Rotate `v` by `angle` radians about the unit vector `axis`, right-handed.
///
/// Rodrigues' formula. Written out rather than built from a matrix because
/// there is only ever one axis in play and the matrix would be assembled,
/// used once per sample and thrown away.
pub fn rotate_about(v: &Vec3, axis: &Vec3, angle: f64) -> Vec3 {
    let (s, c) = angle.sin_cos();
    let k = cross(axis, v);
    let d = dot(axis, v) * (1.0 - c);
    [
        v[0] * c + k[0] * s + axis[0] * d,
        v[1] * c + k[1] * s + axis[1] * d,
        v[2] * c + k[2] * s + axis[2] * d,
    ]
}

/// Component-wise median of a set of vectors.
///
/// Not the geometric median, and deliberately: this is used to average a
/// cloud of small rotation vectors, where the point is to be unmoved by the
/// outliers that a motorcycle's accelerometer produces in quantity.
pub fn median_vec(vs: &[Vec3]) -> Vec3 {
    let axis = |k: usize| median(&vs.iter().map(|v| v[k]).collect::<Vec<_>>());
    [axis(0), axis(1), axis(2)]
}

/// Centred box filter of width `w`, clipped at the edges.
///
/// Matches the prototype's `_movavg`: the window is `[i - w/2, i + w/2]`
/// inclusive of the extra sample on the right for even widths, and shortens
/// rather than reflecting at the ends. Computed from a prefix sum, so width
/// costs nothing.
pub fn moving_average(x: &[f64], w: usize) -> Vec<f64> {
    if w < 2 || x.is_empty() {
        return x.to_vec();
    }
    let n = x.len();
    let h = w / 2;
    let mut prefix = Vec::with_capacity(n + 1);
    prefix.push(0.0);
    for &v in x {
        prefix.push(prefix[prefix.len() - 1] + v);
    }
    (0..n)
        .map(|i| {
            let hi = (i + h + 1).min(n);
            let lo = i.saturating_sub(h);
            (prefix[hi] - prefix[lo]) / (hi - lo) as f64
        })
        .collect()
}

/// `+1` when the two series move together, `-1` when they oppose.
///
/// A raw dot product, not a Pearson correlation: the prototype's `_corr_sign`,
/// kept because the polarity decisions were validated with it.
pub fn correlation_sign(a: &[f64], b: &[f64]) -> f64 {
    let s: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    if s < 0.0 {
        -1.0
    } else {
        1.0
    }
}

/// Pearson correlation coefficient; 0 when either series is constant.
pub fn pearson(xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len().min(ys.len());
    if n == 0 {
        return 0.0;
    }
    let nf = n as f64;
    let mx = xs[..n].iter().sum::<f64>() / nf;
    let my = ys[..n].iter().sum::<f64>() / nf;
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    let mut syy = 0.0;
    for i in 0..n {
        let dx = xs[i] - mx;
        let dy = ys[i] - my;
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    if sxx <= 0.0 || syy <= 0.0 {
        return 0.0;
    }
    sxy / (sxx.sqrt() * syy.sqrt())
}

/// Linear-interpolated quantile (the "type 7" estimator) of an ascending slice.
///
/// This is the estimator `motion-record-analysis` uses for its cadence
/// statistics, and cadence drives every window in the moments engine, so the
/// choice has to match rather than merely be reasonable.
pub fn quantile_sorted(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let pos = (sorted.len() - 1) as f64 * p;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

pub fn median(values: &[f64]) -> f64 {
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    quantile_sorted(&v, 0.5)
}

/// Great-circle distance in metres.
pub fn haversine(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.sqrt().min(1.0).asin()
}

/// Initial bearing from point 1 to point 2, in degrees clockwise from north.
pub fn bearing(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dl = (lon2 - lon1).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.sin().mul_add(-p2.cos() * dl.cos(), p1.cos() * p2.sin());
    y.atan2(x).to_degrees().rem_euclid(360.0)
}

/// Signed shortest-arc difference `b - a` for two bearings, in `(-180, 180]`.
///
/// Used wherever a course *rate* is needed: a plain subtraction reports −359°
/// for a one-degree turn through north, which would invert the sign of every
/// heading change near due north.
pub fn angle_delta(a: f64, b: f64) -> f64 {
    let mut d = (b - a) % 360.0;
    if d > 180.0 {
        d -= 360.0;
    }
    if d <= -180.0 {
        d += 360.0;
    }
    d
}

/// Mean of a set of angles in degrees, via unit vectors.
///
/// `None` when the vectors cancel, which is the honest answer for a set with no
/// mean direction.
pub fn circular_mean(degrees: &[f64]) -> Option<f64> {
    let mut x = 0.0;
    let mut y = 0.0;
    for &d in degrees {
        let r = d.to_radians();
        x += r.cos();
        y += r.sin();
    }
    if x.abs() < 1e-12 && y.abs() < 1e-12 {
        return None;
    }
    Some(y.atan2(x).to_degrees().rem_euclid(360.0))
}

/// Interpolate along the great circle between two points.
///
/// `f` is the fraction from `a` to `b`. Used by the GPS merge: a linear
/// interpolation of latitude and longitude is close enough over one second at
/// road speed, but slerp costs nothing and cannot go wrong at high latitude or
/// across the antimeridian.
pub fn interpolate_great_circle(
    lat1: f64,
    lon1: f64,
    lat2: f64,
    lon2: f64,
    f: f64,
) -> (f64, f64) {
    let (p1, l1) = (lat1.to_radians(), lon1.to_radians());
    let (p2, l2) = (lat2.to_radians(), lon2.to_radians());
    let d = 2.0
        * (((p2 - p1) / 2.0).sin().powi(2)
            + p1.cos() * p2.cos() * ((l2 - l1) / 2.0).sin().powi(2))
        .sqrt()
        .min(1.0)
        .asin();
    if d.abs() < 1e-12 {
        return (lat1, lon1);
    }
    let a = ((1.0 - f) * d).sin() / d.sin();
    let b = (f * d).sin() / d.sin();
    let x = a * p1.cos() * l1.cos() + b * p2.cos() * l2.cos();
    let y = a * p1.cos() * l1.sin() + b * p2.cos() * l2.sin();
    let z = a * p1.sin() + b * p2.sin();
    let lat = z.atan2((x * x + y * y).sqrt()).to_degrees();
    let lon = y.atan2(x).to_degrees();
    (lat, lon)
}

/// Dominant eigenvector of a symmetric 3x3 matrix, by power iteration.
///
/// The prototype runs a fixed 300 iterations from `[1, 0, 0]`; this keeps that
/// as the cap but stops once the direction settles, which is typically well
/// under twenty steps and makes the result independent of the cap.
pub fn principal_axis(m: &[[f64; 3]; 3]) -> Vec3 {
    let mut v: Vec3 = [1.0, 0.0, 0.0];
    for _ in 0..300 {
        let next = [
            m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
            m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
            m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
        ];
        if magnitude(&next) == 0.0 {
            break;
        }
        let n = normalize(&next);
        // 1 - |cos| between successive iterates; converged when the axis stops
        // turning.
        if 1.0 - dot(&n, &v).abs() < 1e-14 {
            v = n;
            break;
        }
        v = n;
    }
    v
}

/// Covariance matrix of a set of deviations from a reference direction.
pub fn covariance(deviations: &[Vec3]) -> [[f64; 3]; 3] {
    let mut c = [[0.0f64; 3]; 3];
    if deviations.is_empty() {
        return c;
    }
    for d in deviations {
        for i in 0..3 {
            for j in 0..3 {
                c[i][j] += d[i] * d[j];
            }
        }
    }
    let n = deviations.len() as f64;
    for row in c.iter_mut() {
        for v in row.iter_mut() {
            *v /= n;
        }
    }
    c
}

/// Format a float the way JavaScript's `round`-then-stringify does.
///
/// `motion-record-analysis` rounds to a fixed number of places and then lets JS
/// stringify the resulting Number, so trailing zeros disappear (`55`, not
/// `55.0`; `<ele>918</ele>`, not `918.0`) and negative zero prints as `0`.
/// Reproducing that is the difference between matching its GPX output and
/// merely resembling it.
pub fn fmt_num(v: f64, dp: usize) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    let factor = 10f64.powi(dp as i32);
    let mut r = (v * factor).round() / factor;
    if r == 0.0 {
        r = 0.0; // collapse -0.0
    }
    let mut s = format!("{r:.*}", dp);
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if s == "-0" {
        s = "0".to_string();
    }
    s
}
