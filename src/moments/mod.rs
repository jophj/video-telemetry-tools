//! Finding the interesting moments in a ride.
//!
//! Ported from the `motion-record-analysis` prototype, whose central
//! discipline is that **sample cadence is measured at runtime and never
//! assumed**. That is what lets one implementation serve both a phone export at
//! 1 Hz and camera telemetry at 29.97 Hz with no configuration: every window
//! here is expressed in seconds, every derivative divides by the actual
//! interval between the two samples involved, and nothing uses a sample count
//! as a proxy for elapsed time.
//!
//! The consequence worth stating plainly: a reported maximum is the maximum
//! *of the samples present*. At 1 Hz a corner's true peak lean almost certainly
//! fell between two samples, so the figure understates it. The report says so.

pub mod clips;
pub mod output;

use crate::math::*;

/// Physically impossible longitudinal acceleration, in m/s².
///
/// About 1.5 g. This rejects only nonsense: the reference phone ride peaks at
/// 7.5 m/s² accelerating and 8.2 braking, both well inside what a motorcycle
/// can do, and neither should be filtered away.
pub const ACCEL_CEILING_MS2: f64 = 15.0;

/// One sample of a ride, from whichever source.
#[derive(Debug, Clone, Default)]
pub struct Sample {
    /// Epoch seconds, fractional.
    pub t: f64,
    /// Where this happened, when that is known.
    ///
    /// Optional because "we know how far the bike was leaned but not where it
    /// was" is a real and common state: on the reference clip the camera's
    /// receiver has no usable fix for 45 % of the ride, while its IMU works
    /// throughout. Carrying the receiver's garbage coordinates instead would
    /// put them into every distance and every reported location -- summing
    /// them naively gives 3470 km for a 25-minute ride.
    pub pos: Option<(f64, f64)>,
    pub ele_m: Option<f64>,
    /// Metres per second.
    pub speed_ms: Option<f64>,
    /// Degrees, signed, positive to the right.
    pub lean_deg: Option<f64>,
    /// Longitudinal acceleration in g, from a vehicle-frame IMU.
    pub accel_lon_g: Option<f64>,
    /// Lateral acceleration in g, positive to the right.
    pub accel_lat_g: Option<f64>,
    /// Total acceleration magnitude in g, 1.0 at rest.
    pub accel_total_g: Option<f64>,
    /// Seconds into the video, where the source is a recording.
    pub video_time_s: Option<f64>,
}

/// Where longitudinal acceleration should come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccelSource {
    /// Differentiate GPS speed.
    ///
    /// The default, and the only option for a source with no vehicle-frame
    /// orientation. A phone in a tank bag measures its own attitude, not the
    /// bike's, so its raw accelerometer correlates with real speed change at
    /// only r = 0.16–0.20 and must not feed a reported figure.
    #[default]
    Gps,
    /// Use the IMU's own longitudinal channel.
    ///
    /// Legitimate for camera telemetry, where the 200 Hz accelerometer has been
    /// rotated into the vehicle frame, and far better resolved than 10 Hz GPS
    /// speed differencing.
    Imu,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub top: usize,
    /// Minimum separation between ranked moments, in seconds.
    pub min_gap_s: f64,
    /// Lean angle beyond which an excursion counts as a curve.
    pub curve_threshold_deg: f64,
    /// Length of the curve-density window, in seconds.
    pub window_s: f64,
    /// Multiple of the median interval above which a gap breaks derivation.
    pub max_gap_factor: f64,
    pub accel_ceiling_ms2: f64,
    pub accel_source: AccelSource,
    /// Which metrics to compute, in report order.
    pub metrics: Vec<MetricId>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            top: 10,
            min_gap_s: 5.0,
            curve_threshold_deg: 15.0,
            window_s: 10.0,
            max_gap_factor: 5.0,
            accel_ceiling_ms2: ACCEL_CEILING_MS2,
            accel_source: AccelSource::Gps,
            metrics: MetricId::ALL.to_vec(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricId {
    Lean,
    Accel,
    Decel,
    Speed,
    Curves,
    Lateral,
    Total,
}

impl MetricId {
    pub const ALL: &'static [MetricId] = &[
        MetricId::Lean,
        MetricId::Accel,
        MetricId::Decel,
        MetricId::Speed,
        MetricId::Curves,
        MetricId::Lateral,
        MetricId::Total,
    ];

    pub fn slug(self) -> &'static str {
        match self {
            MetricId::Lean => "lean",
            MetricId::Accel => "accel",
            MetricId::Decel => "decel",
            MetricId::Speed => "speed",
            MetricId::Curves => "curves",
            MetricId::Lateral => "lateral",
            MetricId::Total => "total",
        }
    }

    pub fn parse(s: &str) -> Option<MetricId> {
        MetricId::ALL.iter().copied().find(|m| m.slug() == s)
    }

    pub fn title(self) -> &'static str {
        match self {
            MetricId::Lean => "Max lean angle",
            MetricId::Accel => "Max acceleration",
            MetricId::Decel => "Max deceleration",
            MetricId::Speed => "Max speed",
            MetricId::Curves => "Curviest windows",
            MetricId::Lateral => "Max cornering load",
            MetricId::Total => "Max combined load",
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            MetricId::Lean => "deg",
            MetricId::Accel | MetricId::Decel => "m/s^2",
            MetricId::Speed => "km/h",
            MetricId::Curves => "curves",
            MetricId::Lateral | MetricId::Total => "g",
        }
    }

    /// Ordering used when several moments merge into one clip: the lower
    /// number names the clip.
    pub fn priority(self) -> usize {
        match self {
            MetricId::Lean => 0,
            MetricId::Curves => 1,
            MetricId::Decel => 2,
            MetricId::Accel => 3,
            MetricId::Speed => 4,
            MetricId::Lateral => 5,
            MetricId::Total => 6,
        }
    }
}

/// A break in the sample stream.
#[derive(Debug, Clone, Copy)]
pub struct Gap {
    pub index: usize,
    pub after_t: f64,
    pub dt: f64,
}

/// Measured sampling behaviour.
#[derive(Debug, Clone)]
pub struct Cadence {
    pub sample_count: usize,
    pub median_dt: f64,
    pub hz: Option<f64>,
    pub p5_dt: f64,
    pub p95_dt: f64,
    pub min_dt: f64,
    pub max_dt: f64,
    /// Intervals longer than this break a derivative rather than producing a
    /// false spike.
    pub gap_threshold: f64,
    pub gaps: Vec<Gap>,
    /// Zero or negative intervals, which are skipped.
    pub non_positive: usize,
    pub regular: bool,
    pub duration_s: f64,
}

/// Measure cadence from the timestamps.
///
/// The median interval, not the mean, so that a long gap in the middle of a
/// ride does not move the estimate; and p5/p95 rather than min/max so that
/// "regular" is a statement about the bulk of the data.
pub fn derive_cadence(samples: &[Sample], max_gap_factor: f64) -> Cadence {
    let n = samples.len();
    if n < 2 {
        return Cadence {
            sample_count: n,
            median_dt: f64::NAN,
            hz: None,
            p5_dt: f64::NAN,
            p95_dt: f64::NAN,
            min_dt: f64::NAN,
            max_dt: f64::NAN,
            gap_threshold: f64::NAN,
            gaps: Vec::new(),
            non_positive: 0,
            regular: false,
            duration_s: 0.0,
        };
    }

    let mut deltas = Vec::with_capacity(n - 1);
    let mut non_positive = 0usize;
    for w in samples.windows(2) {
        let dt = w[1].t - w[0].t;
        if dt > 0.0 {
            deltas.push(dt);
        } else {
            non_positive += 1;
        }
    }
    deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let median_dt = quantile_sorted(&deltas, 0.5);
    let gap_threshold = median_dt * max_gap_factor;
    let p5_dt = quantile_sorted(&deltas, 0.05);
    let p95_dt = quantile_sorted(&deltas, 0.95);

    let mut gaps = Vec::new();
    for (i, w) in samples.windows(2).enumerate() {
        let dt = w[1].t - w[0].t;
        if dt > gap_threshold {
            gaps.push(Gap {
                index: i + 1,
                after_t: w[0].t,
                dt,
            });
        }
    }

    Cadence {
        sample_count: n,
        median_dt,
        hz: (median_dt > 0.0).then(|| 1.0 / median_dt),
        p5_dt,
        p95_dt,
        min_dt: deltas.first().copied().unwrap_or(f64::NAN),
        max_dt: deltas.last().copied().unwrap_or(f64::NAN),
        gap_threshold,
        gaps,
        non_positive,
        // The central 90% of intervals staying within 20% of the median.
        regular: median_dt > 0.0 && (p95_dt - p5_dt) / median_dt < 0.2,
        duration_s: samples[n - 1].t - samples[0].t,
    }
}

/// Longitudinal acceleration in m/s², index-aligned with `samples`.
///
/// A plain backward difference of GPS speed over the actual interval,
/// attributed to the later sample. Deliberately unsmoothed: smoothing would
/// blunt exactly the peaks this is looking for.
pub fn derive_acceleration_from_gps(
    samples: &[Sample],
    gap_threshold: f64,
    ceiling: f64,
) -> Vec<Option<f64>> {
    let mut out = vec![None; samples.len()];
    for i in 1..samples.len() {
        let (prev, cur) = (&samples[i - 1], &samples[i]);
        let (Some(a), Some(b)) = (prev.speed_ms, cur.speed_ms) else {
            continue;
        };
        let dt = cur.t - prev.t;
        if dt <= 0.0 {
            continue;
        }
        // Across a gap the bike accelerated somewhere unobserved, and the
        // difference over the whole gap is not an acceleration.
        if gap_threshold.is_finite() && dt > gap_threshold {
            continue;
        }
        let accel = (b - a) / dt;
        if accel.abs() > ceiling {
            continue;
        }
        out[i] = Some(accel);
    }
    out
}

/// A sensor's clipping limit, discovered rather than hardcoded.
#[derive(Debug, Clone, Copy)]
pub struct SensorLimit {
    pub value: f64,
    pub samples: usize,
}

/// Find the value at which a source's lean angle is clipped.
///
/// A smooth analogue signal essentially never repeats its exact extreme value;
/// a clipped one pins there. So a maximum `|lean|` recurring on two or more
/// samples is taken to be the limit. The reference phone export clips at
/// exactly ±55.0° on three samples, but nothing here knows the number 55.
pub fn detect_lean_limit(samples: &[Sample]) -> Option<SensorLimit> {
    let mut max = f64::NEG_INFINITY;
    let mut count = 0usize;
    for s in samples {
        let Some(lean) = s.lean_deg else { continue };
        let a = lean.abs();
        if a > max {
            max = a;
            count = 1;
        } else if a == max {
            count += 1;
        }
    }
    (max.is_finite() && count >= 2).then_some(SensorLimit {
        value: max,
        samples: count,
    })
}

/// One detected corner, timed and located at its peak.
#[derive(Debug, Clone)]
pub struct Curve {
    pub t: f64,
    pub lean_deg: f64,
    pub index: usize,
}

/// Find the corners.
///
/// A curve is a lean excursion that peaks beyond `threshold` before lean
/// returns towards neutral or reverses. The neutral band is `threshold / 2`:
/// without that hysteresis, lean wobbling either side of the threshold
/// mid-corner would be counted as several corners.
///
/// Purely sequential, with no dependence on sample spacing, which is what makes
/// the count cadence-independent.
pub fn detect_curves(samples: &[Sample], threshold: f64) -> Vec<Curve> {
    let neutral = threshold / 2.0;
    let mut curves = Vec::new();
    let mut sign = 0i8;
    let mut peak: Option<(usize, f64)> = None;

    let mut close = |sign: &mut i8, peak: &mut Option<(usize, f64)>, curves: &mut Vec<Curve>| {
        if let Some((i, lean)) = *peak {
            if lean.abs() >= threshold {
                curves.push(Curve {
                    t: samples[i].t,
                    lean_deg: lean,
                    index: i,
                });
            }
        }
        *sign = 0;
        *peak = None;
    };

    for (i, s) in samples.iter().enumerate() {
        let Some(lean) = s.lean_deg else { continue };
        let abs = lean.abs();
        if abs < neutral {
            close(&mut sign, &mut peak, &mut curves);
            continue;
        }
        let sg = if lean > 0.0 {
            1
        } else if lean < 0.0 {
            -1
        } else {
            0
        };
        if sg != sign {
            // Reversed into the other direction: a new corner.
            close(&mut sign, &mut peak, &mut curves);
            sign = sg;
            peak = Some((i, lean));
        } else if let Some((_, p)) = peak {
            if abs > p.abs() {
                peak = Some((i, lean));
            }
        }
    }
    // A corner still in progress at the end of the ride still happened.
    close(&mut sign, &mut peak, &mut curves);
    curves
}

/// A candidate moment before ranking.
#[derive(Debug, Clone)]
struct Candidate {
    t: f64,
    score: f64,
    index: usize,
    /// For window metrics, the span covered.
    window: Option<(f64, f64)>,
    curve_count: Option<usize>,
    peak_lean: Option<f64>,
}

/// Rank candidates, suppressing near-duplicates.
///
/// Without this a top-ten list is not ten moments: the hardest braking of the
/// reference ride occupies three consecutive seconds and would take three
/// slots, and one corner sequence would take three more. So after choosing a
/// peak, everything within `min_gap_s` of it is excluded.
///
/// Sorted by score descending with ties broken by the earlier timestamp, which
/// is what makes the output deterministic. The comparison is strict, so two
/// moments exactly `min_gap_s` apart are both kept. Returned in score order.
fn select_top(mut candidates: Vec<Candidate>, top: usize, min_gap_s: f64) -> Vec<Candidate> {
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal))
    });
    let mut chosen: Vec<Candidate> = Vec::with_capacity(top);
    for c in candidates {
        if chosen.len() >= top {
            break;
        }
        if chosen.iter().any(|k| (k.t - c.t).abs() < min_gap_s) {
            continue;
        }
        chosen.push(c);
    }
    chosen
}

/// Find the densest windows of corners.
///
/// Every curve peak is a candidate window start, because a window that is
/// maximal must begin at one: sliding it forward from anywhere else only ever
/// loses corners. The window is the half-open span `[start, start + length)`.
fn curviest_windows(
    curves: &[Curve],
    window_s: f64,
    top: usize,
) -> Vec<Candidate> {
    let mut candidates = Vec::with_capacity(curves.len());
    for (i, start_curve) in curves.iter().enumerate() {
        let start = start_curve.t;
        let end = start + window_s;
        let mut count = 0usize;
        let mut peak = start_curve;
        for c in &curves[i..] {
            if c.t >= end {
                break;
            }
            count += 1;
            if c.lean_deg.abs() > peak.lean_deg.abs() {
                peak = c;
            }
        }
        candidates.push(Candidate {
            t: start,
            score: count as f64,
            index: peak.index,
            window: Some((start, end)),
            curve_count: Some(count),
            peak_lean: Some(peak.lean_deg),
        });
    }
    // Separated by the window length so that reported windows never overlap;
    // `--min-gap` deliberately does not apply here.
    select_top(candidates, top, window_s)
}

/// One ranked moment.
#[derive(Debug, Clone)]
pub struct Entry {
    pub rank: usize,
    /// Epoch seconds.
    pub t: f64,
    /// Seconds from the start of the ride.
    pub offset_s: f64,
    pub video_time_s: Option<f64>,
    /// Where it happened, when the receiver could say.
    pub pos: Option<(f64, f64)>,
    pub ele_m: Option<f64>,
    pub speed_ms: Option<f64>,
    /// The metric's value in its own unit.
    pub value: f64,
    /// Acceleration in g, for the acceleration metrics.
    pub g: Option<f64>,
    /// `left` or `right`, for lean.
    pub direction: Option<&'static str>,
    pub at_sensor_limit: bool,
    pub curve_count: Option<usize>,
    pub window_s: Option<(f64, f64)>,
    pub peak_lean_deg: Option<f64>,
}

impl Entry {
    pub fn speed_kmh(&self) -> Option<f64> {
        self.speed_ms.map(|s| s * 3.6)
    }

    /// The span this moment covers: zero for an instant, the window length for
    /// a window metric.
    pub fn span_s(&self) -> f64 {
        self.window_s.map(|(a, b)| b - a).unwrap_or(0.0)
    }
}

#[derive(Debug, Clone)]
pub struct Metric {
    pub id: MetricId,
    pub entries: Vec<Entry>,
    /// Samples that had no value for this metric and were skipped.
    pub skipped: usize,
    /// Set when this metric could not be computed at all.
    pub unavailable: Option<&'static str>,
}

#[derive(Debug, Clone)]
pub struct Quality {
    pub total: usize,
    pub missing_lean: usize,
    pub missing_speed: usize,
    pub missing_accel: usize,
    pub lean_limit: Option<SensorLimit>,
    pub curves_detected: usize,
}

/// The complete analysis.
pub struct Analysis {
    pub cadence: Cadence,
    pub quality: Quality,
    pub metrics: Vec<Metric>,
    pub options: Options,
    pub t0: f64,
    pub distance_m: f64,
    pub warnings: Vec<String>,
    /// Where the samples came from, for the report header.
    pub source: String,
    /// Declared rates from the source's own metadata, for comparison against
    /// the measured cadence. Display only.
    pub declared_hz: Vec<(String, f64)>,
}

/// Analyse a ride.
pub fn analyse(samples: &[Sample], opts: &Options) -> anyhow::Result<Analysis> {
    if samples.len() < 2 {
        anyhow::bail!(
            "need at least 2 samples to compute anything; found {}",
            samples.len()
        );
    }
    let mut warnings = Vec::new();
    let cadence = derive_cadence(samples, opts.max_gap_factor);
    let t0 = samples[0].t;

    if !cadence.regular {
        warnings.push(format!(
            "sampling is irregular: the central 90% of intervals span {:.3}..{:.3} s against a \
             median of {:.3} s",
            cadence.p5_dt, cadence.p95_dt, cadence.median_dt
        ));
    }
    if !cadence.gaps.is_empty() {
        warnings.push(format!(
            "{} gap(s) longer than {:.2} s break the derived channels rather than producing false \
             spikes",
            cadence.gaps.len(),
            cadence.gap_threshold
        ));
    }
    if cadence.non_positive > 0 {
        warnings.push(format!(
            "{} zero or backwards interval(s) were skipped",
            cadence.non_positive
        ));
    }

    // Acceleration, from whichever source was asked for.
    let accel: Vec<Option<f64>> = match opts.accel_source {
        AccelSource::Gps => {
            derive_acceleration_from_gps(samples, cadence.gap_threshold, opts.accel_ceiling_ms2)
        }
        AccelSource::Imu => samples
            .iter()
            .map(|s| s.accel_lon_g.map(|g| g * G))
            .collect(),
    };
    if opts.accel_source == AccelSource::Imu && accel.iter().all(|a| a.is_none()) {
        warnings.push(
            "no IMU longitudinal acceleration in this source; falling back to GPS speed \
             differentiation"
                .into(),
        );
    }
    let accel = if opts.accel_source == AccelSource::Imu && accel.iter().all(|a| a.is_none()) {
        derive_acceleration_from_gps(samples, cadence.gap_threshold, opts.accel_ceiling_ms2)
    } else {
        accel
    };

    let lean_limit = detect_lean_limit(samples);
    let curves = detect_curves(samples, opts.curve_threshold_deg);

    let quality = Quality {
        total: samples.len(),
        missing_lean: samples.iter().filter(|s| s.lean_deg.is_none()).count(),
        missing_speed: samples.iter().filter(|s| s.speed_ms.is_none()).count(),
        missing_accel: accel.iter().filter(|a| a.is_none()).count(),
        lean_limit,
        curves_detected: curves.len(),
    };

    let mut metrics = Vec::new();
    for &id in &opts.metrics {
        metrics.push(build_metric(
            id, samples, &accel, &curves, lean_limit, opts, t0,
        ));
    }

    let distance_m = distance_over(samples, cadence.gap_threshold);

    Ok(Analysis {
        cadence,
        quality,
        metrics,
        options: opts.clone(),
        t0,
        distance_m,
        warnings,
        source: String::new(),
        declared_hz: Vec::new(),
    })
}

/// Ground distance covered, in metres.
///
/// Walks the samples that *have* a position and bridges the ones that do not,
/// so a single dropped fix does not silently delete the segment either side of
/// it. A gap longer than `gap_threshold` is never bridged: across one the bike
/// travelled unobserved, and the straight line over it is not distance that
/// was measured.
pub fn distance_over(samples: &[Sample], gap_threshold: f64) -> f64 {
    let mut total = 0.0;
    let mut previous: Option<(f64, (f64, f64))> = None;
    for s in samples {
        let Some(pos) = s.pos else { continue };
        if let Some((prev_t, prev_pos)) = previous {
            let dt = s.t - prev_t;
            if dt > 0.0 && (!gap_threshold.is_finite() || dt <= gap_threshold) {
                total += haversine(prev_pos.0, prev_pos.1, pos.0, pos.1);
            }
        }
        previous = Some((s.t, pos));
    }
    total
}

fn build_metric(
    id: MetricId,
    samples: &[Sample],
    accel: &[Option<f64>],
    curves: &[Curve],
    lean_limit: Option<SensorLimit>,
    opts: &Options,
    t0: f64,
) -> Metric {
    let mut skipped = 0usize;
    let mut candidates: Vec<Candidate> = Vec::new();
    let plain = |i: usize, score: f64| Candidate {
        t: samples[i].t,
        score,
        index: i,
        window: None,
        curve_count: None,
        peak_lean: None,
    };

    match id {
        MetricId::Lean => {
            for (i, s) in samples.iter().enumerate() {
                match s.lean_deg {
                    Some(l) => candidates.push(plain(i, l.abs())),
                    None => skipped += 1,
                }
            }
        }
        MetricId::Speed => {
            for (i, s) in samples.iter().enumerate() {
                match s.speed_ms {
                    Some(v) => candidates.push(plain(i, v)),
                    None => skipped += 1,
                }
            }
        }
        MetricId::Accel | MetricId::Decel => {
            for (i, a) in accel.iter().enumerate() {
                match a {
                    // Exactly zero belongs to neither.
                    Some(a) if id == MetricId::Accel && *a > 0.0 => candidates.push(plain(i, *a)),
                    Some(a) if id == MetricId::Decel && *a < 0.0 => candidates.push(plain(i, -*a)),
                    Some(_) => {}
                    None => skipped += 1,
                }
            }
        }
        MetricId::Lateral => {
            for (i, s) in samples.iter().enumerate() {
                match s.accel_lat_g {
                    Some(g) => candidates.push(plain(i, g.abs())),
                    None => skipped += 1,
                }
            }
        }
        MetricId::Total => {
            for (i, s) in samples.iter().enumerate() {
                match s.accel_total_g {
                    Some(g) => candidates.push(plain(i, g)),
                    None => skipped += 1,
                }
            }
        }
        MetricId::Curves => {
            let chosen = curviest_windows(curves, opts.window_s, opts.top);
            return Metric {
                id,
                entries: to_entries(id, chosen, samples, accel, lean_limit, t0),
                skipped: 0,
                unavailable: curves.is_empty().then_some(
                    "no curves were detected at this lean threshold, or the source has no lean \
                     angle",
                ),
            };
        }
    }

    if candidates.is_empty() {
        return Metric {
            id,
            entries: Vec::new(),
            skipped,
            unavailable: Some("the source carries no data for this metric"),
        };
    }

    let chosen = select_top(candidates, opts.top, opts.min_gap_s);
    Metric {
        id,
        entries: to_entries(id, chosen, samples, accel, lean_limit, t0),
        skipped,
        unavailable: None,
    }
}

fn to_entries(
    id: MetricId,
    chosen: Vec<Candidate>,
    samples: &[Sample],
    accel: &[Option<f64>],
    lean_limit: Option<SensorLimit>,
    t0: f64,
) -> Vec<Entry> {
    chosen
        .into_iter()
        .enumerate()
        .map(|(rank, c)| {
            let s = &samples[c.index];
            let (value, g, direction, at_limit) = match id {
                MetricId::Lean => {
                    let lean = s.lean_deg.unwrap_or(0.0);
                    (
                        lean,
                        None,
                        Some(if lean < 0.0 { "left" } else { "right" }),
                        lean_limit
                            .map(|l| lean.abs() == l.value)
                            .unwrap_or(false),
                    )
                }
                MetricId::Accel => {
                    let a = accel[c.index].unwrap_or(0.0);
                    (a, Some(a / G), None, false)
                }
                MetricId::Decel => {
                    let a = accel[c.index].unwrap_or(0.0).abs();
                    (a, Some(a / G), None, false)
                }
                MetricId::Speed => (s.speed_ms.unwrap_or(0.0) * 3.6, None, None, false),
                MetricId::Lateral => {
                    let g = s.accel_lat_g.unwrap_or(0.0);
                    (
                        g,
                        Some(g),
                        Some(if g < 0.0 { "left" } else { "right" }),
                        false,
                    )
                }
                MetricId::Total => {
                    let g = s.accel_total_g.unwrap_or(0.0);
                    (g, Some(g), None, false)
                }
                MetricId::Curves => (c.score, None, None, false),
            };

            // A window metric is reported at the window's start; everything
            // else at the instant it happened.
            let (t, offset) = match c.window {
                Some((start, _)) => (start, start - t0),
                None => (s.t, s.t - t0),
            };

            Entry {
                rank: rank + 1,
                t,
                offset_s: offset,
                video_time_s: s.video_time_s,
                pos: s.pos,
                ele_m: s.ele_m,
                speed_ms: s.speed_ms,
                value,
                g,
                direction,
                at_sensor_limit: at_limit,
                curve_count: c.curve_count,
                window_s: c.window,
                peak_lean_deg: c.peak_lean,
            }
        })
        .collect()
}

// ---------------------------------------------------------------- adapters

/// Build samples from camera telemetry.
///
/// One sample per GPS fix rather than per master sample. The master timeline
/// runs at 29.97 Hz and the receiver at 10 Hz, so using every master sample
/// would triple the work while repeating each position three times — and the
/// metrics that need position (speed, the coordinates of every reported moment)
/// would gain nothing. Lean and the acceleration channels are read from the
/// master timeline at each fix's own instant, so their resolution is unaffected
/// for the purposes of ranking.
pub fn samples_from_telemetry(t: &crate::telemetry::Telemetry) -> Vec<Sample> {
    let mut out = Vec::with_capacity(t.gps.len());
    let mut i = 0usize;
    for fix in &t.gps {
        while i + 1 < t.n && t.times[i + 1] <= fix.time {
            i += 1;
        }
        let time = match fix.utc.or_else(|| {
            t.t0.map(|t0| {
                t0 + chrono::Duration::microseconds((fix.time * 1e6).round() as i64)
            })
        }) {
            Some(x) => x,
            None => continue,
        };
        out.push(Sample {
            t: time.timestamp_micros() as f64 / 1e6,
            // A rejected fix contributes no position at all. The camera keeps
            // emitting coordinates after losing lock, and they are nonsense.
            pos: fix.valid.then_some((fix.lat, fix.lon)),
            ele_m: fix.valid.then_some(fix.alt_m),
            speed_ms: fix.valid.then_some(fix.speed_2d),
            lean_deg: Some(t.lean_deg[i]),
            accel_lon_g: t.accel_lon_g.as_ref().map(|v| v[i]),
            accel_lat_g: t.accel_lat_g.as_ref().map(|v| v[i]),
            accel_total_g: t.accel_total_g.as_ref().map(|v| v[i]),
            video_time_s: Some(fix.time),
        });
    }
    out
}

/// Build samples from a GPX track.
///
/// Works on anyone's GPX, including a merged one: the lean angle and
/// acceleration come from whatever extensions the file carries.
pub fn samples_from_gpx(g: &crate::gpx::Gpx) -> Vec<Sample> {
    g.points
        .iter()
        .filter(|p| p.time.is_some())
        .map(|p| Sample {
            t: p.time.unwrap().timestamp_micros() as f64 / 1e6,
            pos: Some((p.lat, p.lon)),
            ele_m: p.ele_m,
            speed_ms: p.speed_ms,
            lean_deg: p.lean_deg,
            // A GPX carries longitudinal acceleration in m/s^2; the sample
            // struct holds it in g.
            accel_lon_g: p.accel_ms2.map(|a| a / G),
            accel_lat_g: None,
            accel_total_g: None,
            video_time_s: p.video_time_s,
        })
        .collect()
}

/// Build samples straight from a CSV written by `gmax telemetry`.
pub fn samples_from_csv(text: &str) -> anyhow::Result<Vec<Sample>> {
    let mut lines = text.lines();
    let header: Vec<&str> = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("the CSV is empty"))?
        .split(',')
        .map(|h| h.trim())
        .collect();
    let idx = |name: &str| header.iter().position(|h| *h == name);
    let (Some(i_utc), Some(i_lat), Some(i_lon)) = (idx("utc"), idx("lat"), idx("lon")) else {
        anyhow::bail!("the CSV needs at least utc, lat and lon columns");
    };
    let i_time = idx("time_s");
    let i_ele = idx("alt_m");
    let i_speed = idx("speed_kmh");
    let i_lean = idx("lean_deg");
    let i_alon = idx("accel_lon_g");
    let i_alat = idx("accel_lat_g");
    let i_atot = idx("accel_total_g");

    let mut out = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split(',').collect();
        let get = |i: Option<usize>| -> Option<f64> {
            i.and_then(|i| f.get(i))
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse().ok())
        };
        let Some(utc) = f.get(i_utc).map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            continue;
        };
        let Ok(time) = chrono::DateTime::parse_from_rfc3339(utc) else {
            continue;
        };
        // A row with no position is kept: the receiver could not place it, but
        // the IMU channels on that row are still valid.
        let pos = get(Some(i_lat)).zip(get(Some(i_lon)));
        out.push(Sample {
            t: time.timestamp_micros() as f64 / 1e6,
            pos,
            ele_m: get(i_ele),
            speed_ms: get(i_speed).map(|kmh| kmh / 3.6),
            lean_deg: get(i_lean),
            accel_lon_g: get(i_alon),
            accel_lat_g: get(i_alat),
            accel_total_g: get(i_atot),
            video_time_s: get(i_time),
        });
    }
    if out.is_empty() {
        anyhow::bail!("the CSV contained no rows with a usable timestamp");
    }
    Ok(out)
}
