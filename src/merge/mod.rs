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
use crate::telemetry::{GpsFix, Source, Telemetry};

/// The weakest lean-angle correlation still trusted as an alignment.
///
/// The reference clip peaks at `|r| = 0.89` and falls to 0.52 four seconds
/// either side of the peak, so a best match below 0.5 is no sharper than being
/// several seconds wrong on a good ride — the tracks are probably not the same
/// ride, or overlap too little to say.
const MIN_LEAN_R: f64 = 0.5;

/// Longest gap in a GPX base that gets empty slots for substitution.
///
/// Lost lock lasts seconds to a few minutes; the reference clip's longest
/// outage is well inside this. A gap longer than ten minutes is a stop, a
/// paused recording or a bogus timestamp, and filling it at a 10 Hz cadence
/// would manufacture thousands of slots — or, for a timestamp a year out,
/// billions.
const MAX_FILL_GAP_S: f64 = 600.0;

/// Interpolate across a bracketing gap of up to this many of the source's own
/// sample intervals, however small `max_dt` is.
///
/// A 1 Hz phone track with ordinary jitter has neighbours 1.0–1.3 s apart, and
/// a fixed one-second limit on the gap would refuse to interpolate most of it.
/// Two and a half intervals tolerates a single dropped sample and no more.
const BRACKET_CADENCES: f64 = 2.5;

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
    /// Search this many seconds either side of zero for the clock offset. The
    /// positional fit that measures the camera's GPS lag searches the same
    /// distance either side of the clock offset the lean fit found.
    pub span: f64,
    /// How far in time a position may be taken from, in seconds.
    ///
    /// A source's position at an instant is interpolated between the samples
    /// either side when they are no further apart than `max_dt` or
    /// [`BRACKET_CADENCES`] of the source's own median interval, whichever is
    /// larger — so a 1 Hz source interpolates as readily as a 10 Hz one.
    /// Across a longer gap, or at the ends of a track, the nearest sample is
    /// used if it is within `max_dt`, and otherwise nothing is.
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
    /// it could not be measured.
    pub lean_polarity_agrees: Option<bool>,
    /// False when the automatic alignment is weak or ran to the edge of its
    /// search, in which case `warnings` says which.
    pub alignment_reliable: bool,
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
    /// Points placed by interpolating the camera's track across fixes it
    /// rejected, where the external source could not help.
    pub points_interpolated: usize,
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
                     alignment is unaffected, and it is negated wherever it fills a gap in ours\n",
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
             {} interpolated across rejected camera fixes, {} still unpositioned\n",
            self.points_total,
            self.points_gopro,
            self.points_external,
            self.points_interpolated,
            self.points_unpositioned
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
            video_time_s: t.video_time_of(fix.time),
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
    /// A backstop against a pathological cadence: a track with duplicate
    /// timestamps a millisecond apart would otherwise fill ten minutes with
    /// six hundred thousand slots.
    const MAX_FILL_SLOTS: i64 = 100_000;

    let mut timed = base.timed_points();
    if timed.is_empty() {
        return Vec::new();
    }
    // The reader sorts, but a `Gpx` built in code need not have been read.
    timed.sort_by_key(|p| p.time);

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
            // Bounded above as well as below: see `MAX_FILL_GAP_S`. The merge
            // reports gaps it left unfilled, since it sees the slots too.
            let steps = (gap / cadence).round() as i64;
            if gap > cadence * 2.5 && gap <= MAX_FILL_GAP_S && steps <= MAX_FILL_SLOTS {
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
/// were repaired; see [`apply_to_fixes`] for what else changes.
pub fn apply_to_telemetry(t: &mut Telemetry, merged: &Gpx) -> usize {
    let clocked = t.t0.is_some();
    let origin = t.times.first().copied().unwrap_or(0.0);
    apply_to_fixes(&mut t.gps, origin, clocked, merged)
}

/// The body of [`apply_to_telemetry`], on the fixes alone.
///
/// `origin` is the timeline's first timestamp, which a slot's video time is
/// measured from (see [`Telemetry::video_time_of`]); on a clip cut from a
/// longer recording it is where the cut began, not zero.
///
/// `clocked` says whether the telemetry had a clock to date fixes that carry
/// no UTC of their own; a fix with neither never became a slot, so the merge
/// had no say over it and it is left alone.
///
/// Every fix that became a slot is made to agree with the merged track. One
/// the merge placed takes its position. One the merge left out while the fix
/// itself was valid is invalidated: that only happens when a GPS-lag
/// correction re-timed the camera's positions and this fix's instant fell off
/// the end of them, and keeping its uncorrected position would put a point
/// tens of metres behind its corrected neighbours. Such a fix is left without
/// a reject reason, since none of the receiver-quality reasons applies — its
/// position was sound, only too stale to use at its own instant.
pub fn apply_to_fixes(fixes: &mut [GpsFix], origin: f64, clocked: bool, merged: &Gpx) -> usize {
    // Video times of the camera's own fixes, ascending, for bisection.
    let fix_times: Vec<f64> = fixes.iter().map(|f| f.time - origin).collect();
    let mut placed = vec![false; fixes.len()];
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
        placed[j] = true;
        let fix = &mut fixes[j];
        // A position interpolated across the camera's own rejected fixes is
        // not a measurement, so it does not revive a fix the quality gate
        // rejected; the fix stays as it was found, with its reason intact.
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
    for (fix, placed) in fixes.iter_mut().zip(placed) {
        let had_slot = fix.utc.is_some() || clocked;
        if fix.valid && had_slot && !placed {
            fix.valid = false;
            fix.reject = None;
        }
    }
    repaired
}

/// Merge an external GPX into a base timeline.
pub fn merge(slots: &[Slot], external: &Gpx, opts: &Options) -> (Gpx, Report) {
    let mut ext: Vec<&TrackPoint> = external.timed_points();
    // The reader sorts, but a `Gpx` built in code need not have been read, and
    // every search below bisects.
    ext.sort_by_key(|p| p.time);
    // Problems found reading the external file bear on the merge — a point
    // whose time could not be parsed is a point that cannot be substituted —
    // so they are reported with it.
    let mut warnings: Vec<String> = external
        .warnings
        .iter()
        .map(|w| format!("external GPX: {w}"))
        .collect();

    if ext.is_empty() {
        warnings.push("the external GPX has no timestamped points; nothing to merge".into());
    }

    // Gaps in the base too long to have been given slots cannot be filled, so
    // say so rather than let them pass as coverage the external source lacked.
    let long_gaps: Vec<(DateTime<Utc>, f64)> = slots
        .windows(2)
        .map(|w| (w[0].time, secs(w[1].time - w[0].time)))
        .filter(|(_, g)| *g > MAX_FILL_GAP_S)
        .collect();
    if let Some(&(at, longest)) = long_gaps
        .iter()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
    {
        warnings.push(format!(
            "the base track has {} gap(s) longer than {:.0} min, the longest {:.0} min from {}; \
             they were left unfilled, since a gap that long is a stop or a bad timestamp rather \
             than lost lock",
            long_gaps.len(),
            MAX_FILL_GAP_S / 60.0,
            longest / 60.0,
            at.format("%Y-%m-%d %H:%M:%S")
        ));
    }

    // --- align ------------------------------------------------------------
    let unmeasured = |method: &str| Alignment {
        offset: 0.0,
        method: method.to_string(),
        score: 0.0,
        samples: 0,
        lean_polarity: None,
        lag: None,
        reliable: true,
    };
    let alignment = match opts.align {
        Align::None => unmeasured("assumed identical clocks"),
        Align::Fixed(o) => Alignment {
            offset: o,
            ..unmeasured("given on the command line")
        },
        Align::Auto if ext.is_empty() => Alignment {
            reliable: false,
            ..unmeasured("not attempted (no external points)")
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
        // within the noise of the fits that measured it. Nor one measured
        // against an alignment that is itself in doubt, since the lag is the
        // difference between two fits and inherits the error of both.
        Lag::Auto => match alignment.lag {
            Some(l) if l.abs() > 0.5 && alignment.reliable => (Some(l), true),
            other => (other, false),
        },
    };
    let applied_lag = if lag_applied { lag.unwrap_or(0.0) } else { 0.0 };

    // Both sources now go through one interpolation path, on one clock. The
    // camera's sequence number is its slot index, so an interpolation that
    // skips a slot is one that bridged a fix the camera rejected.
    let camera = Series::new(
        slots
            .iter()
            .enumerate()
            .filter_map(|(k, s)| {
                s.fix.as_ref().map(|f| SeriesPoint {
                    time: s.time - Duration::microseconds((applied_lag * 1e6).round() as i64),
                    point: f,
                    seq: k,
                })
            })
            .collect(),
        opts.max_dt,
    );
    let external_series = Series::new(
        ext.iter()
            .enumerate()
            .map(|(k, p)| SeriesPoint {
                time: p.time.unwrap() + Duration::microseconds((offset * 1e6).round() as i64),
                point: p,
                seq: k,
            })
            .collect(),
        opts.max_dt,
    );

    if lag_applied {
        // The displacement is quoted at this ride's own median speed, which
        // is what the lag actually cost it.
        let median_speed = median_speed(&camera).or_else(|| median_speed(&external_series));
        warnings.push(match median_speed {
            Some(v) => format!(
                "the camera's GPS positions lag their own timestamps by {:+.2} s and have been \
                 re-timed by that much. At this ride's median speed of {:.0} km/h that is a \
                 displacement of about {:.0} m.",
                applied_lag,
                v * 3.6,
                applied_lag.abs() * v
            ),
            None => format!(
                "the camera's GPS positions lag their own timestamps by {applied_lag:+.2} s and \
                 have been re-timed by that much."
            ),
        });
    }

    // --- measure what disagreement is left --------------------------------
    let mut separations = Vec::new();
    for slot in slots {
        if let (Some((a, _)), Some((b, _))) = (camera.at(slot.time), external_series.at(slot.time))
        {
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
    let camera_dispersion = positional_dispersion(&camera.pts);
    let external_dispersion = positional_dispersion(&external_series.pts);
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

    // --- how to use the external lean -------------------------------------
    // The external source's lean only ever fills a gap in ours, and it must
    // arrive in our sign convention or a repaired gap would lean the wrong
    // way. The polarity is measured at the offset actually used, whichever way
    // that offset was chosen; when it cannot be measured confidently and we
    // have lean of our own to disagree with, the external lean is dropped
    // rather than risk mixing the conventions.
    let base_has_lean = slots.iter().any(|s| s.lean_deg.is_some());
    let polarity = lean_correlation(slots, &ext, offset).map(|(r, _)| r);
    let external_lean_sign = match polarity {
        _ if !base_has_lean => Some(1.0),
        Some(r) if r.abs() >= MIN_LEAN_R => Some(r.signum()),
        _ => None,
    };
    let lean_polarity_agrees = alignment
        .lean_polarity
        .or_else(|| polarity.filter(|r| r.abs() >= MIN_LEAN_R).map(|r| r > 0.0));

    // --- substitute -------------------------------------------------------
    let mut points = Vec::with_capacity(slots.len());
    let mut n_gopro = 0usize;
    let mut n_external = 0usize;
    let mut n_interpolated = 0usize;
    let mut n_missing = 0usize;

    for slot in slots {
        // A camera position is a measurement when it is the fix itself or
        // lies between two consecutive fixes; one bridging a fix the camera
        // rejected is an interpolation, and is labelled as one.
        let from_camera = camera.at(slot.time).map(|(p, measured)| {
            (p, if measured { Source::Gopro } else { Source::Interpolated })
        });
        let from_external = external_series.at(slot.time).map(|(p, _)| (p, Source::External));
        let measured_camera = from_camera.clone().filter(|(_, s)| *s == Source::Gopro);

        // The gate. Under `QualityGated` the camera keeps the point wherever it
        // had a position good enough to survive the quality checks, the
        // external source fills in everywhere else, and bridging the camera's
        // own gap is the last resort: a real measurement from the other
        // receiver beats a guess from this one.
        let external_first = matches!(opts.prefer, Prefer::External) || prefer_external_on_quality;
        let chosen = match opts.prefer {
            Prefer::Gopro => from_camera,
            _ if external_first => from_external.or(from_camera),
            _ => measured_camera.or(from_external).or(from_camera),
        };

        match chosen {
            Some((mut p, source)) => {
                // The camera's IMU channels are unaffected by GPS quality, so
                // they survive a substitution. An external source's own lean is
                // used only where we have none of our own, and in our sign
                // convention. Longitudinal acceleration needs no such care: no
                // alignment measures its polarity, and forward being positive
                // is not something producers disagree on the way they do about
                // which side lean is positive.
                let own_lean = if source == Source::External {
                    p.lean_deg
                        .and_then(|l| external_lean_sign.map(|sign| sign * l))
                } else {
                    p.lean_deg
                };
                p.time = Some(slot.time);
                p.lean_deg = slot.lean_deg.or(own_lean);
                p.accel_ms2 = slot.accel_ms2.or(p.accel_ms2);
                p.video_time_s = slot.video_time_s;
                p.source = Some(source.as_str().to_string());
                // The merged track is one continuous timeline.
                p.segment = 0;
                match source {
                    Source::External => n_external += 1,
                    Source::Interpolated => n_interpolated += 1,
                    Source::Gopro => n_gopro += 1,
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
        lean_polarity_agrees,
        alignment_reliable: alignment.reliable,
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
        points_interpolated: n_interpolated,
        points_unpositioned: n_missing,
        external_points: ext.len(),
        external_span: external.time_span(),
        warnings,
    };

    let mut metadata = vec![
        ("merge_method".to_string(), report.method.clone()),
        ("merge_offset_s".to_string(), fmt_num(offset, 3)),
        ("merge_score".to_string(), fmt_num(report.score, 4)),
        (
            "merge_alignment_reliable".to_string(),
            report.alignment_reliable.to_string(),
        ),
        ("merge_points_total".to_string(), points.len().to_string()),
        ("merge_points_gopro".to_string(), n_gopro.to_string()),
        ("merge_points_external".to_string(), n_external.to_string()),
        (
            "merge_points_interpolated".to_string(),
            n_interpolated.to_string(),
        ),
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
        warnings: Vec::new(),
    };
    (merged, report)
}

/// A duration in seconds, as a float.
fn secs(d: Duration) -> f64 {
    d.num_microseconds()
        .map(|us| us as f64 / 1e6)
        .unwrap_or(if d < Duration::zero() { f64::NEG_INFINITY } else { f64::INFINITY })
}

/// Median reported speed of a series, in m/s.
fn median_speed(series: &Series) -> Option<f64> {
    let mut v: Vec<f64> = series
        .pts
        .iter()
        .filter_map(|p| p.point.speed_ms)
        .filter(|s| s.is_finite())
        .collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(quantile_sorted(&v, 0.5))
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
    /// False when the fit is weak or its optimum sat at the edge of the
    /// search, so the true answer may lie outside it.
    reliable: bool,
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
///
/// Neither search can see past its own window, so an optimum sitting on the
/// window's edge is reported as such rather than trusted: the true value is
/// probably beyond it, and the edge is merely the closest the search got.
fn align(
    slots: &[Slot],
    ext: &[&TrackPoint],
    span: f64,
    warnings: &mut Vec<String>,
) -> Alignment {
    let base_has_lean = slots.iter().any(|s| s.lean_deg.is_some());
    let ext_has_lean = ext.iter().any(|p| p.lean_deg.is_some());

    let lean = if base_has_lean && ext_has_lean {
        align_by_lean(slots, ext, span)
    } else {
        None
    };

    // The positional fit measures clock offset plus GPS lag, and the lag is a
    // few seconds at most, so it is searched for around the lean offset.
    // Centred on zero instead, a large clock offset pushes the positional
    // optimum out of the window and the lag is clipped to wherever the
    // window's edge happens to fall.
    let centre = lean.as_ref().map(|l| l.offset).unwrap_or(0.0);
    let positional = align_by_position(slots, ext, centre, span);

    if let Some(l) = lean {
        let mut reliable = true;
        if l.r.abs() < MIN_LEAN_R {
            reliable = false;
            warnings.push(format!(
                "lean-angle alignment is weak (best |r| = {:.2} at {:+.1} s, against about 0.9 \
                 on a good match): the tracks may not be the same ride or may barely overlap. \
                 Check the result, or pass --time-offset.",
                l.r.abs(),
                l.offset
            ));
        }
        if l.at_edge {
            reliable = false;
            warnings.push(format!(
                "the best lean-angle match, at {:+.1} s, sits at the edge of the ±{span:.0} s \
                 search, so the true offset is probably outside it. Widen the search with \
                 --align-span or pass --time-offset.",
                l.offset
            ));
        }
        let lag = match positional {
            Some(p) if p.at_edge => {
                warnings.push(format!(
                    "the positional fit reached the edge of its ±{span:.0} s search around the \
                     lean offset, so the camera's GPS lag could not be measured"
                ));
                None
            }
            Some(p) => Some(p.offset - l.offset),
            None => None,
        };
        return Alignment {
            offset: l.offset,
            method: "lean-angle correlation".to_string(),
            score: l.r.abs(),
            samples: l.samples,
            lean_polarity: Some(l.r > 0.0),
            lag,
            reliable,
        };
    }

    if base_has_lean && ext_has_lean {
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
        Some(p) => {
            if p.at_edge {
                warnings.push(format!(
                    "the best positional match, at {:+.1} s, sits at the edge of the ±{span:.0} s \
                     search, so the true offset is probably outside it. Widen the search with \
                     --align-span or pass --time-offset.",
                    p.offset
                ));
            }
            Alignment {
                offset: p.offset,
                method: "positional fit".to_string(),
                score: p.metres,
                samples: p.samples,
                lean_polarity: None,
                // A positional fit has already absorbed the lag into the
                // offset, so there is nothing left to correct separately.
                lag: None,
                reliable: !p.at_edge,
            }
        }
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
                reliable: false,
            }
        }
    }
}

/// A lean-angle series as `(unix seconds, degrees)`.
type LeanSeries = Vec<(f64, f64)>;

/// The lean-angle series of both sources.
fn lean_series(slots: &[Slot], ext: &[&TrackPoint]) -> (LeanSeries, LeanSeries) {
    let base = slots
        .iter()
        .filter_map(|s| s.lean_deg.map(|l| (s.time.timestamp_micros() as f64 / 1e6, l)))
        .collect();
    let ext_lean = ext
        .iter()
        .filter_map(|p| {
            p.lean_deg
                .map(|l| (p.time.unwrap().timestamp_micros() as f64 / 1e6, l))
        })
        .collect();
    (base, ext_lean)
}

/// A tenth of a second: fine enough to place a corner, coarse enough that the
/// lean search stays cheap.
const LEAN_STEP: f64 = 0.1;
/// Accept a base lean sample within this of the shifted external instant.
const LEAN_TOLERANCE: f64 = 0.2;
/// Below this many paired lean samples any offset can be made to fit.
const LEAN_MIN_PAIRS: usize = 100;

/// Pearson's `r` between the two lean series at one offset, and the number of
/// pairs it rests on.
fn lean_correlation_of(
    base: &[(f64, f64)],
    base_times: &[f64],
    ext_lean: &[(f64, f64)],
    offset: f64,
) -> Option<(f64, usize)> {
    let mut xs = Vec::with_capacity(ext_lean.len());
    let mut ys = Vec::with_capacity(ext_lean.len());
    for &(t, lean) in ext_lean {
        if let Some(j) = nearest_within(base_times, t + offset, LEAN_TOLERANCE) {
            xs.push(lean);
            ys.push(base[j].1);
        }
    }
    (xs.len() >= LEAN_MIN_PAIRS).then(|| (pearson(&xs, &ys), xs.len()))
}

/// [`lean_correlation_of`] straight from the slots and external points.
fn lean_correlation(slots: &[Slot], ext: &[&TrackPoint], offset: f64) -> Option<(f64, usize)> {
    let (base, ext_lean) = lean_series(slots, ext);
    let base_times: Vec<f64> = base.iter().map(|(t, _)| *t).collect();
    lean_correlation_of(&base, &base_times, &ext_lean, offset)
}

/// The outcome of the lean-angle search.
struct LeanFit {
    offset: f64,
    /// Signed, so the caller can report whether the polarities agree.
    r: f64,
    samples: usize,
    /// The optimum sat on the edge of the search window.
    at_edge: bool,
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
fn align_by_lean(slots: &[Slot], ext: &[&TrackPoint], span: f64) -> Option<LeanFit> {
    let (base, ext_lean) = lean_series(slots, ext);
    if base.len() < LEAN_MIN_PAIRS || ext_lean.len() < LEAN_MIN_PAIRS {
        return None;
    }

    let base_times: Vec<f64> = base.iter().map(|(t, _)| *t).collect();
    let mut best: Option<LeanFit> = None;
    let steps = (2.0 * span / LEAN_STEP).round() as i64;

    for k in 0..=steps {
        let offset = -span + k as f64 * LEAN_STEP;
        let Some((r, n)) = lean_correlation_of(&base, &base_times, &ext_lean, offset) else {
            continue;
        };
        if best.as_ref().map(|b| r.abs() > b.r.abs()).unwrap_or(true) {
            best = Some(LeanFit {
                offset,
                r,
                samples: n,
                at_edge: k == 0 || k == steps,
            });
        }
    }
    best
}

/// The outcome of the positional search.
#[derive(Clone, Copy)]
struct PositionFit {
    offset: f64,
    /// Median separation at the optimum.
    metres: f64,
    samples: usize,
    /// The coarse optimum sat on the edge of the search window.
    at_edge: bool,
}

/// Align by minimising the median separation between the two tracks, searching
/// `span` seconds either side of `centre`.
///
/// The fallback, for an external source with no lean angle, and the measure
/// of GPS lag for one with it. Coarse then fine, and it uses the *median*
/// rather than the mean so that a handful of wild camera positions cannot
/// drag the fit.
fn align_by_position(
    slots: &[Slot],
    ext: &[&TrackPoint],
    centre: f64,
    span: f64,
) -> Option<PositionFit> {
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

    let search = |from: f64, to: f64, step: f64| -> Option<PositionFit> {
        let mut best: Option<PositionFit> = None;
        let steps = ((to - from) / step).round() as i64;
        for k in 0..=steps {
            let offset = from + k as f64 * step;
            if let Some((metres, samples)) = evaluate(offset) {
                if best.map(|b| metres < b.metres).unwrap_or(true) {
                    best = Some(PositionFit {
                        offset,
                        metres,
                        samples,
                        at_edge: k == 0 || k == steps,
                    });
                }
            }
        }
        best
    };

    let coarse = search(centre - span, centre + span, COARSE)?;
    // Refine within one coarse step of the winner. The edge that matters is
    // the coarse search's: the fine one's edges are merely a step either side.
    let fine = search(coarse.offset - COARSE, coarse.offset + COARSE, FINE);
    Some(match fine {
        Some(f) if f.metres <= coarse.metres => PositionFit {
            at_edge: coarse.at_edge,
            ..f
        },
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
fn positional_dispersion(points: &[SeriesPoint]) -> Option<f64> {
    let mut ratios = Vec::new();
    for w in points.windows(2) {
        let dt = secs(w[1].time - w[0].time);
        // Skip a coverage gap: the bike really did travel across it.
        if dt <= 0.0 || dt > 2.0 {
            continue;
        }
        let Some(reported) = w[1].point.speed_ms else { continue };
        // Below walking pace the ratio is dominated by receiver noise on both
        // sides and says nothing about track quality.
        if reported < 5.0 {
            continue;
        }
        let moved = haversine(w[0].point.lat, w[0].point.lon, w[1].point.lat, w[1].point.lon);
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

/// One position in a [`Series`].
struct SeriesPoint<'a> {
    /// On the base clock, after any offset or lag correction.
    time: DateTime<Utc>,
    point: &'a TrackPoint,
    /// Position in the source's own sequence. Two neighbours whose numbers
    /// are not consecutive have something missing between them.
    seq: usize,
}

/// A time-ordered position track that positions can be read off at any
/// instant — the one interpolation path both sources go through.
struct Series<'a> {
    pts: Vec<SeriesPoint<'a>>,
    max_dt: f64,
    /// The longest bracketing gap interpolated across; see [`Options::max_dt`].
    bracket_limit: f64,
}

impl<'a> Series<'a> {
    fn new(pts: Vec<SeriesPoint<'a>>, max_dt: f64) -> Self {
        // The source's own cadence, measured rather than assumed, and only
        // within segments so that a logging break does not inflate it.
        let mut deltas: Vec<f64> = pts
            .windows(2)
            .filter(|w| w[0].point.segment == w[1].point.segment)
            .map(|w| secs(w[1].time - w[0].time))
            .filter(|d| *d > 0.0)
            .collect();
        deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let cadence = if deltas.is_empty() {
            0.0
        } else {
            quantile_sorted(&deltas, 0.5)
        };
        Series {
            pts,
            max_dt,
            bracket_limit: max_dt.max(BRACKET_CADENCES * cadence),
        }
    }

    /// The position at `time`, and whether it is a measurement: the sample
    /// itself or an interpolation between consecutive samples, as opposed to
    /// one bridging missing samples or holding the nearest one.
    ///
    /// Never extrapolates, and never interpolates across a segment boundary.
    /// Position follows the great circle, elevation and speed blend linearly
    /// and course takes the shortest arc.
    fn at(&self, time: DateTime<Utc>) -> Option<(TrackPoint, bool)> {
        let pts = &self.pts;
        if pts.is_empty() {
            return None;
        }
        let i = pts.partition_point(|p| p.time < time);

        // Exactly on a point, or close enough that interpolation is moot.
        if let Some(p) = pts.get(i) {
            if secs(p.time - time).abs() < 1e-3 {
                return Some((p.point.clone(), true));
            }
        }

        let before = i.checked_sub(1).map(|j| &pts[j]);
        let after = pts.get(i);
        if let (Some(a), Some(b)) = (before, after) {
            let gap = secs(b.time - a.time);
            if a.point.segment == b.point.segment && gap <= self.bracket_limit {
                let f = if gap > 0.0 { secs(time - a.time) / gap } else { 0.0 };
                let mut p = blend(a.point, b.point, f);
                p.time = Some(time);
                return Some((p, b.seq == a.seq + 1));
            }
        }

        // Too far apart to interpolate, or past an end: hold the nearest
        // sample if it is close enough in time to stand for this instant.
        [before, after]
            .into_iter()
            .flatten()
            .map(|p| (p, secs(p.time - time).abs()))
            .filter(|(_, d)| *d <= self.max_dt)
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .map(|(p, _)| {
                let mut q = p.point.clone();
                q.time = Some(time);
                (q, false)
            })
    }
}

/// The point a fraction `f` of the way from `p0` to `p1`.
fn blend(p0: &TrackPoint, p1: &TrackPoint, f: f64) -> TrackPoint {
    let (lat, lon) = interpolate_great_circle(p0.lat, p0.lon, p1.lat, p1.lon, f);
    let mix = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(x), Some(y)) => Some(x + (y - x) * f),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    };
    TrackPoint {
        lat,
        lon,
        ele_m: mix(p0.ele_m, p1.ele_m),
        time: None,
        hdop: mix(p0.hdop, p1.hdop),
        speed_ms: mix(p0.speed_ms, p1.speed_ms),
        course_deg: match (p0.course_deg, p1.course_deg) {
            // Shortest arc, so a heading crossing north does not swing the
            // long way round through the interpolation.
            (Some(a), Some(b)) => Some((a + angle_delta(a, b) * f).rem_euclid(360.0)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        },
        lean_deg: mix(p0.lean_deg, p1.lean_deg),
        accel_ms2: mix(p0.accel_ms2, p1.accel_ms2),
        at_sensor_limit: p0.at_sensor_limit && p1.at_sensor_limit,
        source: None,
        video_time_s: None,
        segment: p0.segment,
    }
}
