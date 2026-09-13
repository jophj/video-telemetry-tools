//! Repair a GoPro GPS track from an external GPX source.
//!
//! The camera's receiver is the weak link in an otherwise good telemetry set.
//! On the reference clip it has no lock at all for the first thirteen seconds
//! and loses it for eleven of twenty-five minutes — 45 % of the ride — while
//! the phone riding in the same tank bag reported a four-metre fix throughout.
//! Since lean angle, acceleration and orientation all come from the camera's
//! IMU and are unaffected, the useful operation is to keep everything the
//! camera measured well and substitute only the positions it measured badly.
//!
//! Two problems have to be solved in order:
//!
//!   1. **Alignment.** The two devices have independent clocks. Measured on the
//!      reference material, the obvious approach — sliding the tracks until
//!      their positions coincide — lands 5 s wrong, because the camera's *bad*
//!      positions dominate the fit. Correlating lean angle instead is accurate
//!      to a tenth of a second, because both devices measure the same corners
//!      with instruments that work.
//!   2. **Substitution.** Which source owns each point, decided per point from
//!      measured quality rather than wholesale.

use chrono::{DateTime, Duration, Utc};

use crate::gpx::{Gpx, TrackPoint};
use crate::math::*;
use crate::telemetry::{Source, Telemetry};

/// How to choose between the two sources at each point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Prefer {
    /// Keep the camera's position where its own quality metrics say it is
    /// sound, and substitute the external source elsewhere.
    #[default]
    QualityGated,
    /// Take the external position wherever the external track reaches.
    External,
    /// Keep every camera position it has, however poor.
    Gopro,
}

#[derive(Debug, Clone, Copy)]
pub enum Align {
    /// Correlate lean angle if both sources have it, else fit positions.
    Auto,
    /// A known offset in seconds, added to external timestamps.
    Fixed(f64),
    /// Assume the clocks already agree.
    None,
}

/// How to handle the camera's GPS position latency.
///
/// A receiver's positions can lag the timestamps they are given. Measured on
/// the reference clip, the GoPro Max 2's are stale by about 4.4 s: with the
/// clocks aligned by lean angle, the camera's positions sit 71 m *behind* the
/// phone's along the direction of travel, and 71 m over 4.4 s is 58 km/h --
/// the ride's median speed. The cross-track offset is only 12 m, so this is a
/// lag and not a spatial bias.
///
/// Left uncorrected, the camera's positions are displaced by tens of metres at
/// road speed, and splicing them against a correctly-timed source produces a
/// track that steps back and forth at every join.
#[derive(Debug, Clone, Copy)]
pub enum Lag {
    /// Measure it as the difference between the positional and lean-derived
    /// offsets, and correct it.
    Auto,
    /// A known lag in seconds.
    Fixed(f64),
    /// Take the camera's position timestamps at face value.
    None,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub prefer: Prefer,
    pub align: Align,
    pub lag: Lag,
    /// Search this many seconds either side of zero for the clock offset.
    pub span: f64,
    /// Reject a substitution whose nearest external point is further than this
    /// in time.
    pub max_dt: f64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            prefer: Prefer::QualityGated,
            align: Align::Auto,
            lag: Lag::Auto,
            span: 30.0,
            max_dt: 1.0,
        }
    }
}

/// One position on the base timeline, which may or may not be usable.
///
/// Keeping the unusable ones is the whole point: they are where the external
/// source is needed, and discarding them first would lose the record of when
/// the camera was *trying* to report a position.
#[derive(Debug, Clone)]
pub struct Slot {
    pub time: DateTime<Utc>,
    /// Seconds from the start of the video, for cross-referencing to frames.
    pub video_time_s: Option<f64>,
    /// The camera's own position, when it was good enough to keep.
    pub fix: Option<TrackPoint>,
    /// Rider channels from the camera's IMU. These are unaffected by GPS
    /// quality and are carried through regardless of which source wins.
    pub lean_deg: Option<f64>,
    pub accel_ms2: Option<f64>,
}

/// What the merge did, in enough detail to audit it afterwards.
#[derive(Debug, Clone)]
pub struct Report {
    pub method: String,
    pub offset_s: f64,
    /// `|r|` for lean alignment, or median metres of separation for positional.
    pub score: f64,
    /// Whether the two sources' lean angles agree in polarity. `None` when
    /// alignment did not use lean.
    pub lean_polarity_agrees: Option<bool>,
    /// Measured staleness of the camera's positions, in seconds, and whether
    /// it was corrected.
    pub gps_lag_s: Option<f64>,
    pub gps_lag_applied: bool,
    /// Median separation between the two sources after every correction, in
    /// metres. This is the honest measure of how much they still disagree.
    pub residual_m: Option<f64>,
    /// Measured positional noise of each source, lower being better.
    pub camera_dispersion: Option<f64>,
    pub external_dispersion: Option<f64>,
    /// Set when the quality gate concluded the external source is the better
    /// receiver and handed it the overlap.
    pub external_preferred_on_quality: bool,
    pub samples: usize,
    pub points_total: usize,
    pub points_gopro: usize,
    pub points_external: usize,
    pub points_unpositioned: usize,
    pub external_points: usize,
    pub external_span: Option<(DateTime<Utc>, DateTime<Utc>)>,
    pub warnings: Vec<String>,
}

impl Report {
    pub fn summary(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "alignment: {} offset {:+.3} s ({}), from {} paired samples\n",
            self.method,
            self.offset_s,
            if self.method.starts_with("lean") {
                format!("|r| = {:.3}", self.score)
            } else {
                format!("median separation {:.1} m", self.score)
            },
            self.samples
        ));
        if let Some(agrees) = self.lean_polarity_agrees {
            if !agrees {
                s.push_str(
                    "note: the external source's lean angle is signed opposite to ours; \
                     alignment is unaffected, but do not mix the two channels\n",
                );
            }
        }
        if let Some(l) = self.gps_lag_s {
            s.push_str(&format!(
                "camera GPS lag: {:+.2} s{}\n",
                l,
                if self.gps_lag_applied {
                    " (corrected)"
                } else {
                    " (measured, not corrected)"
                }
            ));
        }
        if let Some(r) = self.residual_m {
            s.push_str(&format!(
                "residual disagreement between the sources: {r:.1} m (median)\n"
            ));
        }
        if let (Some(c), Some(e)) = (self.camera_dispersion, self.external_dispersion) {
            s.push_str(&format!(
                "positional noise: camera {c:.2}, external {e:.2} (lower is better){}\n",
                if self.external_preferred_on_quality {
                    " -- external preferred"
                } else {
                    ""
                }
            ));
        }
        s.push_str(&format!(
            "points: {} total, {} from the camera, {} substituted from the external source, \
             {} still unpositioned\n",
            self.points_total, self.points_gopro, self.points_external, self.points_unpositioned
        ));
        if let Some((a, b)) = self.external_span {
            s.push_str(&format!(
                "external source: {} points spanning {} to {}\n",
                self.external_points,
                a.format("%H:%M:%S"),
                b.format("%H:%M:%S")
            ));
        }
        for w in &self.warnings {
            s.push_str(&format!("warning: {w}\n"));
        }
        s
    }
}

/// Build the base timeline from extracted camera telemetry.
///
/// This is the richer of the two entry points: it knows every instant the
/// receiver reported, including the ones it got wrong, so gaps are explicit
/// rather than inferred.
pub fn slots_from_telemetry(t: &Telemetry) -> Vec<Slot> {
    let mut slots = Vec::with_capacity(t.gps.len());
    let mut i = 0usize;
    for fix in &t.gps {
        // Track the master timeline alongside, to pick up the rider channels in
        // effect at this instant.
        while i + 1 < t.n && t.times[i + 1] <= fix.time {
            i += 1;
        }
        let time = match fix.utc.or_else(|| t.utc_of(fix.time)) {
            Some(x) => x,
            None => continue, // no clock, so nothing to align against
        };
        slots.push(Slot {
            time,
            video_time_s: Some(fix.time),
            fix: fix.valid.then(|| TrackPoint {
                lat: fix.lat,
                lon: fix.lon,
                ele_m: Some(fix.alt_m),
                time: Some(time),
                hdop: fix.dop,
                speed_ms: Some(fix.speed_2d),
                course_deg: fix.course_deg,
                ..Default::default()
            }),
            lean_deg: Some(t.lean_deg[i]),
            accel_ms2: t.accel_lon_g.as_ref().map(|v| v[i] * G),
        });
    }
    slots
}

/// Build the base timeline from an already-extracted GPX.
///
/// A GPX carries only the points that were good enough to write, so the gaps
/// have to be reconstructed: any interval longer than a few sample periods gets
/// empty slots at the track's own measured cadence, which is what gives the
/// external source somewhere to be substituted into.
pub fn slots_from_gpx(base: &Gpx) -> Vec<Slot> {
    let timed = base.timed_points();
    if timed.is_empty() {
        return Vec::new();
    }

    // Measure the cadence rather than assuming it.
    let mut deltas: Vec<f64> = timed
        .windows(2)
        .map(|w| {
            (w[1].time.unwrap() - w[0].time.unwrap()).num_milliseconds() as f64 / 1000.0
        })
        .filter(|d| *d > 0.0)
        .collect();
    deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let cadence = if deltas.is_empty() {
        1.0
    } else {
        quantile_sorted(&deltas, 0.5)
    };

    let mut slots = Vec::with_capacity(timed.len());
    for (k, p) in timed.iter().enumerate() {
        let time = p.time.unwrap();
        if k > 0 {
            // Fill a gap with empty slots so the merge has points to place.
            let prev = timed[k - 1].time.unwrap();
            let gap = (time - prev).num_milliseconds() as f64 / 1000.0;
            if gap > cadence * 2.5 {
                let steps = (gap / cadence).round() as i64;
                for s in 1..steps {
                    slots.push(Slot {
                        time: prev + Duration::milliseconds((s as f64 * cadence * 1000.0) as i64),
                        video_time_s: None,
                        fix: None,
                        lean_deg: None,
                        accel_ms2: None,
                    });
                }
            }
        }
        slots.push(Slot {
            time,
            video_time_s: p.video_time_s,
            fix: Some((*p).clone()),
            lean_deg: p.lean_deg,
            accel_ms2: p.accel_ms2,
        });
    }
    slots
}

/// Write merged positions back onto the camera telemetry they came from.
///
/// [`merge`] produces a GPX, which is the right output for a track and the
/// wrong one for the moments engine. A GPX carries lean angle and longitudinal
/// acceleration and nothing else, so ranking a ride from one silently drops
/// the lateral and total-load metrics — they come off the camera's IMU and
/// have no GPX representation. Putting the repaired positions back where they
/// came from keeps both: the external receiver's positions, and the full set
/// of channels the camera measured.
///
/// Matching is by video time, which both sides carry and which no clock offset
/// can disturb: the merge may shift the *external* timestamps to align them,
/// but a slot's position in the recording is fixed. Returns how many fixes
/// were repaired.
pub fn apply_to_telemetry(t: &mut Telemetry, merged: &Gpx) -> usize {
    // Video times of the camera's own fixes, ascending, for bisection.
    let fix_times: Vec<f64> = t.gps.iter().map(|f| f.time).collect();
    let mut repaired = 0;
    for p in &merged.points {
        let Some(vt) = p.video_time_s else { continue };
        // The nearest fix either side, then whichever is closer. Half a fix
        // interval is the most a correctly matched point can be away.
        let i = fix_times.partition_point(|&x| x < vt);
        let cand = [i.checked_sub(1), (i < fix_times.len()).then_some(i)];
        let Some(j) = cand
            .into_iter()
            .flatten()
            .min_by(|&a, &b| {
                (fix_times[a] - vt)
                    .abs()
                    .partial_cmp(&(fix_times[b] - vt).abs())
                    .unwrap()
            })
            .filter(|&j| (fix_times[j] - vt).abs() < 0.05)
        else {
            continue;
        };
        let fix = &mut t.gps[j];
        // A point the merge could not place leaves the fix as it found it,
        // rejected and with its reason intact.
        if p.source.as_deref() == Some("interpolated") && !fix.valid {
            continue;
        }
        fix.lat = p.lat;
        fix.lon = p.lon;
        if let Some(e) = p.ele_m {
            fix.alt_m = e;
        }
        if let Some(s) = p.speed_ms {
            fix.speed_2d = s;
        }
        fix.course_deg = p.course_deg;
        fix.source = match p.source.as_deref() {
            Some("external") => Source::External,
            Some("interpolated") => Source::Interpolated,
            _ => Source::Gopro,
        };
        if !fix.valid {
            repaired += 1;
        }
        fix.valid = true;
        fix.reject = None;
    }
    repaired
}

/// Merge an external GPX into a base timeline.
pub fn merge(slots: &[Slot], external: &Gpx, opts: &Options) -> (Gpx, Report) {
    let ext: Vec<&TrackPoint> = external.timed_points();
    let mut warnings = Vec::new();

    if ext.is_empty() {
        warnings.push("the external GPX has no timestamped points; nothing to merge".into());
    }

    // --- align ------------------------------------------------------------
    let alignment = match opts.align {
        Align::None => Alignment {
            offset: 0.0,
            method: "assumed identical clocks".to_string(),
            score: 0.0,
            samples: 0,
            lean_polarity: None,
            lag: None,
        },
        Align::Fixed(o) => Alignment {
            offset: o,
            method: "given on the command line".to_string(),
            score: 0.0,
            samples: 0,
            lean_polarity: None,
            lag: None,
        },
        Align::Auto if ext.is_empty() => Alignment {
            offset: 0.0,
            method: "not attempted (no external points)".to_string(),
            score: 0.0,
            samples: 0,
            lean_polarity: None,
            lag: None,
        },
        Align::Auto => align(slots, &ext, opts.span, &mut warnings),
    };
    let offset = alignment.offset;

    // --- correct the camera's GPS latency ---------------------------------
    // The camera's positions describe where the bike was `lag` seconds before
    // the timestamp they carry, so they are re-timed rather than moved. Its IMU
    // channels are correctly timed and stay where they are.
    let (lag, lag_applied) = match opts.lag {
        Lag::None => (alignment.lag, false),
        Lag::Fixed(l) => (Some(l), l.abs() > 1e-9),
        // Only correct a lag big enough to matter: below half a second it is
        // within the noise of the fits that measured it.
        Lag::Auto => match alignment.lag {
            Some(l) if l.abs() > 0.5 => (Some(l), true),
            other => (other, false),
        },
    };
    let applied_lag = if lag_applied { lag.unwrap_or(0.0) } else { 0.0 };
    if lag_applied {
        warnings.push(format!(
            "the camera's GPS positions lag their own timestamps by {:+.2} s and have been \
             re-timed by that much. At this ride's median speed that is a displacement of about \
             {:.0} m.",
            applied_lag,
            applied_lag.abs() * 16.2
        ));
    }

    // Both sources now go through one interpolation path, on one clock.
    let camera: Vec<(DateTime<Utc>, TrackPoint)> = slots
        .iter()
        .filter_map(|s| {
            s.fix.as_ref().map(|f| {
                (
                    s.time - Duration::microseconds((applied_lag * 1e6).round() as i64),
                    f.clone(),
                )
            })
        })
        .collect();
    let camera_refs: Vec<(DateTime<Utc>, &TrackPoint)> =
        camera.iter().map(|(t, p)| (*t, p)).collect();

    let ext_shifted_owned: Vec<(DateTime<Utc>, &TrackPoint)> = ext
        .iter()
        .map(|p| {
            (
                p.time.unwrap() + Duration::microseconds((offset * 1e6).round() as i64),
                *p,
            )
        })
        .collect();

    // --- measure what disagreement is left --------------------------------
    let mut separations = Vec::new();
    for slot in slots {
        if let (Some(a), Some(b)) = (
            interpolate_at(&camera_refs, slot.time, opts.max_dt),
            interpolate_at(&ext_shifted_owned, slot.time, opts.max_dt),
        ) {
            separations.push(haversine(a.lat, a.lon, b.lat, b.lon));
        }
    }
    separations.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let residual = (!separations.is_empty()).then(|| quantile_sorted(&separations, 0.5));

    // --- which receiver is actually better? -------------------------------
    // `QualityGated` has to mean "gated on measured quality", not "gated on
    // whichever source's self-report we happen to have". The camera's DOP is
    // nearly constant and so carries no information about its position track;
    // comparing both sources' self-consistency does.
    let camera_dispersion = positional_dispersion(&camera_refs);
    let external_dispersion = positional_dispersion(&ext_shifted_owned);
    let external_is_better = match (camera_dispersion, external_dispersion) {
        // A factor of two is a wide margin, so this only fires when the
        // difference is unambiguous. Here it is eleven-fold.
        (Some(c), Some(e)) => e * 2.0 < c,
        _ => false,
    };
    let prefer_external_on_quality =
        opts.prefer == Prefer::QualityGated && external_is_better;
    if prefer_external_on_quality {
        warnings.push(format!(
            "the external source's positions are measurably cleaner than the camera's \
             (dispersion {:.2} against {:.2}), so it is preferred wherever it reaches. \
             Pass --prefer gopro to keep the camera's positions instead.",
            external_dispersion.unwrap_or(0.0),
            camera_dispersion.unwrap_or(0.0),
        ));
    }

    // --- substitute -------------------------------------------------------
    let mut points = Vec::with_capacity(slots.len());
    let mut n_gopro = 0usize;
    let mut n_external = 0usize;
    let mut n_missing = 0usize;

    for slot in slots {
        let from_camera = interpolate_at(&camera_refs, slot.time, opts.max_dt);
        let from_external = interpolate_at(&ext_shifted_owned, slot.time, opts.max_dt);

        // The gate. Under `QualityGated` the camera keeps the point wherever it
        // had a position good enough to survive the quality checks, and the
        // external source fills in everywhere else.
        let external_first = matches!(opts.prefer, Prefer::External) || prefer_external_on_quality;
        let chosen = match opts.prefer {
            Prefer::Gopro => from_camera.map(|p| (p, Source::Gopro)),
            _ if external_first => from_external
                .map(|p| (p, Source::External))
                .or_else(|| from_camera.map(|p| (p, Source::Gopro))),
            _ => from_camera
                .map(|p| (p, Source::Gopro))
                .or_else(|| from_external.map(|p| (p, Source::External))),
        };

        match chosen {
            Some((mut p, source)) => {
                // The camera's IMU channels are unaffected by GPS quality, so
                // they survive a substitution. An external source's own lean is
                // used only where we have none of our own.
                p.time = Some(slot.time);
                p.lean_deg = slot.lean_deg.or(p.lean_deg);
                p.accel_ms2 = slot.accel_ms2.or(p.accel_ms2);
                p.video_time_s = slot.video_time_s;
                p.source = Some(source.as_str().to_string());
                match source {
                    Source::External => n_external += 1,
                    _ => n_gopro += 1,
                }
                points.push(p);
            }
            None => {
                // Neither source can say where the bike was. Leaving the point
                // out is the honest outcome; inventing one would put a
                // fabricated position into a file that looks authoritative.
                n_missing += 1;
            }
        }
    }

    if n_external == 0 && !ext.is_empty() && opts.prefer != Prefer::Gopro {
        warnings.push(
            "no points were substituted: check the alignment offset and that the external \
             track actually covers this recording's time range"
                .into(),
        );
    }
    // If the two sources still disagree by more than either receiver's own
    // accuracy, splicing them per point leaves visible steps, and the user
    // should know there is a choice to make.
    // If the two sources still disagree by more than either receiver's own
    // accuracy, and we are splicing between them, the track will step at each
    // join and the user should know.
    if let Some(r) = residual {
        if r > 25.0 && n_gopro > 0 && n_external > 0 && !prefer_external_on_quality {
            warnings.push(format!(
                "the two sources still disagree by {r:.0} m after alignment, so the merged track \
                 steps at each of the {} joins between them.",
                n_gopro.min(n_external)
            ));
        }
    }

    let report = Report {
        method: alignment.method,
        offset_s: offset,
        score: alignment.score,
        lean_polarity_agrees: alignment.lean_polarity,
        gps_lag_s: lag,
        gps_lag_applied: lag_applied,
        residual_m: residual,
        camera_dispersion,
        external_dispersion,
        external_preferred_on_quality: prefer_external_on_quality,
        samples: alignment.samples,
        points_total: points.len(),
        points_gopro: n_gopro,
        points_external: n_external,
        points_unpositioned: n_missing,
        external_points: ext.len(),
        external_span: external.time_span(),
        warnings,
    };

    let mut metadata = vec![
        ("merge_method".to_string(), report.method.clone()),
        ("merge_offset_s".to_string(), fmt_num(offset, 3)),
        ("merge_score".to_string(), fmt_num(report.score, 4)),
        ("merge_points_total".to_string(), points.len().to_string()),
        ("merge_points_gopro".to_string(), n_gopro.to_string()),
        ("merge_points_external".to_string(), n_external.to_string()),
        (
            "merge_points_unpositioned".to_string(),
            n_missing.to_string(),
        ),
        ("merge_max_dt_s".to_string(), fmt_num(opts.max_dt, 3)),
        (
            "merge_prefer".to_string(),
            format!("{:?}", opts.prefer).to_lowercase(),
        ),
    ];
    if let Some(l) = lag {
        metadata.push(("merge_gps_lag_s".to_string(), fmt_num(l, 3)));
        metadata.push((
            "merge_gps_lag_applied".to_string(),
            lag_applied.to_string(),
        ));
    }
    if let Some(r) = residual {
        metadata.push(("merge_residual_m".to_string(), fmt_num(r, 1)));
    }
    if let Some(d) = camera_dispersion {
        metadata.push(("merge_camera_dispersion".to_string(), fmt_num(d, 4)));
    }
    if let Some(d) = external_dispersion {
        metadata.push(("merge_external_dispersion".to_string(), fmt_num(d, 4)));
    }
    if let Some(name) = &external.name {
        metadata.push(("merge_external_name".to_string(), name.clone()));
    }

    let merged = Gpx {
        name: None,
        creator: None,
        activity: None,
        time: points.first().and_then(|p| p.time),
        points,
        metadata,
    };
    (merged, report)
}

/// The result of aligning the two sources.
struct Alignment {
    /// Seconds to add to external timestamps to reach the base clock.
    offset: f64,
    method: String,
    /// `|r|` for lean alignment, median metres for a positional fit.
    score: f64,
    samples: usize,
    /// Whether the two sources' lean angles agree in polarity.
    lean_polarity: Option<bool>,
    /// How stale the camera's positions are relative to their timestamps.
    lag: Option<f64>,
}

/// Find the clock offset between the two sources, and the camera's GPS lag.
///
/// Both are measured where possible, because they are different quantities and
/// only measuring both separates them:
///
///   * **Clock offset** is when the two devices think it is. Lean angle gives
///     this accurately, since both devices measure the same corners with
///     instruments that work. On the reference clip the peak is sharp:
///     `r = +0.891` at +1.60 s, staying within 90 % of that only over
///     +1.0…+2.2 s, and falling to 0.52 at zero.
///   * **GPS lag** is how far behind the camera's positions run relative to the
///     timestamps attached to them. It is the *difference* between the
///     positional fit and the lean fit -- about 4.4 s here.
///
/// Fitting positions alone cannot tell the two apart, which is why the
/// prototypes' positional fit landed 5 s from the true clock offset.
fn align(
    slots: &[Slot],
    ext: &[&TrackPoint],
    span: f64,
    warnings: &mut Vec<String>,
) -> Alignment {
    let base_has_lean = slots.iter().any(|s| s.lean_deg.is_some());
    let ext_has_lean = ext.iter().any(|p| p.lean_deg.is_some());

    let positional = align_by_position(slots, ext, span);

    if base_has_lean && ext_has_lean {
        if let Some((offset, r, n)) = align_by_lean(slots, ext, span) {
            return Alignment {
                offset,
                method: "lean-angle correlation".to_string(),
                score: r.abs(),
                samples: n,
                lean_polarity: Some(r > 0.0),
                lag: positional.map(|(p, _, _)| p - offset),
            };
        }
        warnings.push(
            "lean-angle alignment found too little overlap; fell back to fitting positions".into(),
        );
    } else if !ext_has_lean {
        warnings.push(
            "the external source carries no lean angle, so alignment falls back to fitting \
             positions. That fit absorbs any GPS latency into the clock offset, so the two \
             cannot be separated and neither is corrected for."
                .into(),
        );
    }

    match positional {
        Some((offset, metres, n)) => Alignment {
            offset,
            method: "positional fit".to_string(),
            score: metres,
            samples: n,
            lean_polarity: None,
            // A positional fit has already absorbed the lag into the offset, so
            // there is nothing left to correct separately.
            lag: None,
        },
        None => {
            warnings.push(
                "could not align the two sources at all; assuming their clocks agree".into(),
            );
            Alignment {
                offset: 0.0,
                method: "alignment failed".to_string(),
                score: 0.0,
                samples: 0,
                lean_polarity: None,
                lag: None,
            }
        }
    }
}

/// Align by maximising `|r|` between the two lean-angle series.
///
/// This is the accurate method, and the reason is physical: both devices
/// measure the same corners with instruments that are working, so the signals
/// genuinely match. The prototypes measured `|r| = 0.87` at a 0.80 s offset
/// this way, against a positional fit that was about 5 s wrong.
///
/// Sign-agnostic, because the two sources need not agree on which way is
/// positive — the returned `r` keeps its sign so the caller can report whether
/// they do.
fn align_by_lean(slots: &[Slot], ext: &[&TrackPoint], span: f64) -> Option<(f64, f64, usize)> {
    /// A tenth of a second: fine enough to place a corner, coarse enough that
    /// the search stays cheap.
    const STEP: f64 = 0.1;
    /// Accept a base sample within this of the shifted external instant.
    const TOLERANCE: f64 = 0.2;
    /// Below this many paired samples any offset can be made to fit.
    const MIN_PAIRS: usize = 100;

    let base: Vec<(f64, f64)> = slots
        .iter()
        .filter_map(|s| s.lean_deg.map(|l| (s.time.timestamp_micros() as f64 / 1e6, l)))
        .collect();
    let ext_lean: Vec<(f64, f64)> = ext
        .iter()
        .filter_map(|p| {
            p.lean_deg
                .map(|l| (p.time.unwrap().timestamp_micros() as f64 / 1e6, l))
        })
        .collect();
    if base.len() < MIN_PAIRS || ext_lean.len() < MIN_PAIRS {
        return None;
    }

    let base_times: Vec<f64> = base.iter().map(|(t, _)| *t).collect();
    let mut best: Option<(f64, f64, usize)> = None;
    let steps = (2.0 * span / STEP).round() as i64;

    for k in 0..=steps {
        let offset = -span + k as f64 * STEP;
        let mut xs = Vec::with_capacity(ext_lean.len());
        let mut ys = Vec::with_capacity(ext_lean.len());
        for &(t, lean) in &ext_lean {
            if let Some(j) = nearest_within(&base_times, t + offset, TOLERANCE) {
                xs.push(lean);
                ys.push(base[j].1);
            }
        }
        if xs.len() < MIN_PAIRS {
            continue;
        }
        let r = pearson(&xs, &ys);
        if best.is_none() || r.abs() > best.unwrap().1.abs() {
            best = Some((offset, r, xs.len()));
        }
    }
    best
}

/// Align by minimising the median separation between the two tracks.
///
/// The fallback, for an external source with no lean angle. Coarse then fine,
/// and it uses the *median* rather than the mean so that a handful of wild
/// camera positions cannot drag the fit.
fn align_by_position(slots: &[Slot], ext: &[&TrackPoint], span: f64) -> Option<(f64, f64, usize)> {
    const COARSE: f64 = 0.5;
    const FINE: f64 = 0.05;
    const MIN_PAIRS: usize = 30;
    const TOLERANCE: f64 = 0.5;

    let base: Vec<(f64, f64, f64)> = slots
        .iter()
        .filter_map(|s| {
            s.fix
                .as_ref()
                .map(|f| (s.time.timestamp_micros() as f64 / 1e6, f.lat, f.lon))
        })
        .collect();
    if base.len() < MIN_PAIRS || ext.len() < MIN_PAIRS {
        return None;
    }
    let ext_pts: Vec<(f64, f64, f64)> = ext
        .iter()
        .map(|p| (p.time.unwrap().timestamp_micros() as f64 / 1e6, p.lat, p.lon))
        .collect();
    let ext_times: Vec<f64> = ext_pts.iter().map(|(t, _, _)| *t).collect();

    // Every fifth base fix: a 10 Hz track has far more points than the fit
    // needs, and the cost is linear in them.
    let sampled: Vec<&(f64, f64, f64)> = base.iter().step_by(5).collect();

    let evaluate = |offset: f64| -> Option<(f64, usize)> {
        let mut d = Vec::with_capacity(sampled.len());
        for &&(t, lat, lon) in &sampled {
            if let Some(j) = nearest_within(&ext_times, t - offset, TOLERANCE) {
                d.push(haversine(lat, lon, ext_pts[j].1, ext_pts[j].2));
            }
        }
        if d.len() < MIN_PAIRS {
            return None;
        }
        d.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Some((quantile_sorted(&d, 0.5), d.len()))
    };

    let search = |from: f64, to: f64, step: f64| -> Option<(f64, f64, usize)> {
        let mut best: Option<(f64, f64, usize)> = None;
        let steps = ((to - from) / step).round() as i64;
        for k in 0..=steps {
            let offset = from + k as f64 * step;
            if let Some((median, n)) = evaluate(offset) {
                if best.is_none() || median < best.unwrap().1 {
                    best = Some((offset, median, n));
                }
            }
        }
        best
    };

    let coarse = search(-span, span, COARSE)?;
    // Refine within one coarse step of the winner.
    let fine = search(coarse.0 - COARSE, coarse.0 + COARSE, FINE);
    Some(match fine {
        Some(f) if f.1 <= coarse.1 => f,
        _ => coarse,
    })
}

/// How noisy a source's positions are, as a dimensionless dispersion.
///
/// A receiver reports Doppler speed far more accurately than it reports
/// position, so the ratio of position-differenced speed to reported speed is a
/// self-check that needs no external truth: a clean track sits at 1.0 with
/// little spread, and a jumpy one spreads out. The spread is measured as
/// p90 − p10 so that a handful of wild fixes cannot dominate it.
///
/// Measured on the reference material: the phone scores 0.07 (p10 0.977,
/// p50 1.006, p90 1.046) and the camera 0.76 (p10 0.771, p50 1.008, p90 1.526).
/// An order of magnitude apart, and it is why the camera's own DOP — pinned
/// between 3.04 and 3.31 for every fix it accepts — is not a usable gate on
/// its own.
fn positional_dispersion(points: &[(DateTime<Utc>, &TrackPoint)]) -> Option<f64> {
    let mut ratios = Vec::new();
    for w in points.windows(2) {
        let dt = (w[1].0 - w[0].0).num_microseconds()? as f64 / 1e6;
        // Skip a coverage gap: the bike really did travel across it.
        if dt <= 0.0 || dt > 2.0 {
            continue;
        }
        let Some(reported) = w[1].1.speed_ms else { continue };
        // Below walking pace the ratio is dominated by receiver noise on both
        // sides and says nothing about track quality.
        if reported < 5.0 {
            continue;
        }
        let moved = haversine(w[0].1.lat, w[0].1.lon, w[1].1.lat, w[1].1.lon);
        ratios.push(moved / dt / reported);
    }
    // Too few samples to characterise a distribution.
    if ratios.len() < 100 {
        return None;
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(quantile_sorted(&ratios, 0.9) - quantile_sorted(&ratios, 0.1))
}

/// Index of the entry in a sorted time series nearest `t`, within `tolerance`.
fn nearest_within(times: &[f64], t: f64, tolerance: f64) -> Option<usize> {
    if times.is_empty() {
        return None;
    }
    let i = times.partition_point(|&x| x < t);
    let mut best: Option<usize> = None;
    for cand in [i.checked_sub(1), (i < times.len()).then_some(i)]
        .into_iter()
        .flatten()
    {
        let d = (times[cand] - t).abs();
        if d <= tolerance && best.map(|b| d < (times[b] - t).abs()).unwrap_or(true) {
            best = Some(cand);
        }
    }
    best
}

/// Interpolate an external position at `time`.
///
/// Requires points either side within `max_dt`; extrapolating past the ends of
/// the external track would invent positions where it has nothing to say.
/// Position follows the great circle, elevation and speed blend linearly and
/// course takes the shortest arc.
fn interpolate_at(
    ext: &[(DateTime<Utc>, &TrackPoint)],
    time: DateTime<Utc>,
    max_dt: f64,
) -> Option<TrackPoint> {
    if ext.is_empty() {
        return None;
    }
    let i = ext.partition_point(|(t, _)| *t < time);

    // Exactly on a point, or close enough that interpolation is moot.
    if let Some((t, p)) = ext.get(i) {
        if (*t - time).num_milliseconds().abs() < 1 {
            return Some((*p).clone());
        }
    }

    let before = i.checked_sub(1).map(|j| &ext[j]);
    let after = ext.get(i);
    let (Some((t0, p0)), Some((t1, p1))) = (before, after) else {
        return None; // outside the external track's coverage
    };

    let dt0 = (time - *t0).num_microseconds()? as f64 / 1e6;
    let dt1 = (*t1 - time).num_microseconds()? as f64 / 1e6;
    if dt0 > max_dt || dt1 > max_dt {
        return None; // the nearest external points are too far away in time
    }
    let span = dt0 + dt1;
    let f = if span > 0.0 { dt0 / span } else { 0.0 };

    let (lat, lon) = interpolate_great_circle(p0.lat, p0.lon, p1.lat, p1.lon, f);
    let blend = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(x), Some(y)) => Some(x + (y - x) * f),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    };

    Some(TrackPoint {
        lat,
        lon,
        ele_m: blend(p0.ele_m, p1.ele_m),
        time: Some(time),
        hdop: blend(p0.hdop, p1.hdop),
        speed_ms: blend(p0.speed_ms, p1.speed_ms),
        course_deg: match (p0.course_deg, p1.course_deg) {
            // Shortest arc, so a heading crossing north does not swing the
            // long way round through the interpolation.
            (Some(a), Some(b)) => Some((a + angle_delta(a, b) * f).rem_euclid(360.0)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        },
        lean_deg: blend(p0.lean_deg, p1.lean_deg),
        accel_ms2: blend(p0.accel_ms2, p1.accel_ms2),
        at_sensor_limit: p0.at_sensor_limit && p1.at_sensor_limit,
        source: None,
        video_time_s: None,
    })
}
