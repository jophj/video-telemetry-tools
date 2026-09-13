//! Turn GPMF streams into a single time-aligned set of rider channels.
//!
//! The camera gives orientation (`GRAV`, 29.97 Hz), rotation rate (`GYRO`,
//! 806 Hz), acceleration (`ACCL`, 201 Hz) and position (`GPS9`, 10 Hz), each on
//! its own clock. This module resolves them onto one timeline and derives what
//! a rider actually cares about: lean angle, pitch, yaw rate, and longitudinal,
//! lateral and vertical acceleration in the vehicle's own frame.
//!
//! The hard part is not the trigonometry, it is deciding which way is forward
//! and which way is right, because a camera can be mounted at any angle. That
//! is done by calibrating against gravity over the straight-line parts of the
//! ride, then resolving the remaining sign ambiguities against an *external*
//! reference — GPS. See [`resolve_lean_sign`] for why that last step matters.

pub mod output;

use std::collections::BTreeMap;

use chrono::{DateTime, TimeZone, Utc};

use crate::fourcc::FourCc;
use crate::gpmf::{self, Gpmf, Series};
use crate::math::*;

/// GoPro timestamps `GPS9` against 2000-01-01, not the 1980 GPS epoch.
const GPS_EPOCH_UNIX: i64 = 946_684_800;

/// `GPS9` reports this DOP when it has nothing to report.
const DOP_SENTINEL: f64 = 99.98;

/// A sample counts as upright reference material above this speed, in m/s.
///
/// Below walking pace the machine is being held up by a foot rather than by
/// its own momentum, and gravity in the mount frame says nothing about which
/// way the vehicle considers up.
const UPRIGHT_MIN_SPEED_MS: f64 = 5.0;

/// ...and below this rate of turn, in degrees per second.
const UPRIGHT_MAX_TURN_DPS: f64 = 2.0;

/// Half-width of the window each roll-zero point is taken over, in seconds.
///
/// Long enough to span the corners either side of a straight, short enough to
/// follow a mount that creeps over a twenty-minute ride.
const ROLL_ZERO_HALF_WINDOW_S: f64 = 30.0;

/// Widened half-width, used where the narrow window is too sparse.
const ROLL_ZERO_WIDE_HALF_WINDOW_S: f64 = 90.0;

/// Upright samples one baseline point needs before the narrow window is used.
const ROLL_ZERO_MIN_LOCAL: usize = 40;

/// Upright samples a baseline point needs before it is preferred to the
/// whole-file median.
const ROLL_ZERO_MIN_WIDE: usize = 10;

/// Upright samples the file needs before the roll zero is tracked at all.
const ROLL_ZERO_MIN_TOTAL: usize = 200;

/// Spacing of the baseline grid, in seconds.
///
/// The baseline is a median over a minute of riding, so it cannot move
/// meaningfully between adjacent 30 Hz samples. Evaluating it once a second
/// and interpolating turns a per-sample sort into a per-second one.
const ROLL_ZERO_GRID_S: f64 = 1.0;

/// Baseline points averaged together to take the step out of the grid.
const ROLL_ZERO_SMOOTH: usize = 11;

/// Drift across the ride, in degrees, beyond which the mount is worth a word.
const ROLL_ZERO_DRIFT_WARN_DEG: f64 = 4.0;

/// Longitudinal acceleration below which the machine counts as coasting, in
/// m/s².
///
/// Loose, because it is measured by differentiating GPS speed and that is a
/// noisy thing to do. It does not need to be tight: what it has to exclude is
/// hard braking and hard drive, and what it is used for tolerates the rest.
const COAST_MAX_ACCEL_MS2: f64 = 0.3;

/// Coasting samples needed before the gravity reference is trusted.
const GRAV_TILT_MIN_SAMPLES: usize = 200;

/// A `GRAV`-to-accelerometer disagreement beyond this, in degrees, is not a
/// sensor misalignment but a sign that one of the two streams is not what this
/// code thinks it is. It is reported and not applied.
const GRAV_TILT_MAX_DEG: f64 = 30.0;

/// A tilt worth mentioning in the quality report, in degrees.
const GRAV_TILT_WARN_DEG: f64 = 2.0;

#[derive(Debug, Clone)]
pub struct Options {
    /// Reject fixes whose dilution of precision is at or above this.
    pub max_dop: f64,
    /// Reject a fix whose position is bit-identical to this many predecessors.
    ///
    /// On losing lock the camera keeps emitting samples at 10 Hz with the last
    /// position frozen in place. A real receiver jitters in the seventh decimal
    /// even at a standstill, so an exactly repeated position over seconds means
    /// the fix is stale, not that the bike is parked.
    pub frozen_run: usize,
    /// Half-width of the box filter applied to `GYRO` before use.
    pub gyro_window_s: f64,
    /// Half-width of the box filter applied to `ACCL` before use.
    ///
    /// Deliberately ten times the gyro window. A motorcycle puts multi-g
    /// vibration through its accelerometer, so a short window measures road
    /// buzz rather than the load on the machine.
    pub accel_window_s: f64,
    /// Reject a fix that moved this many times faster than the receiver's own
    /// reported speed says it could have. Zero disables the check.
    ///
    /// This catches what dilution of precision cannot. On the reference clip
    /// the camera reports a DOP between 3.04 and 3.31 for every fix it accepts
    /// -- a near-constant value that plainly is not a live quality estimate --
    /// while its position track jumps: a tenth of the steps imply a speed more
    /// than 50 % above the Doppler speed the same receiver reports, peaking at
    /// 1646 km/h. Doppler speed is reliable where position is not (their median
    /// ratio is 1.008), so it makes a good independent reference.
    pub max_speed_ratio: f64,
    /// Allowance added to the speed limit above, in m/s, so that ordinary
    /// receiver noise between adjacent fixes is not mistaken for a jump.
    pub speed_floor_ms: f64,
    /// Put `GRAV` back on the vertical the accelerometer measures before
    /// deriving anything from it. See [`GravTilt`].
    ///
    /// On by default and there is no good reason to turn it off on real
    /// footage; it exists so that the corrected and uncorrected channels can
    /// be compared, which is how the correction is tested.
    pub correct_grav: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_dop: 5.0,
            frozen_run: 30,
            gyro_window_s: 0.02,
            accel_window_s: 0.2,
            max_speed_ratio: 2.0,
            speed_floor_ms: 10.0,
            correct_grav: true,
        }
    }
}

/// One GPS fix, at the receiver's own rate.
#[derive(Debug, Clone)]
pub struct GpsFix {
    /// Seconds from the start of the recording.
    pub time: f64,
    pub lat: f64,
    pub lon: f64,
    pub alt_m: f64,
    pub speed_2d: f64,
    pub speed_3d: f64,
    pub dop: Option<f64>,
    /// 0 = none, 2 = 2D, 3 = 3D. `None` for the older `GPS5` stream, which does
    /// not report it.
    pub fix: Option<u8>,
    pub utc: Option<DateTime<Utc>>,
    /// Course over ground, derived from movement, not reported by the camera.
    pub course_deg: Option<f64>,
    pub valid: bool,
    /// Why an invalid fix was rejected, for the quality report.
    pub reject: Option<Reject>,
    /// Which source this fix came from once a merge has run.
    pub source: Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    NoFix,
    HighDop,
    NullIsland,
    Frozen,
    Implausible,
}

impl Reject {
    pub fn as_str(self) -> &'static str {
        match self {
            Reject::NoFix => "no fix",
            Reject::HighDop => "dilution of precision too high",
            Reject::NullIsland => "position is exactly 0,0",
            Reject::Frozen => "position frozen after loss of lock",
            Reject::Implausible => "position inconsistent with the receiver's own speed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Source {
    #[default]
    Gopro,
    External,
    Interpolated,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Gopro => "gopro",
            Source::External => "external",
            Source::Interpolated => "interpolated",
        }
    }
}

/// How confident we are that lean is signed the right way round.
#[derive(Debug, Clone)]
pub struct LeanSign {
    /// `|r|` between lean and the rightward turn rate it was signed against.
    /// Strong on real footage: about 0.94.
    pub correlation: f64,
    /// `r` between the gyro-derived turn rate and GPS course rate, i.e. how
    /// well the absolute convention itself was confirmed. `None` when there was
    /// no usable GPS to confirm it with.
    pub gps_correlation: Option<f64>,
    /// Paired samples the decision rested on.
    pub samples: usize,
    pub flipped: bool,
    pub confidence: Confidence,
}

/// The fixed tilt between `GRAV` and the accelerometer's own vertical.
///
/// `GRAV` is the camera's *fused estimate* of which way is down, not a
/// measurement, and on this camera it is several degrees out. Where the
/// machine is rolling straight and neither braking nor driving, the only force
/// acting on it is gravity — so the accelerometer, which measures force, **is**
/// the vertical. On the reference ride the two disagree by a median 8°,
/// steadily, from one end of the recording to the other.
///
/// It matters because of *which* channels trust `GRAV` absolutely. Lean and
/// pitch are angles against a reference fitted from `GRAV` itself, so a fixed
/// tilt cancels out of them entirely. The road-frame acceleration channels do
/// not have that luxury: they resolve force against `GRAV` directly, with
/// nothing to cancel against. So the tilt lands in them whole, and a machine
/// riding dead straight reported 0.09 g across itself — 5° of cornering load
/// that was not there, which is enough to make `accel_lat` disagree with the
/// lean angle it is supposed to corroborate.
///
/// Unlike [`LeanZero`] this is *not* tracked over time, and not because it was
/// easier. The roll zero describes where the camera sits on the machine, which
/// a slipping mount changes. This describes how two sensors inside one camera
/// disagree with each other, which no amount of mount movement can affect — so
/// the whole recording is one measurement of it, and using all of it is the
/// accurate thing to do rather than the lazy one.
#[derive(Debug, Clone)]
pub struct GravTilt {
    /// Size of the correction, in degrees.
    pub angle_deg: f64,
    /// Axis it was applied about, in the camera's frame.
    pub axis: Vec3,
    /// Coasting samples it was measured over.
    pub samples: usize,
    /// False when the tilt was measured but judged too large to be a sensor
    /// misalignment, and so left uncorrected.
    pub applied: bool,
}

/// The slowly-varying roll zero removed from the lean angle.
///
/// [`calibrate_mount`] fits one upright reference for the whole file, so the
/// zero it produces is only right on average. On a real ride the reading a
/// genuinely upright machine produces wanders — the mount creeps, the rider
/// re-seats the camera, the helmet it is stuck to moves on the rider's head —
/// and on the reference footage that wander spans 12° across twenty minutes.
/// A single static zero therefore reports a corner at whatever end of that
/// band it happens to fall in: a 43° left-hander sitting in a −6° pocket comes
/// out as 49°, five degrees clear of a field it should have tied with.
///
/// So the same fit is made again locally: the median lean over the upright
/// samples within [`ROLL_ZERO_HALF_WINDOW_S`] of each instant, subtracted from
/// the lean angle. Upright riding then reads zero wherever it occurs, which is
/// the property the whole-file fit only has on average.
#[derive(Debug, Clone)]
pub struct LeanZero {
    /// Upright samples the baseline was fitted from.
    pub samples: usize,
    /// Half-width of the window each baseline point was taken over, in seconds.
    pub window_s: f64,
    /// Smallest and largest offset removed, in degrees.
    pub range_deg: (f64, f64),
    /// The baseline itself, on the master timeline, already subtracted from
    /// [`Telemetry::lean_deg`]. Add it back to recover the raw angle.
    pub offset_deg: Vec<f64>,
}

impl LeanZero {
    /// How far the zero moved across the ride, in degrees.
    pub fn drift_deg(&self) -> f64 {
        self.range_deg.1 - self.range_deg.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// Confirmed against GPS, and lean tracks the turn rate strongly.
    High,
    /// Both links present but one of them is weak.
    Low,
    /// No usable GPS: the handedness convention is assumed, not measured.
    Assumed,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Low => "low",
            Confidence::Assumed => "assumed",
        }
    }
}

/// The derived rider channels, all on the `GRAV` timeline.
pub struct Telemetry {
    /// Samples on the master timeline.
    pub n: usize,
    /// Measured sample rate of the master timeline, in Hz.
    pub hz: f64,
    /// Nominal rate, i.e. samples per GPMF payload — 30 where `hz` is 29.97.
    pub hz_nominal: f64,
    /// Seconds from the start of the recording.
    pub times: Vec<f64>,
    /// Absolute time of the first master sample, from GPS where available.
    pub t0: Option<DateTime<Utc>>,
    /// True when `t0` came from the container clock rather than from GPS.
    pub t0_is_fallback: bool,
    pub duration_s: f64,

    pub lean_deg: Vec<f64>,
    pub pitch_deg: Vec<f64>,
    pub yaw_rate_dps: Vec<f64>,
    pub lean_rate_dps: Option<Vec<f64>>,
    pub pitch_rate_dps: Option<Vec<f64>>,
    pub accel_lon_g: Option<Vec<f64>>,
    pub accel_lat_g: Option<Vec<f64>>,
    pub accel_vert_g: Option<Vec<f64>>,
    pub accel_total_g: Option<Vec<f64>>,
    pub imu_temp_c: Option<Vec<f64>>,

    /// GPS fixes at the receiver's own rate.
    pub gps: Vec<GpsFix>,
    /// For each master sample, the index of the most recent GPS fix.
    gps_index: Vec<Option<usize>>,

    pub lean_sign: LeanSign,
    /// The roll zero tracked out of `lean_deg`, when there was enough
    /// straight-line riding to track it.
    pub lean_zero: Option<LeanZero>,
    /// The `GRAV`-to-accelerometer tilt, when there was enough coasting to
    /// measure it.
    pub grav_tilt: Option<GravTilt>,
    pub warnings: Vec<String>,
    pub device_name: Option<String>,
}

impl Telemetry {
    /// The GPS fix in effect at master sample `i`, valid or not.
    pub fn fix_at(&self, i: usize) -> Option<&GpsFix> {
        self.gps_index.get(i).copied().flatten().map(|j| &self.gps[j])
    }

    /// The GPS fix in effect at master sample `i`, only if it is usable.
    pub fn valid_fix_at(&self, i: usize) -> Option<&GpsFix> {
        self.fix_at(i).filter(|f| f.valid)
    }

    pub fn speed_at(&self, i: usize) -> Option<f64> {
        self.valid_fix_at(i).map(|f| f.speed_2d)
    }

    /// A sub-range of this telemetry, carrying its calibration.
    ///
    /// The point of it: a clip cut out of a ride cannot calibrate itself. The
    /// roll zero needs straight-line riding and the gravity reference needs
    /// coasting, and eleven seconds of one corner has neither — so telemetry
    /// derived from the clip alone falls back to a mount fit made from that one
    /// corner and comes out degrees wrong. See [`LeanZero`] and [`GravTilt`].
    /// Slicing the parent instead gives the clip the calibration of the whole
    /// ride, which is the only place it could have come from.
    ///
    /// Timestamps are re-based so the slice starts at zero, because that is
    /// what a standalone file's timeline means. `t0` moves with them, so
    /// absolute time is preserved.
    pub fn slice(&self, from: usize, to: usize) -> Telemetry {
        let (from, to) = (from.min(self.n), to.min(self.n));
        let n = to.saturating_sub(from);
        let origin = self.times.get(from).copied().unwrap_or(0.0);
        let t0 = self.utc_of(origin);
        let cut = |v: &[f64]| v[from..to].to_vec();
        let cut_opt = |v: &Option<Vec<f64>>| v.as_ref().map(|v| cut(v));

        // The fixes landing inside the slice, re-based onto its clock. The
        // bound is the end of the *last sample*, not the start, or a fix in the
        // final sample period would be dropped.
        let span_end = self.times.get(to.saturating_sub(1)).copied().unwrap_or(origin);
        let gps: Vec<GpsFix> = self
            .gps
            .iter()
            .filter(|f| f.time >= origin && f.time <= span_end)
            .map(|f| GpsFix { time: f.time - origin, ..f.clone() })
            .collect();
        let times: Vec<f64> = self.times[from..to].iter().map(|t| t - origin).collect();
        let gps_index = build_gps_index(&times, &gps);

        Telemetry {
            n,
            hz: self.hz,
            hz_nominal: self.hz_nominal,
            duration_s: times.last().copied().unwrap_or(0.0) - times.first().copied().unwrap_or(0.0),
            times,
            t0,
            t0_is_fallback: self.t0_is_fallback,
            lean_deg: cut(&self.lean_deg),
            pitch_deg: cut(&self.pitch_deg),
            yaw_rate_dps: cut(&self.yaw_rate_dps),
            lean_rate_dps: cut_opt(&self.lean_rate_dps),
            pitch_rate_dps: cut_opt(&self.pitch_rate_dps),
            accel_lon_g: cut_opt(&self.accel_lon_g),
            accel_lat_g: cut_opt(&self.accel_lat_g),
            accel_vert_g: cut_opt(&self.accel_vert_g),
            accel_total_g: cut_opt(&self.accel_total_g),
            imu_temp_c: cut_opt(&self.imu_temp_c),
            gps,
            gps_index,
            lean_sign: self.lean_sign.clone(),
            // `range_deg` and `samples` deliberately stay the parent's: they
            // describe the fit, and the fit was made over the whole ride. Only
            // the offsets actually subtracted here are sliced.
            lean_zero: self.lean_zero.as_ref().map(|z| LeanZero {
                offset_deg: cut(&z.offset_deg),
                ..z.clone()
            }),
            grav_tilt: self.grav_tilt.clone(),
            warnings: Vec::new(),
            device_name: self.device_name.clone(),
        }
    }

    /// The sample range covering `[start, end]` on this source's timeline.
    ///
    /// Half-open, in the form [`slice`](Self::slice) takes.
    pub fn range_covering(&self, start: f64, end: f64) -> (usize, usize) {
        let from = self.times.partition_point(|&t| t < start);
        let to = self.times.partition_point(|&t| t <= end);
        (from.min(self.n), to.min(self.n))
    }

    /// Absolute time at a point on this source's own timeline.
    ///
    /// Measured from `times[0]`, which is what `t0` is anchored to, and not
    /// from zero. The two are the same thing on a whole recording, where the
    /// first sample lands a frame or two in. They are emphatically not the same
    /// on a clip cut out of one: a stream copy keeps the GPMF's original
    /// timestamps, so an eighteen-minutes-in clip has a timeline starting at
    /// 1098 s, and treating that as an offset from `t0` put its UTC eighteen
    /// minutes into the future.
    pub fn utc_of(&self, time_s: f64) -> Option<DateTime<Utc>> {
        let t0 = self.t0?;
        let micros = ((time_s - self.times[0]) * 1e6).round() as i64;
        Some(t0 + chrono::Duration::microseconds(micros))
    }

    /// Absolute time of master sample `i`.
    pub fn utc_at(&self, i: usize) -> Option<DateTime<Utc>> {
        self.utc_of(*self.times.get(i)?)
    }

    pub fn valid_fix_count(&self) -> usize {
        self.gps.iter().filter(|f| f.valid).count()
    }

    pub fn fix_percentage(&self) -> f64 {
        if self.gps.is_empty() {
            return 0.0;
        }
        100.0 * self.valid_fix_count() as f64 / self.gps.len() as f64
    }

    /// Ground distance covered, summed over consecutive valid fixes.
    ///
    /// Runs of coverage are never bridged: joining the two ends of a tunnel
    /// with a straight line would invent distance that was not measured.
    pub fn distance_m(&self) -> f64 {
        self.coverage_runs()
            .iter()
            .flat_map(|run| run.windows(2))
            .map(|w| haversine(w[0].lat, w[0].lon, w[1].lat, w[1].lon))
            .sum()
    }

    /// Cumulative climb, ratcheted so that noise does not accumulate.
    ///
    /// Computed per run of continuous coverage and summed. Treating the whole
    /// ride as one series instead would count every altitude step across a
    /// coverage gap as climb: on the reference clip, where GPS is lost for
    /// eleven of twenty-five minutes, that inflates 15 km of riding to 2.9 km
    /// of ascent.
    pub fn elevation_gain_m(&self) -> f64 {
        let hz = self.gps_hz().unwrap_or(10.0);
        let window = ((hz * 5.0) as usize).max(3);
        let mut gain = 0.0;
        for run in self.coverage_runs() {
            if run.len() < 3 {
                continue;
            }
            let alts: Vec<f64> = run.iter().map(|f| f.alt_m).collect();
            // GPS altitude is far noisier than its position, so smooth over
            // five seconds before ratcheting.
            let smoothed = moving_average(&alts, window);
            let mut reference = smoothed[0];
            for &a in &smoothed[1..] {
                if a - reference > 1.0 {
                    gain += a - reference;
                    reference = a;
                } else if a < reference {
                    reference = a;
                }
            }
        }
        gain
    }

    /// Longest gap between fixes that still counts as continuous coverage.
    ///
    /// Five times the receiver's measured interval, which is the same rule the
    /// moments engine uses for breaking a derived channel. Expressing it once
    /// keeps the two from reporting different distances for the same ride.
    pub fn coverage_gap_threshold(&self) -> f64 {
        const GAP_FACTOR: f64 = 5.0;
        let mut deltas: Vec<f64> = self
            .gps
            .windows(2)
            .map(|w| w[1].time - w[0].time)
            .filter(|d| *d > 0.0)
            .collect();
        if deltas.is_empty() {
            return 1.0;
        }
        deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());
        quantile_sorted(&deltas, 0.5) * GAP_FACTOR
    }

    /// Runs of consecutive valid fixes with no gap longer than
    /// [`Telemetry::coverage_gap_threshold`].
    ///
    /// Anything derived by integrating along the track — distance, climb —
    /// has to respect these boundaries, because between them the bike went
    /// somewhere that was not recorded.
    pub fn coverage_runs(&self) -> Vec<Vec<&GpsFix>> {
        let threshold = self.coverage_gap_threshold();
        let mut runs: Vec<Vec<&GpsFix>> = Vec::new();
        let mut current: Vec<&GpsFix> = Vec::new();
        for f in self.gps.iter().filter(|f| f.valid) {
            match current.last() {
                Some(prev) if f.time - prev.time <= threshold => current.push(f),
                Some(_) => {
                    runs.push(std::mem::take(&mut current));
                    current.push(f);
                }
                None => current.push(f),
            }
        }
        if !current.is_empty() {
            runs.push(current);
        }
        runs
    }

    pub fn gps_hz(&self) -> Option<f64> {
        if self.gps.len() < 2 {
            return None;
        }
        let span = self.gps[self.gps.len() - 1].time - self.gps[0].time;
        if span <= 0.0 {
            return None;
        }
        Some((self.gps.len() - 1) as f64 / span)
    }
}

/// The master timeline a source carries, without deriving anything from it.
///
/// Cheap where [`compute`] is not: no calibration, no GPS gate, no warnings.
/// It exists so that a clip can be matched back to the recording it was cut
/// from. A stream copy keeps the GPMF's original timestamps, so a clip's
/// timeline is a verbatim sub-range of its parent's and the two line up sample
/// for sample — which is what makes the parent's calibration transferable
/// rather than merely approximate.
pub fn timeline(gpmf: &Gpmf) -> Option<Vec<f64>> {
    let series = gpmf.series();
    let device_id = pick_device(&series);
    series
        .get(&(device_id, FourCc::from_str_lossy("GRAV")))
        .map(|s| s.times.clone())
}

/// Compute rider channels from a parsed GPMF stream.
pub fn compute(gpmf: &Gpmf, opts: &Options) -> anyhow::Result<Telemetry> {
    let series = gpmf.series();
    let mut warnings = gpmf.warnings.clone();

    // Pick the device with the richest set of streams. Dual-lens cameras emit
    // one DEVC per lens and only one of them carries the full IMU set.
    let device_id = pick_device(&series);
    let get = |key: &str| -> Option<&Series> { series.get(&(device_id, FourCc::from_str_lossy(key))) };
    let device_name = series
        .iter()
        .find(|((d, _), _)| *d == device_id)
        .and_then(|(_, s)| s.device_name.clone());

    let grav_series = get("GRAV")
        .ok_or_else(|| anyhow::anyhow!("clip has no GRAV stream; cannot derive lean angle"))?;
    let n = grav_series.len();
    if n < 2 {
        anyhow::bail!("GRAV stream has only {n} samples");
    }

    let times: Vec<f64> = grav_series.times.clone();
    let hz = grav_series.hz().unwrap_or(0.0);
    let duration_s = times[n - 1] - times[0];

    // Gravity as a unit vector. GRAV points *up* in the camera's frame.
    let mut grav: Vec<Vec3> = (0..n)
        .map(|i| {
            let s = grav_series.sample(i);
            normalize(&[s[0], s[1], s[2]])
        })
        .collect();

    // Rotation rate and acceleration, box-filtered onto the master timeline.
    let gyro = get("GYRO").map(|s| {
        let src = grav_frame_vectors(s);
        box_resample(&s.times, &src, &times, opts.gyro_window_s)
    });
    let accel = get("ACCL").map(|s| {
        let src = grav_frame_vectors(s);
        box_resample(&s.times, &src, &times, opts.accel_window_s)
    });
    if gyro.is_none() {
        warnings.push("no GYRO stream: lean rate, pitch rate and yaw rate unavailable".into());
    }
    if accel.is_none() {
        warnings.push("no ACCL stream: accelerations unavailable".into());
    }

    // Position. Absent GPS is a warning, not a failure: lean and the IMU
    // channels are still perfectly good without it.
    let (mut gps, gps_key) = match (get("GPS9"), get("GPS5")) {
        (Some(s), _) => {
            if !gps9_reports_fix(s) {
                warnings.push(
                    "GPS9 reports fix type 0 for every sample: this camera does not populate the \
                     field, so fix quality is judged on dilution of precision alone"
                        .into(),
                );
            }
            (parse_gps9(s, opts), Some("GPS9"))
        }
        (None, Some(s)) => (parse_gps5(s, opts), Some("GPS5")),
        (None, None) => (Vec::new(), None),
    };
    if gps_key.is_none() {
        warnings.push(
            "clip has no GPS stream: speed, position and course are unavailable, and the lean \
             sign cannot be verified against an external reference"
                .into(),
        );
    }
    derive_course(&mut gps);

    let gps_index = build_gps_index(&times, &gps);
    let speed_at = |i: usize| -> Option<f64> {
        gps_index
            .get(i)
            .copied()
            .flatten()
            .map(|j| &gps[j])
            .filter(|f| f.valid)
            .map(|f| f.speed_2d)
    };

    // --- gravity reference -------------------------------------------------
    // Put `GRAV` back on the vertical the accelerometer measures, before
    // anything uses it. See [`GravTilt`]: this has to happen here, ahead of the
    // mount calibration, because the tilt is in the channel every later step
    // reads — and the acceleration channels, which resolve force against
    // `GRAV` with nothing to cancel against, inherit the whole of it.
    let grav_tilt = match accel.as_deref().filter(|_| opts.correct_grav) {
        None => None,
        Some(acc) => {
            let dv = speed_derivative(&times, &speed_at, n, hz);
            let provisional = turn_rate_series(gyro.as_deref(), &grav, n);
            let rotations: Vec<Vec3> = (0..n)
                .filter(|&i| {
                    speed_at(i).unwrap_or(0.0) > UPRIGHT_MIN_SPEED_MS
                        && provisional[i].to_degrees().abs() < UPRIGHT_MAX_TURN_DPS
                        && dv[i].abs() < COAST_MAX_ACCEL_MS2
                })
                // Rolling straight and neither braking nor driving: the only
                // force here is gravity, so the accelerometer points along the
                // true vertical and this cross product is the rotation that
                // takes `GRAV` onto it.
                .map(|i| cross(&grav[i], &normalize(&acc[i])))
                .collect();
            (rotations.len() >= GRAV_TILT_MIN_SAMPLES).then(|| {
                let r = median_vec(&rotations);
                let angle = magnitude(&r).clamp(-1.0, 1.0).asin();
                let applied = angle.to_degrees() <= GRAV_TILT_MAX_DEG;
                let axis = normalize(&r);
                if applied {
                    for g in grav.iter_mut() {
                        *g = normalize(&rotate_about(g, &axis, angle));
                    }
                }
                GravTilt {
                    angle_deg: angle.to_degrees(),
                    axis,
                    samples: rotations.len(),
                    applied,
                }
            })
        }
    };
    match &grav_tilt {
        None => warnings.push(
            "no coasting, straight-line riding in this source, so GRAV could not be checked \
             against the accelerometer. The road-frame acceleration channels rest on the \
             camera's own idea of vertical, which on this camera has been several degrees out."
                .into(),
        ),
        Some(t) if !t.applied => warnings.push(format!(
            "GRAV disagrees with the accelerometer by {:.1}°, which is too much to be a sensor \
             misalignment. It has been left alone: something about this recording's GRAV or ACCL \
             stream is not what it appears to be, and the acceleration channels are suspect.",
            t.angle_deg,
        )),
        Some(t) if t.angle_deg > GRAV_TILT_WARN_DEG => warnings.push(format!(
            "GRAV sat {:.1}° off the vertical the accelerometer measures, over {} coasting \
             samples. It has been corrected; without that, straight-line riding reports \
             cornering load it is not carrying.",
            t.angle_deg, t.samples,
        )),
        Some(_) => {}
    }

    // --- mount calibration -------------------------------------------------
    // Two passes: an initial guess over the whole clip, then a refit over the
    // straight-line samples only, where gravity really is the vehicle's "up".
    let (mut up_ref, mut lat_ax) = calibrate_mount(&grav, None);
    let mut yaw = turn_rate_series(gyro.as_deref(), &grav, n);
    let straight: Vec<usize> = (0..n)
        .filter(|&i| {
            speed_at(i).unwrap_or(0.0) > UPRIGHT_MIN_SPEED_MS
                && yaw[i].to_degrees().abs() < UPRIGHT_MAX_TURN_DPS
        })
        .collect();
    if straight.len() > 30.max((0.02 * n as f64) as usize) {
        let (u, l) = calibrate_mount(&grav, Some(&straight));
        up_ref = u;
        lat_ax = l;
        yaw = turn_rate_series(gyro.as_deref(), &grav, n);
    }
    let mut fwd = normalize(&cross(&lat_ax, &up_ref));

    // --- lean --------------------------------------------------------------
    let mut lean: Vec<f64> = grav
        .iter()
        .map(|g| dot(g, &lat_ax).atan2(dot(g, &up_ref)).to_degrees())
        .collect();
    lean = moving_average(&lean, 5);

    // `yaw` is in rad/s here and is the rightward turn-rate reference.
    let turn_rate_dps: Vec<f64> = yaw.iter().map(|v| v.to_degrees()).collect();
    let lean_sign = resolve_lean_sign(&lean, &turn_rate_dps, &gps, &gps_index, &speed_at);
    if lean_sign.flipped {
        for v in lean.iter_mut() {
            *v = -*v;
        }
        lat_ax = scale(&lat_ax, -1.0);
        fwd = normalize(&cross(&lat_ax, &up_ref));
    }
    if lean_sign.confidence != Confidence::High {
        warnings.push(format!(
            "lean sign confidence is {}: |r| = {:.3} against turn rate over {} samples, and the \
             turn-rate convention itself was confirmed against GPS at r = {}. Positive lean is \
             reported as a right-hand turn, but that could not be fully verified here.",
            lean_sign.confidence.as_str(),
            lean_sign.correlation,
            lean_sign.samples,
            lean_sign
                .gps_correlation
                .map(|r| format!("{r:+.3}"))
                .unwrap_or_else(|| "n/a (no usable GPS)".into()),
        ));
    }

    // --- roll zero ---------------------------------------------------------
    // Track the zero locally rather than trusting the whole-file fit. See
    // [`LeanZero`] for why a static zero misreports individual corners, and
    // note that this runs over `straight` — the very samples the static fit
    // was made from — so it is the same measurement, just not averaged flat.
    let lean_zero = match rolling_roll_zero(&lean, &times, &straight) {
        Some(offset) => {
            let mut lo = f64::INFINITY;
            let mut hi = f64::NEG_INFINITY;
            for (v, z) in lean.iter_mut().zip(&offset) {
                *v -= z;
                lo = lo.min(*z);
                hi = hi.max(*z);
            }
            let zero = LeanZero {
                samples: straight.len(),
                window_s: ROLL_ZERO_HALF_WINDOW_S,
                range_deg: (lo, hi),
                offset_deg: offset,
            };
            if zero.drift_deg() > ROLL_ZERO_DRIFT_WARN_DEG {
                warnings.push(format!(
                    "the upright reading drifted {:.1}° over this recording ({:+.1}° to {:+.1}°), \
                     which is the mount moving rather than the rider. It has been tracked out of \
                     the lean angle, but a mount that will not hold still limits how much any \
                     lean figure is worth.",
                    zero.drift_deg(),
                    zero.range_deg.0,
                    zero.range_deg.1,
                ));
            }
            Some(zero)
        }
        None => {
            warnings.push(format!(
                "only {} straight-line sample(s) here, too few to track the roll zero: lean is \
                 reported against a single whole-file mount calibration, which can be several \
                 degrees out at any given instant. A short clip cut out of a longer ride will \
                 disagree with the ride it came from.",
                straight.len(),
            ));
            None
        }
    };

    // --- forward polarity --------------------------------------------------
    // Which end of the lateral axis is "right" is settled; which end of the
    // forward axis is "ahead" is not. Resolve it against GPS speed change:
    // accelerating forwards must read as positive longitudinal acceleration.
    if let Some(acc) = accel.as_deref() {
        let dv = speed_derivative(&times, &speed_at, n, hz);
        let lon_for = |f: &Vec3| -> Vec<f64> {
            (0..n)
                .map(|i| {
                    let fh = normalize(&reject(f, &grav[i]));
                    let ah = reject(&acc[i], &grav[i]);
                    dot(&ah, &fh)
                })
                .collect()
        };
        if correlation_sign(&lon_for(&fwd), &dv) < 0.0 {
            fwd = scale(&fwd, -1.0);
        }
    }

    // --- rates -------------------------------------------------------------
    let (lean_rate, pitch_rate) = match gyro.as_deref() {
        None => (None, None),
        Some(gy) => {
            let mut roll_rate: Vec<f64> =
                (0..n).map(|i| dot(&gy[i], &fwd).to_degrees()).collect();
            // A roll rate must agree with the derivative of the roll angle it
            // is supposed to describe.
            let mut dlean = vec![0.0; n];
            for i in 1..n.saturating_sub(1) {
                let dt = times[i + 1] - times[i - 1];
                if dt > 0.0 {
                    dlean[i] = (lean[i + 1] - lean[i - 1]) / dt;
                }
            }
            if correlation_sign(&roll_rate, &dlean) < 0.0 {
                for v in roll_rate.iter_mut() {
                    *v = -*v;
                }
            }
            let pitch_rate: Vec<f64> =
                (0..n).map(|i| dot(&gy[i], &lat_ax).to_degrees()).collect();
            (Some(roll_rate), Some(pitch_rate))
        }
    };

    // --- accelerations in the road frame -----------------------------------
    let (a_lon, a_lat, a_vert, a_tot) = match accel.as_deref() {
        None => (None, None, None, None),
        Some(acc) => {
            let mut lon = Vec::with_capacity(n);
            let mut lat = Vec::with_capacity(n);
            let mut vert = Vec::with_capacity(n);
            let mut tot = Vec::with_capacity(n);
            for i in 0..n {
                let up = grav[i];
                let av = dot(&acc[i], &up);
                let ah = reject(&acc[i], &up);
                let fh = normalize(&reject(&fwd, &up));
                let left = cross(&up, &fh);
                lon.push(dot(&ah, &fh) / G);
                lat.push(-dot(&ah, &left) / G); // positive to the right
                vert.push((av - G) / G); // zero at rest
                tot.push(magnitude(&acc[i]) / G); // one at rest
            }
            // Lateral acceleration and lean are two views of the same corner.
            if correlation_sign(&lat, &lean) < 0.0 {
                for v in lat.iter_mut() {
                    *v = -*v;
                }
            }
            (Some(lon), Some(lat), Some(vert), Some(tot))
        }
    };

    // --- pitch -------------------------------------------------------------
    let pitch: Vec<f64> = grav
        .iter()
        .map(|g| dot(&fwd, g).clamp(-1.0, 1.0).asin().to_degrees())
        .collect();
    let pitch = moving_average(&pitch, 15);

    // --- sensor temperature ------------------------------------------------
    let imu_temp = get("GYRO")
        .or_else(|| get("ACCL"))
        .filter(|s| !s.tmpc.is_empty())
        .map(|s| {
            // One reading per payload; hold it across the payload's samples.
            let per = (n as f64 / s.tmpc.len() as f64).max(1.0);
            (0..n)
                .map(|i| {
                    let j = ((i as f64 / per) as usize).min(s.tmpc.len() - 1);
                    s.tmpc[j]
                })
                .collect()
        });

    // --- absolute clock ----------------------------------------------------
    let (t0, t0_is_fallback) = match gps.iter().find(|f| f.valid && f.utc.is_some()) {
        Some(f) => {
            let anchor = f.utc.unwrap();
            let offset = chrono::Duration::microseconds(((f.time - times[0]) * 1e6).round() as i64);
            (Some(anchor - offset), false)
        }
        None => (None, true),
    };

    let hz_nominal = nominal_rate(grav_series, gpmf);

    Ok(Telemetry {
        n,
        hz,
        hz_nominal,
        times,
        t0,
        t0_is_fallback,
        duration_s,
        lean_deg: lean,
        pitch_deg: pitch,
        yaw_rate_dps: yaw.iter().map(|v| v.to_degrees()).collect(),
        lean_rate_dps: lean_rate,
        pitch_rate_dps: pitch_rate,
        accel_lon_g: a_lon,
        accel_lat_g: a_lat,
        accel_vert_g: a_vert,
        accel_total_g: a_tot,
        imu_temp_c: imu_temp,
        gps,
        gps_index,
        lean_sign,
        lean_zero,
        grav_tilt,
        warnings,
        device_name,
    })
}

/// Decide which way round lean is signed, using GPS as the external reference.
///
/// This is the one place `gmax` deliberately departs from the prototype's
/// *result* rather than just its structure, so it is worth setting out why.
///
/// The prototype resolved the sign by correlating lean against gyro yaw rate,
/// `dot(GYRO, GRAV)`. That signal is strong — measured on the reference clip,
/// `|r(lean, yaw)| = 0.94` at every baseline — and it does not depend on the
/// calibrated lateral axis, so it is not circular in the way it first appears.
/// The flaw is subtler: its *sign convention* is opposite to the one the tool
/// reports. `GRAV` points up and the sensor frame is right-handed, so turning
/// right is a clockwise rotation seen from above, which is a **negative**
/// rotation about the up axis. Forcing lean to agree with it therefore makes
/// lean negative in a right-hand turn — confidently, consistently backwards.
/// The prototype's viewer then multiplies lean by −1 to undo it.
///
/// So the fix is two-stage, and uses each signal for what it is good at:
///
/// 1. Establish that `-dot(GYRO, GRAV)` is rightward turn rate, by correlating
///    it against GPS course rate — a reference whose sign depends on nothing we
///    calibrated, since turning right always increases bearing. This is weak
///    but absolute (`r ≈ 0.4`; GPS position noise caps it).
/// 2. Resolve lean against that turn-rate signal, which is strong (`|r| ≈ 0.94`)
///    and now known to be the right way round.
///
/// Without usable GPS, stage 1 cannot run and the convention is *assumed*
/// rather than confirmed; the confidence reported says so.
fn resolve_lean_sign(
    lean: &[f64],
    turn_rate_right: &[f64],
    gps: &[GpsFix],
    gps_index: &[Option<usize>],
    speed_at: &impl Fn(usize) -> Option<f64>,
) -> LeanSign {
    // --- stage 1: is our turn-rate signal really positive-to-the-right? -----
    let rate = course_rate(gps);
    let mut turn = Vec::new();
    let mut gps_turn = Vec::new();
    for i in 0..lean.len() {
        // Below walking pace a lean angle means nothing and a GPS bearing is
        // noise, so neither side of the comparison is informative.
        if speed_at(i).unwrap_or(0.0) <= 6.0 {
            continue;
        }
        let Some(j) = gps_index.get(i).copied().flatten() else {
            continue;
        };
        if !rate[j].is_finite() {
            continue;
        }
        turn.push(turn_rate_right[i]);
        gps_turn.push(rate[j]);
    }

    if turn.len() < 100 {
        // No external reference. Fall back to the documented convention.
        return LeanSign {
            correlation: pearson(lean, turn_rate_right).abs(),
            gps_correlation: None,
            samples: turn.len(),
            flipped: correlation_sign(lean, turn_rate_right) < 0.0,
            confidence: Confidence::Assumed,
        };
    }

    let r_convention = pearson(&turn, &gps_turn);
    // If the gyro-derived turn rate disagrees with GPS, our assumed handedness
    // is wrong for this camera; invert the reference before using it.
    let reference: Vec<f64> = if r_convention < 0.0 {
        turn_rate_right.iter().map(|v| -v).collect()
    } else {
        turn_rate_right.to_vec()
    };

    // --- stage 2: sign lean against the (now absolute) turn rate ------------
    let mut lean_v = Vec::new();
    let mut ref_v = Vec::new();
    for i in 0..lean.len() {
        if speed_at(i).unwrap_or(0.0) <= 6.0 {
            continue;
        }
        lean_v.push(lean[i]);
        ref_v.push(reference[i]);
    }
    let r_lean = pearson(&lean_v, &ref_v);
    let flipped = r_lean < 0.0;

    // Confidence is limited by the weaker of the two links in the chain.
    let confidence = if r_convention.abs() >= 0.25 && r_lean.abs() >= 0.5 {
        Confidence::High
    } else {
        Confidence::Low
    };

    LeanSign {
        correlation: r_lean.abs(),
        gps_correlation: Some(r_convention),
        samples: lean_v.len(),
        flipped,
        confidence,
    }
}

/// Mean gravity direction and the axis the bike rolls about.
///
/// `up_ref` is the mean gravity direction over the reference samples, i.e.
/// which way is up when the machine is upright. `lat_ax` is the direction
/// gravity *moves* as the machine leans, found as the dominant eigenvector of
/// the deviations from `up_ref` — the bike leans far more than it pitches, so
/// the principal component of gravity's motion is the lean axis.
fn calibrate_mount(grav: &[Vec3], reference: Option<&[usize]>) -> (Vec3, Vec3) {
    let mean = match reference {
        Some(idx) if !idx.is_empty() => {
            let mut acc = [0.0; 3];
            for &i in idx {
                for k in 0..3 {
                    acc[k] += grav[i][k];
                }
            }
            scale(&acc, 1.0 / idx.len() as f64)
        }
        _ => {
            let mut acc = [0.0; 3];
            for g in grav {
                for k in 0..3 {
                    acc[k] += g[k];
                }
            }
            scale(&acc, 1.0 / grav.len() as f64)
        }
    };
    let up_ref = normalize(&mean);
    let deviations: Vec<Vec3> = grav.iter().map(|g| sub(g, &up_ref)).collect();
    let axis = principal_axis(&covariance(&deviations));
    // Keep only the part perpendicular to up, so lean and pitch stay separable.
    let lat_ax = normalize(&reject(&axis, &up_ref));
    (up_ref, lat_ax)
}

/// The lean angle a genuinely upright machine reads, as it varies over the
/// ride.
///
/// `upright` is the ascending list of master samples that were rolling fast
/// enough and straight enough to be standing on their own wheels — the same
/// set [`calibrate_mount`] was fitted from. The median lean over the ones near
/// each instant is what upright reads there, and subtracting it leaves an
/// angle measured against the machine rather than against the mount.
///
/// A median, not a mean: a corner that creeps under
/// [`UPRIGHT_MAX_TURN_DPS`] — a long motorway sweeper, a roundabout taken
/// wide — is a real lean wrongly admitted to the reference set, and a mean
/// would let it drag the zero towards itself.
///
/// Returns `None` when there is too little straight-line riding to say
/// anything, which is the honest answer for a clip cut out of a single corner.
pub fn rolling_roll_zero(lean: &[f64], times: &[f64], upright: &[usize]) -> Option<Vec<f64>> {
    let n = lean.len();
    if upright.len() < ROLL_ZERO_MIN_TOTAL || n == 0 {
        return None;
    }
    // The reference samples, as parallel time and angle arrays. `upright` is
    // ascending, so `ref_t` is too and can be bisected.
    let ref_t: Vec<f64> = upright.iter().map(|&i| times[i]).collect();
    let ref_lean: Vec<f64> = upright.iter().map(|&i| lean[i]).collect();
    let overall = median(&ref_lean);

    // Everything between the first and last reference sample is interpolated;
    // outside that span the nearest grid point is held, so a ride that starts
    // or ends mid-corner keeps the zero of the riding either side of it rather
    // than inventing one.
    let span = times[n - 1] - times[0];
    let steps = (span / ROLL_ZERO_GRID_S).ceil().max(1.0) as usize;
    let mut grid = Vec::with_capacity(steps + 1);
    for k in 0..=steps {
        let t = times[0] + k as f64 * ROLL_ZERO_GRID_S;
        let window = |half: f64| -> (usize, usize) {
            (
                ref_t.partition_point(|&x| x < t - half),
                ref_t.partition_point(|&x| x <= t + half),
            )
        };
        let (a, b) = window(ROLL_ZERO_HALF_WINDOW_S);
        let (a, b) = if b - a >= ROLL_ZERO_MIN_LOCAL {
            (a, b)
        } else {
            window(ROLL_ZERO_WIDE_HALF_WINDOW_S)
        };
        grid.push(if b - a >= ROLL_ZERO_MIN_WIDE {
            median(&ref_lean[a..b])
        } else {
            overall
        });
    }
    // Adjacent windows overlap by all but a second, so the grid is already
    // smooth; this only takes the corner off the step where a dense block of
    // reference samples enters or leaves a window.
    let grid = moving_average(&grid, ROLL_ZERO_SMOOTH);

    Some(
        (0..n)
            .map(|i| {
                let pos = ((times[i] - times[0]) / ROLL_ZERO_GRID_S).clamp(0.0, steps as f64);
                let lo = pos.floor() as usize;
                let hi = (lo + 1).min(steps);
                grid[lo] + (grid[hi] - grid[lo]) * (pos - lo as f64)
            })
            .collect(),
    )
}

/// Rate of turn about the vertical, in radians per second, positive to the
/// right.
///
/// Note the negation. `dot(GYRO, GRAV)` is the rotation rate about the *up*
/// axis, and in a right-handed frame a right-hand turn is clockwise seen from
/// above, hence negative. Negating it here means every lateral channel `gmax`
/// reports — lean angle, lean rate, yaw rate, lateral acceleration — is
/// positive to the right, so they can be compared and plotted together without
/// a per-channel sign table. The prototype left this one inverted relative to
/// the others, which is why its viewer carries a hardcoded list of channels to
/// multiply by −1.
fn turn_rate_series(gyro: Option<&[Vec3]>, grav: &[Vec3], n: usize) -> Vec<f64> {
    match gyro {
        None => vec![0.0; n],
        Some(gy) => {
            let raw: Vec<f64> = (0..n).map(|i| -dot(&gy[i], &grav[i])).collect();
            moving_average(&raw, 15)
        }
    }
}

/// Central-difference derivative of GPS speed on the master timeline.
fn speed_derivative(
    times: &[f64],
    speed_at: &impl Fn(usize) -> Option<f64>,
    n: usize,
    hz: f64,
) -> Vec<f64> {
    // Half a second either side: shorter than that and GPS speed quantisation
    // dominates the derivative.
    let step = ((hz / 2.0) as usize).max(1);
    let mut dv = vec![0.0; n];
    for i in 0..n {
        let a = i.saturating_sub(step);
        let b = (i + step).min(n - 1);
        if let (Some(sa), Some(sb)) = (speed_at(a), speed_at(b)) {
            let dt = times[b] - times[a];
            if dt > 0.0 {
                dv[i] = (sb - sa) / dt;
            }
        }
    }
    dv
}

/// Reorient a triplet series into the gravity frame.
fn grav_frame_vectors(s: &Series) -> Vec<Vec3> {
    let orin = s.orin.as_deref();
    (0..s.len())
        .map(|i| gpmf::to_grav_frame(s.sample(i), orin))
        .collect()
}

/// Box-average a vector series onto a new timeline over a `±half_s` window.
///
/// The window is a real duration, not a sample count, so it means the same
/// thing whether the source runs at 200 Hz or 800 Hz. Both timelines are
/// sorted, so a two-pointer sweep does this in one pass.
fn box_resample(src_times: &[f64], src: &[Vec3], dst_times: &[f64], half_s: f64) -> Vec<Vec3> {
    let mut out = Vec::with_capacity(dst_times.len());
    if src.is_empty() {
        return vec![[0.0; 3]; dst_times.len()];
    }
    let mut lo = 0usize;
    let mut hi = 0usize;
    let mut sum = [0.0f64; 3];
    let mut count = 0usize;

    for &t in dst_times {
        let (from, to) = (t - half_s, t + half_s);
        while hi < src.len() && src_times[hi] <= to {
            for k in 0..3 {
                sum[k] += src[hi][k];
            }
            count += 1;
            hi += 1;
        }
        while lo < hi && src_times[lo] < from {
            for k in 0..3 {
                sum[k] -= src[lo][k];
            }
            count -= 1;
            lo += 1;
        }
        if count > 0 {
            out.push([
                sum[0] / count as f64,
                sum[1] / count as f64,
                sum[2] / count as f64,
            ]);
        } else {
            // No source sample inside the window: fall back to the nearest one
            // rather than emitting a zero, which would read as free-fall.
            let j = nearest_index(src_times, t).unwrap_or(0);
            out.push(src[j]);
        }
    }
    out
}

fn nearest_index(times: &[f64], t: f64) -> Option<usize> {
    if times.is_empty() {
        return None;
    }
    let i = times.partition_point(|&x| x < t);
    let cands = [i.checked_sub(1), Some(i.min(times.len() - 1))];
    cands
        .into_iter()
        .flatten()
        .min_by(|&a, &b| {
            (times[a] - t)
                .abs()
                .partial_cmp(&(times[b] - t).abs())
                .unwrap()
        })
}

/// For each master sample, the most recent GPS fix at or before it.
///
/// "Most recent" rather than "nearest" because a fix describes where the bike
/// was when it was taken; holding it forward until the next one is the honest
/// reading, and it matches the repeated GPS columns in the prototype's CSV.
fn build_gps_index(times: &[f64], gps: &[GpsFix]) -> Vec<Option<usize>> {
    let mut out = Vec::with_capacity(times.len());
    let mut j = 0usize;
    for &t in times {
        while j + 1 < gps.len() && gps[j + 1].time <= t {
            j += 1;
        }
        if gps.is_empty() {
            out.push(None);
        } else if gps[j].time <= t || j == 0 {
            out.push(Some(j));
        } else {
            out.push(None);
        }
    }
    out
}

/// Decode `GPS9`: lat, lon, alt, 2D speed, 3D speed, days, seconds, DOP, fix.
///
/// The `fix` field is treated as advisory rather than authoritative, because
/// not every camera fills it in. On the reference clip — a Max 2 on firmware
/// H24.02.01.30.00 — it reads 0 for all 15 375 samples, including the 88 % of
/// the ride with a good three-metre lock, so gating on `fix >= 2` would reject
/// the entire recording. DOP, by contrast, is exact: it sits at the 99.99
/// sentinel through acquisition and drops to 3.31 on the very sample where the
/// position stops being nonsense.
///
/// So: if the recording ever reports a non-zero `fix`, the camera populates the
/// field and it is used. If it is uniformly zero, it carries no information and
/// is ignored, with a note in the quality report.
fn parse_gps9(s: &Series, opts: &Options) -> Vec<GpsFix> {
    let fix_populated = (0..s.len()).any(|i| s.sample(i)[8] > 0.0);

    let mut out = Vec::with_capacity(s.len());
    for i in 0..s.len() {
        let r = s.sample(i);
        let dop = r[7];
        let fix = r[8] as u8;
        let (lat, lon) = (r[0], r[1]);
        let utc = gps9_time(r[5], r[6]);
        let reject = if fix_populated && fix < 2 {
            Some(Reject::NoFix)
        } else if lat == 0.0 && lon == 0.0 {
            Some(Reject::NullIsland)
        } else if dop >= DOP_SENTINEL || dop >= opts.max_dop {
            Some(Reject::HighDop)
        } else {
            None
        };
        out.push(GpsFix {
            time: s.times[i],
            lat,
            lon,
            alt_m: r[2],
            speed_2d: r[3],
            speed_3d: r[4],
            dop: Some(dop),
            fix: Some(fix),
            utc,
            course_deg: None,
            valid: reject.is_none(),
            reject,
            source: Source::Gopro,
        });
    }
    flag_frozen(&mut out, opts.frozen_run);
    flag_implausible(&mut out, opts.max_speed_ratio, opts.speed_floor_ms);
    out
}

/// True when a `GPS9` stream actually populates its fix-type field.
fn gps9_reports_fix(s: &Series) -> bool {
    (0..s.len()).any(|i| s.sample(i)[8] > 0.0)
}

/// Decode the older five-field `GPS5`: lat, lon, alt, 2D speed, 3D speed.
///
/// It reports neither DOP nor fix type, so only a null position can be
/// rejected outright, and there is no telemetry clock to anchor absolute time.
fn parse_gps5(s: &Series, opts: &Options) -> Vec<GpsFix> {
    let mut out = Vec::with_capacity(s.len());
    for i in 0..s.len() {
        let r = s.sample(i);
        let (lat, lon) = (r[0], r[1]);
        let reject = (lat == 0.0 && lon == 0.0).then_some(Reject::NullIsland);
        out.push(GpsFix {
            time: s.times[i],
            lat,
            lon,
            alt_m: r.get(2).copied().unwrap_or(0.0),
            speed_2d: r.get(3).copied().unwrap_or(0.0),
            speed_3d: r.get(4).copied().unwrap_or(0.0),
            dop: None,
            fix: None,
            utc: None,
            course_deg: None,
            valid: reject.is_none(),
            reject,
            source: Source::Gopro,
        });
    }
    flag_frozen(&mut out, opts.frozen_run);
    flag_implausible(&mut out, opts.max_speed_ratio, opts.speed_floor_ms);
    out
}

fn gps9_time(days: f64, secs: f64) -> Option<DateTime<Utc>> {
    if !days.is_finite() || !secs.is_finite() || days <= 0.0 {
        return None;
    }
    let whole = GPS_EPOCH_UNIX + (days as i64) * 86_400 + secs.trunc() as i64;
    let nanos = ((secs - secs.trunc()) * 1e9).round() as u32;
    Utc.timestamp_opt(whole, nanos.min(999_999_999)).single()
}

/// Reject fixes whose displacement disagrees with the reported speed.
///
/// Walks forward holding the last *accepted* fix as the anchor, so a single
/// spike is rejected without also condemning the good fix that follows it. The
/// anchor is reset at a coverage gap, since the bike really did travel during
/// one and the displacement across it is legitimate.
fn flag_implausible(gps: &mut [GpsFix], ratio: f64, floor_ms: f64) {
    if ratio <= 0.0 {
        return;
    }
    let mut anchor: Option<usize> = None;
    for i in 0..gps.len() {
        if !gps[i].valid {
            continue;
        }
        let Some(a) = anchor else {
            anchor = Some(i);
            continue;
        };
        let dt = gps[i].time - gps[a].time;
        // A gap means the bike went somewhere unobserved; start again.
        if dt <= 0.0 || dt > 1.0 {
            anchor = Some(i);
            continue;
        }
        let moved = haversine(gps[a].lat, gps[a].lon, gps[i].lat, gps[i].lon);
        // The receiver's own speed at either end bounds how far it can have got.
        let speed = gps[a].speed_2d.max(gps[i].speed_2d);
        let allowed = (speed * ratio + floor_ms) * dt;
        if moved > allowed {
            gps[i].valid = false;
            gps[i].reject = Some(Reject::Implausible);
        } else {
            anchor = Some(i);
        }
    }
}

/// Mark runs of bit-identical positions as stale.
fn flag_frozen(gps: &mut [GpsFix], run_len: usize) {
    if run_len == 0 || gps.is_empty() {
        return;
    }
    let mut start = 0usize;
    for i in 1..=gps.len() {
        let same = i < gps.len()
            && gps[i].lat.to_bits() == gps[start].lat.to_bits()
            && gps[i].lon.to_bits() == gps[start].lon.to_bits();
        if !same {
            if i - start > run_len {
                // Keep the first of the run: it was a real fix. Everything
                // after it is the camera repeating itself.
                for f in &mut gps[start + 1..i] {
                    if f.valid {
                        f.valid = false;
                        f.reject = Some(Reject::Frozen);
                    }
                }
            }
            start = i;
        }
    }
}

/// Baseline over which a heading is measured, in seconds.
///
/// This is not a free parameter. The camera reports no heading, so it has to
/// come from displacement between fixes — and over one 10 Hz interval a bike at
/// 65 km/h moves 1.8 m, which is well inside the receiver's own 3 m error, so
/// the bearing is mostly noise. Measured on the reference clip, the correlation
/// between lean angle and the resulting course rate is:
///
/// | baseline | 0.1 s | 0.5 s | 1.0 s | 2.0 s | 3.0 s |
/// |----------|-------|-------|-------|-------|-------|
/// | r        | 0.069 | 0.270 | 0.394 | 0.312 | 0.145 |
///
/// Below a second, position noise dominates; above it, the smoothing spreads a
/// corner across the straights either side of it.
const COURSE_BASELINE_S: f64 = 1.0;

/// Course over ground, measured over [`COURSE_BASELINE_S`] centred on each fix.
///
/// Centred rather than trailing so the heading is attributed to the moment the
/// bike was actually pointing that way; a trailing baseline lags by half its
/// width, which matters when this is about to be correlated against lean.
fn derive_course(gps: &mut [GpsFix]) {
    let half = COURSE_BASELINE_S / 2.0;
    let n = gps.len();
    let mut course = vec![None; n];

    for i in 0..n {
        if !gps[i].valid {
            continue;
        }
        let t = gps[i].time;
        // Nearest valid fix at least `half` seconds either side.
        let Some(a) = (0..=i).rev().find(|&j| gps[j].valid && t - gps[j].time >= half) else {
            continue;
        };
        let Some(b) = (i..n).find(|&j| gps[j].valid && gps[j].time - t >= half) else {
            continue;
        };
        let span = gps[b].time - gps[a].time;
        // Reject a pair straddling a coverage gap: the bike went somewhere we
        // did not see, so the straight line between the ends is not its heading.
        if span > COURSE_BASELINE_S * 2.0 {
            continue;
        }
        let d = haversine(gps[a].lat, gps[a].lon, gps[b].lat, gps[b].lon);
        // One metre of travel over a second means the bike is stopped, and a
        // bearing from that is pure receiver noise.
        if d < 1.0 {
            continue;
        }
        course[i] = Some(bearing(gps[a].lat, gps[a].lon, gps[b].lat, gps[b].lon));
    }

    for (i, c) in course.into_iter().enumerate() {
        gps[i].course_deg = c;
    }
}

/// Rate of change of heading, in degrees per second, positive turning right.
///
/// Measured over the same baseline as the heading itself, for the same reason.
fn course_rate(gps: &[GpsFix]) -> Vec<f64> {
    let half = COURSE_BASELINE_S / 2.0;
    let n = gps.len();
    let mut rate = vec![f64::NAN; n];
    for i in 0..n {
        if gps[i].course_deg.is_none() {
            continue;
        }
        let t = gps[i].time;
        let Some(a) = (0..=i)
            .rev()
            .find(|&j| gps[j].course_deg.is_some() && t - gps[j].time >= half)
        else {
            continue;
        };
        let Some(b) = (i..n)
            .find(|&j| gps[j].course_deg.is_some() && gps[j].time - t >= half)
        else {
            continue;
        };
        let dt = gps[b].time - gps[a].time;
        if dt <= 0.0 || dt > COURSE_BASELINE_S * 2.0 {
            continue;
        }
        rate[i] = angle_delta(gps[a].course_deg.unwrap(), gps[b].course_deg.unwrap()) / dt;
    }
    rate
}

/// Choose the device whose streams look most complete.
fn pick_device(series: &BTreeMap<(u32, FourCc), Series>) -> u32 {
    let mut score: BTreeMap<u32, usize> = BTreeMap::new();
    for ((dev, key), s) in series {
        // Weight the streams the computation actually needs.
        let w = match key.as_bytes() {
            b"GRAV" => 100,
            b"GPS9" | b"GPS5" => 50,
            b"ACCL" | b"GYRO" => 20,
            _ => 1,
        };
        if !s.is_empty() {
            *score.entry(*dev).or_default() += w;
        }
    }
    score
        .into_iter()
        .max_by_key(|&(dev, s)| (s, std::cmp::Reverse(dev)))
        .map(|(dev, _)| dev)
        .unwrap_or(1)
}

/// Samples per payload, rounded — the "30 Hz" a sample count alone suggests.
///
/// Reported alongside the measured rate so the 0.1 % NTSC difference is
/// visible rather than mysterious.
fn nominal_rate(series: &Series, gpmf: &Gpmf) -> f64 {
    let payloads = gpmf.payloads.len().max(1);
    (series.len() as f64 / payloads as f64).round()
}
