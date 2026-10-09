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

/// ...and carrying no more cornering load than a machine leaned this far, in
/// degrees.
///
/// The load a turn puts on the machine is speed times rate of turn, and it is
/// the load, not the rate, that sets how far a machine leans. A cap on the rate
/// alone admits a different lean at every speed: 2°/s is half a degree of lean
/// at walking pace but seven at 130 km/h, which is a motorway sweeper, not a
/// straight. Capping the load instead admits the same lean at every speed. See
/// [`upright_samples`].
///
/// Measured on the reference ride, where the old 2°/s cap let enough fast
/// sweepers into the set to widen the roll zero's apparent drift from 9.0° to
/// 10°. It is not tighter because the set also has to be dense enough to fit a
/// zero locally: at 1.5° a minute of hairpins on the same ride lost so much of
/// its reference that the fit fell back to a window three times as wide, and
/// upright riding there read 3.7° off zero.
const UPRIGHT_MAX_LEAN_DEG: f64 = 2.0;

/// Half-width of the window each roll-zero point is taken over, in seconds.
///
/// Long enough to span the corners either side of a straight, short enough to
/// follow a mount that creeps over a twenty-minute ride.
const ROLL_ZERO_HALF_WINDOW_S: f64 = 30.0;

/// Widened half-width, used where the narrow window is too sparse.
const ROLL_ZERO_WIDE_HALF_WINDOW_S: f64 = 90.0;

/// Upright samples one baseline point needs before the narrow window is used.
const ROLL_ZERO_MIN_LOCAL: usize = 40;

/// Upright samples a baseline point needs to be measured at all. One with
/// fewer is filled in from the measured points either side of it.
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

/// The cornering load the gravity reference admits, as a lean in degrees.
///
/// Looser than [`UPRIGHT_MAX_LEAN_DEG`] because the two fits fail differently.
/// The roll zero is local: one sweeper can fill a whole thirty-second window
/// and drag it. The gravity fit is one median over the entire ride, where
/// sweepers left and right cancel, and it is already cut down to the coasting
/// samples -- so what it most needs is more of them. On the reference ride a
/// 1.5° gate left it 559 samples and 0.011 g of lateral bias on straights
/// judged by GPS course alone; this one gives it 987 and 0.002 g.
const COAST_MAX_LEAN_DEG: f64 = 3.0;

/// A `GRAV`-to-accelerometer disagreement beyond this, in degrees, is not a
/// sensor misalignment but a sign that one of the two streams is not what this
/// code thinks it is. It is reported and not applied.
const GRAV_TILT_MAX_DEG: f64 = 30.0;

/// A tilt worth mentioning in the quality report, in degrees.
const GRAV_TILT_WARN_DEG: f64 = 2.0;

/// Rate of turn above which a corner is well enough defined to say how fast
/// the machine must be going, in degrees per second. See
/// [`flag_speed_against_imu`].
const IMU_SPEED_MIN_TURN_DPS: f64 = 15.0;

/// ...and lateral load above which it is, in g.
const IMU_SPEED_MIN_LATERAL_G: f64 = 0.1;

/// Half-width of the window the IMU's speed is taken over, in seconds.
const IMU_SPEED_HALF_WINDOW_S: f64 = 0.5;

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
    /// Reject a fix whose speed implies this much acceleration against the
    /// valid fixes either side of it, in m/s². Zero disables the check. See
    /// [`flag_speed_jumps`].
    ///
    /// About 1.5 g, the same ceiling the moments engine puts on acceleration:
    /// no road machine gets near it, so a speed step that implies it is the
    /// receiver's, not the bike's.
    pub max_speed_accel_ms2: f64,
    /// Reject a fix whose speed is off by more than this factor, either way,
    /// from the speed the IMU says a corner is being taken at. Zero disables
    /// the check.
    ///
    /// The position check above cannot see a receiver whose speed and position
    /// are wrong *together*, and nor can a jump check see one that is wrong
    /// smoothly. On the reference clip both happen as the receiver reacquires
    /// in a hairpin: it reports 8 km/h, sliding smoothly down from 15 and back
    /// up to 30, through a 41° corner taken at 45°/s -- which is only possible
    /// at around 30 km/h. That fix then headed the lean ranking at "11.3
    /// km/h". See [`flag_speed_against_imu`].
    pub max_imu_speed_ratio: f64,
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
            max_speed_accel_ms2: 15.0,
            max_imu_speed_ratio: 2.5,
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
    /// Ground speed in m/s. NaN when unknown: a merge that revives a rejected
    /// fix with a position from elsewhere may have no speed to give it, and
    /// the camera's own was as wrong as its position. Read it through
    /// [`Telemetry::speed_at`], or filter on `is_finite`.
    pub speed_2d: f64,
    /// As `speed_2d`, and NaN on any fix a merge revived.
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
    /// The speed itself is physically impossible: it jumps further than a
    /// machine can accelerate, or disagrees with the corner the IMU measured.
    ImplausibleSpeed,
}

impl Reject {
    pub fn as_str(self) -> &'static str {
        match self {
            Reject::NoFix => "no fix",
            Reject::HighDop => "dilution of precision too high",
            Reject::NullIsland => "position is exactly 0,0",
            Reject::Frozen => "position frozen after loss of lock",
            Reject::Implausible => "position inconsistent with the receiver's own speed",
            Reject::ImplausibleSpeed => "speed inconsistent with the machine's own motion",
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
    /// True when GPS showed the gyro's rate of turn to run the opposite way to
    /// the one assumed, so `yaw_rate_dps` has been negated to stay positive to
    /// the right.
    ///
    /// Lean is signed against the corrected turn rate and so is right either
    /// way; this records that the reported yaw rate was corrected too, rather
    /// than left reporting a right-hand turn as a left-hand one.
    pub turn_rate_inverted: bool,
    pub confidence: Confidence,
}

/// The fixed tilt between `GRAV` and the accelerometer's own vertical.
///
/// `GRAV` is the camera's *fused estimate* of which way is down, not a
/// measurement, and on this camera it is several degrees out. Where the
/// machine is rolling straight and neither braking nor driving, the only force
/// acting on it is gravity — so the accelerometer, which measures force, **is**
/// the vertical. On the reference ride the two disagree by 4.8°, steadily, from
/// one end of the recording to the other.
///
/// That figure is the rotation fitted over the ride's 987 coasting samples: the
/// component-wise median of the per-sample rotation vectors taking `GRAV` onto
/// the accelerometer, which is the correction actually applied. It is *not*
/// the median of the per-sample angles, which is 6.4° over the same samples
/// and is the wrong number to quote: an angle is a magnitude, so the vibration
/// a motorcycle puts through its accelerometer adds to every sample's angle
/// whichever way it points, where in the rotation vectors it points every way
/// and cancels. (An earlier version of this note quoted 8°, which was that
/// mistake.)
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
    /// True when `t0` came from some clock other than GPS, such as the
    /// container's.
    ///
    /// [`compute`] only ever anchors to GPS, so it leaves this false --
    /// including when there is no GPS clock and `t0` is `None`. An absent clock
    /// is not a fallback one, and saying otherwise told readers of the JSON
    /// that a start time they were not given was approximate.
    pub t0_is_fallback: bool,
    pub duration_s: f64,

    pub lean_deg: Vec<f64>,
    /// Degrees, positive nose-up.
    pub pitch_deg: Vec<f64>,
    /// Degrees per second, positive turning right. NaN throughout when the
    /// source has no `GYRO`, since a rate that was never measured is not zero.
    pub yaw_rate_dps: Vec<f64>,
    pub lean_rate_dps: Option<Vec<f64>>,
    /// Degrees per second, positive nose-up: the rate `pitch_deg` changes at,
    /// measured by the gyro rather than differenced from the angle.
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

    /// Ground speed at master sample `i`, when the fix in effect is usable
    /// and its speed known. A merge can revive a fix with a position and no
    /// speed to go with it, which it records as NaN.
    pub fn speed_at(&self, i: usize) -> Option<f64> {
        self.valid_fix_at(i).map(|f| f.speed_2d).filter(|v| v.is_finite())
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
        //
        // Plus the one fix taken before the slice began, if there is one: it
        // is the fix still in effect over the slice's first samples, so it is
        // what they must report. Without it they had no fix at all, and the
        // index then handed them the first fix *after* them instead -- a
        // position the bike had not reached yet. It keeps its true time, which
        // on the slice's clock is negative.
        let span_end = self.times.get(to.saturating_sub(1)).copied().unwrap_or(origin);
        let first_inside = self.gps.partition_point(|f| f.time < origin);
        let gps: Vec<GpsFix> = self.gps[first_inside.saturating_sub(1)..]
            .iter()
            .filter(|f| f.time <= span_end)
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

    /// The sample range covering `[start_s, end_s]` in seconds of video.
    ///
    /// [`range_covering`](Self::range_covering) for a window planned against
    /// the video -- a clip, a `--ss`/`--duration` -- which has to be moved onto
    /// the GPMF's own timeline first; see [`raw_time_of`](Self::raw_time_of).
    /// Empty on an empty source.
    pub fn video_range(&self, start_s: f64, end_s: f64) -> (usize, usize) {
        match (self.raw_time_of(start_s), self.raw_time_of(end_s)) {
            (Some(a), Some(b)) => self.range_covering(a, b),
            _ => (0, 0),
        }
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
    ///
    /// `None` without a clock, and on an empty slice, which has no first sample
    /// to measure from.
    pub fn utc_of(&self, time_s: f64) -> Option<DateTime<Utc>> {
        let t0 = self.t0?;
        let micros = (self.video_time_of(time_s)? * 1e6).round() as i64;
        Some(t0 + chrono::Duration::microseconds(micros))
    }

    /// Seconds into the video at a point on this source's own timeline.
    ///
    /// Measured from `times[0]` for the same reason [`utc_of`](Self::utc_of)
    /// is: a clip cut out of a longer recording keeps the GPMF's original
    /// timestamps, so its timeline starts wherever it was cut from while its
    /// video starts at zero. Reading the raw timestamp as a video position put
    /// every moment of an eighteen-minutes-in clip past the end of the file.
    ///
    /// On a whole recording this reads a frame or two early, since the first
    /// sample lands 60 ms in on the reference clip. That is well inside a
    /// clip's run-in, and the price of one rule that is right for both.
    pub fn video_time_of(&self, time_s: f64) -> Option<f64> {
        Some(time_s - self.times.first()?)
    }

    /// The point on this source's own timeline at `video_s` seconds into the
    /// video: the inverse of [`video_time_of`](Self::video_time_of).
    ///
    /// Everything planned against the video -- moments, clips, a hand-cut
    /// window -- is in video time, and everything read off the GPMF is not.
    /// The two differ by the whole cut-in point on a clip stream-copied out
    /// of a longer recording, so mixing them up is not a rounding error but
    /// eighteen minutes.
    pub fn raw_time_of(&self, video_s: f64) -> Option<f64> {
        Some(video_s + self.times.first()?)
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
    device_series(gpmf, "GRAV").map(|s| s.times)
}

/// The device [`compute`] reads its telemetry from.
///
/// A dual-lens camera emits one `DEVC` per lens and only one carries the full
/// IMU set, so anything reading a stream on its own must choose the same one
/// `compute` does, or it can read the other lens's copy of it. Decided from
/// sample counts, so no series has to be built to decide it.
pub fn main_device(gpmf: &Gpmf) -> u32 {
    pick_device(gpmf.sample_counts())
}

/// One stream of the main device (see [`main_device`]), built on its own.
pub fn device_series(gpmf: &Gpmf, key: &str) -> Option<Series> {
    gpmf.series_of(main_device(gpmf), FourCc::from_str_lossy(key))
}

/// Compute rider channels from a parsed GPMF stream.
pub fn compute(gpmf: &Gpmf, opts: &Options) -> anyhow::Result<Telemetry> {
    let series = gpmf.series();
    let mut warnings = gpmf.warnings.clone();

    // Pick the device with the richest set of streams. Dual-lens cameras emit
    // one DEVC per lens and only one of them carries the full IMU set.
    let device_id = pick_device(series.iter().map(|(k, s)| (*k, s.len())));
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
    // A stream too narrow for its own layout is malformed, and indexing its
    // fields would panic; it is skipped as if absent, which lets a broken GPS9
    // fall back to a sound GPS5.
    let mut wide_enough = |key: &str, need: usize| {
        get(key).filter(|s| {
            let ok = s.components >= need;
            if !ok {
                warnings.push(format!(
                    "{key} has {} values per sample where {need} are needed; it is ignored",
                    s.components
                ));
            }
            ok
        })
    };
    let (gps9, gps5) = (wide_enough("GPS9", GPS9_FIELDS), wide_enough("GPS5", GPS5_MIN_FIELDS));
    let (mut gps, gps_key) = match (gps9, gps5) {
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

    // Which samples are rolling upright, and what the speed is doing. Both are
    // gates for the calibration below, and both have to be able to say "not
    // known": a source with no gyro cannot tell a straight from a corner, and
    // a speed change across a dropout was not measured. Treating either as
    // zero let corners into the straight-line set and braking into the
    // coasting one.
    let provisional = turn_rate_series(gyro.as_deref(), &grav, n);
    let dv = speed_derivative(&times, &speed_at, n, hz);
    if gyro.is_none() {
        warnings.push(
            "no GYRO stream, so straight-line riding cannot be told from cornering: GRAV has not \
             been checked against the accelerometer, and the roll zero is not tracked. Lean \
             rests on a single whole-file mount calibration."
                .into(),
        );
    }

    // --- gravity reference -------------------------------------------------
    // Put `GRAV` back on the vertical the accelerometer measures, before
    // anything uses it. See [`GravTilt`]: this has to happen here, ahead of the
    // mount calibration, because the tilt is in the channel every later step
    // reads — and the acceleration channels, which resolve force against
    // `GRAV` with nothing to cancel against, inherit the whole of it.
    let grav_tilt = match (accel.as_deref().filter(|_| opts.correct_grav), &provisional) {
        (Some(acc), Some(turn)) => {
            // Rolling straight and neither braking nor driving: the only force
            // here is gravity, so the accelerometer points along the true
            // vertical.
            let coasting: Vec<usize> = upright_samples(&speed_at, turn, n, COAST_MAX_LEAN_DEG)
                .into_iter()
                .filter(|&i| dv[i].is_some_and(|a| a.abs() < COAST_MAX_ACCEL_MS2))
                .collect();
            let tilt = fit_grav_tilt(&grav, acc, &coasting);
            if let Some(t) = tilt.as_ref().filter(|t| t.applied) {
                for g in grav.iter_mut() {
                    *g = normalize(&rotate_about(g, &t.axis, t.angle_deg.to_radians()));
                }
            }
            tilt
        }
        _ => None,
    };
    match &grav_tilt {
        // Without a gyro the reason has already been given, and more fully.
        None if gyro.is_none() || !opts.correct_grav || accel.is_none() => {}
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
    let straight: Vec<usize> = match &yaw {
        Some(turn) => upright_samples(&speed_at, turn, n, UPRIGHT_MAX_LEAN_DEG),
        None => Vec::new(),
    };
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

    // `yaw` is in rad/s here and is the rightward turn-rate reference. With no
    // gyro there is no reference, and zeros stand in for it: they correlate
    // with nothing, so the sign is reported as unconfirmed rather than guessed.
    let turn_rate_dps: Vec<f64> = match &yaw {
        Some(y) => y.iter().map(|v| v.to_degrees()).collect(),
        None => vec![0.0; n],
    };
    let lean_sign = resolve_lean_sign(&lean, &turn_rate_dps, &gps, &gps_index, &speed_at);
    if lean_sign.flipped {
        for v in lean.iter_mut() {
            *v = -*v;
        }
        lat_ax = scale(&lat_ax, -1.0);
        fwd = normalize(&cross(&lat_ax, &up_ref));
    }
    // Yaw rate is the reference itself, so if GPS showed its sense to be
    // backwards the reported channel is backwards too, not just the copy lean
    // was signed against. (Pitch rate settles its own sign; see below.)
    let gyro_sign = if lean_sign.turn_rate_inverted { -1.0 } else { 1.0 };
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
        // Already explained, and more precisely, by the missing-gyro warning.
        None if gyro.is_none() => None,
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
    // A speed change that was not measured says nothing either way.
    if let Some(acc) = accel.as_deref() {
        let known: Vec<usize> = (0..n).filter(|&i| dv[i].is_some()).collect();
        let lon_for = |f: &Vec3| -> Vec<f64> {
            known
                .iter()
                .map(|&i| {
                    let fh = normalize(&reject(f, &grav[i]));
                    let ah = reject(&acc[i], &grav[i]);
                    dot(&ah, &fh)
                })
                .collect()
        };
        let dv_known: Vec<f64> = known.iter().filter_map(|&i| dv[i]).collect();
        if correlation_sign(&lon_for(&fwd), &dv_known) < 0.0 {
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
            let dlean = central_difference(&lean, &times);
            if correlation_sign(&roll_rate, &dlean) < 0.0 {
                for v in roll_rate.iter_mut() {
                    *v = -*v;
                }
            }
            // Pitch rate is read from the same axes as pitch itself. Pitch is
            // `asin(fwd · g)`, and a world-fixed vector seen from a frame
            // turning at ω moves as `dg/dt = -ω × g`, so
            //
            //     d(fwd · g)/dt = fwd · (-ω × g) = ω · (fwd × g).
            //
            // Rotation about `g` itself -- the rate of turn -- drops out, so
            // this is the rate the pitch angle changes at, not a body rate
            // that reads nose-up all through a banked corner. The one thing
            // it needs is the sense of that cross product, which depends on
            // the frame's handedness and the gyro's sign convention, neither
            // of which the camera states; see [`gyro_handedness`]. Reading
            // the rate about the lateral axis instead, as this once did,
            // came out positive nose-*down* on the reference clip, opposite
            // to the angle it sits beside.
            let h = gyro_handedness(gy, &grav, &times);
            let pitch_rate: Vec<f64> = (0..n)
                .map(|i| {
                    let cos_pitch = (1.0 - dot(&fwd, &grav[i]).powi(2)).sqrt().max(1e-6);
                    h * dot(&gy[i], &cross(&fwd, &grav[i])).to_degrees() / cos_pitch
                })
                .collect();
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
            for (a, up) in acc.iter().zip(&grav) {
                let av = dot(a, up);
                let ah = reject(a, up);
                let fh = normalize(&reject(&fwd, up));
                let left = cross(up, &fh);
                lon.push(dot(&ah, &fh) / G);
                lat.push(-dot(&ah, &left) / G); // positive to the right
                vert.push((av - G) / G); // zero at rest
                tot.push(magnitude(a) / G); // one at rest
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

    // `yaw` in degrees per second, positive right whatever the gyro's own
    // handedness; NaN, not zero, where there was no gyro to measure it.
    let yaw_rate_dps: Vec<f64> = match &yaw {
        Some(y) => y.iter().map(|v| gyro_sign * v.to_degrees()).collect(),
        None => vec![f64::NAN; n],
    };

    // --- speed against the IMU ---------------------------------------------
    // Last, because it needs the finished lateral channel. The calibration
    // above used GPS speed only as a gate, where a fix this check rejects does
    // little harm; the channels a reader sees are what it protects.
    if let (Some(alat), true) = (a_lat.as_deref(), gyro.is_some()) {
        let rejected = flag_speed_against_imu(
            &mut gps,
            &times,
            &yaw_rate_dps,
            alat,
            opts.max_imu_speed_ratio,
        );
        if rejected > 0 {
            // Course was derived from the positions of fixes now rejected.
            derive_course(&mut gps);
        }
    }

    // --- absolute clock ----------------------------------------------------
    let t0 = gps.iter().find(|f| f.valid && f.utc.is_some()).map(|f| {
        let anchor = f.utc.unwrap();
        anchor - chrono::Duration::microseconds(((f.time - times[0]) * 1e6).round() as i64)
    });

    let hz_nominal = nominal_rate(grav_series, gpmf);

    Ok(Telemetry {
        n,
        hz,
        hz_nominal,
        times,
        t0,
        t0_is_fallback: false,
        duration_s,
        lean_deg: lean,
        pitch_deg: pitch,
        yaw_rate_dps,
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
    for (i, &turn_rate) in turn_rate_right.iter().enumerate().take(lean.len()) {
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
        turn.push(turn_rate);
        gps_turn.push(rate[j]);
    }

    if turn.len() < 100 {
        // No external reference. Fall back to the documented convention.
        return LeanSign {
            correlation: pearson(lean, turn_rate_right).abs(),
            gps_correlation: None,
            samples: turn.len(),
            flipped: correlation_sign(lean, turn_rate_right) < 0.0,
            turn_rate_inverted: false,
            confidence: Confidence::Assumed,
        };
    }

    let r_convention = pearson(&turn, &gps_turn);
    // If the gyro-derived turn rate disagrees with GPS, our assumed handedness
    // is wrong for this camera; invert the reference before using it. The
    // caller is told, because the reference is not the only thing read from
    // the gyro: yaw rate and pitch rate are wrong in the same way.
    let turn_rate_inverted = r_convention < 0.0;
    let reference: Vec<f64> = if turn_rate_inverted {
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
        turn_rate_inverted,
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
/// set [`calibrate_mount`] was fitted from; see [`upright_samples`]. The median
/// lean over the ones near each instant is what upright reads there, and
/// subtracting it leaves an angle measured against the machine rather than
/// against the mount.
///
/// A median, not a mean: a corner gentle enough to pass as upright — a long
/// motorway sweeper, a roundabout taken wide — is a real lean wrongly admitted
/// to the reference set, and a mean would let it drag the zero towards itself.
///
/// Where there is too little upright riding nearby to measure the zero — a
/// run of corners longer than the widened window — it is interpolated between
/// the measured points either side, and held from the nearest one before the
/// first and after the last. It is *not* replaced by the whole-file median:
/// that is the static zero this exists to improve on, and dropping to it in
/// the middle of a drifting ride puts a step of however far the mount has
/// crept into the lean angle, at both ends of the gap.
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

    let span = times[n - 1] - times[0];
    let steps = (span / ROLL_ZERO_GRID_S).ceil().max(1.0) as usize;
    let measured: Vec<Option<f64>> = (0..=steps)
        .map(|k| {
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
            (b - a >= ROLL_ZERO_MIN_WIDE).then(|| median(&ref_lean[a..b]))
        })
        .collect();
    let grid = fill_gaps(&measured).unwrap_or_else(|| {
        // Enough upright riding in total but nowhere dense enough to measure a
        // local zero: spread thinly over a very long recording. The whole-file
        // median is then the only zero there is.
        vec![median(&ref_lean); steps + 1]
    });
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

/// Fill the unmeasured points of an evenly spaced series: linearly between
/// the measured points either side, and by holding the nearest one beyond the
/// first and last. `None` when nothing was measured.
fn fill_gaps(v: &[Option<f64>]) -> Option<Vec<f64>> {
    let known: Vec<usize> = (0..v.len()).filter(|&k| v[k].is_some()).collect();
    let (&first, &last) = (known.first()?, known.last()?);
    let mut out = vec![0.0; v.len()];
    for (k, slot) in out.iter_mut().enumerate() {
        *slot = if k <= first {
            v[first].unwrap()
        } else if k >= last {
            v[last].unwrap()
        } else {
            // The measured neighbours either side of `k`.
            let j = known.partition_point(|&x| x <= k);
            let (a, b) = (known[j - 1], known[j.min(known.len() - 1)]);
            let (va, vb) = (v[a].unwrap(), v[b].unwrap());
            if a == b {
                va
            } else {
                va + (vb - va) * (k - a) as f64 / (b - a) as f64
            }
        };
    }
    Some(out)
}

/// The master samples that count as upright: rolling on their own momentum,
/// and carrying no more cornering load than `max_lean_deg` of lean would.
///
/// The load is speed times rate of turn, from GPS and the gyro — two sources
/// that owe nothing to the mount calibration this set is used to fit, which is
/// what keeps the fit from choosing its own reference.
fn upright_samples(
    speed_at: &impl Fn(usize) -> Option<f64>,
    turn_rate: &[f64],
    n: usize,
    max_lean_deg: f64,
) -> Vec<usize> {
    let max_lateral = G * max_lean_deg.to_radians().tan();
    (0..n)
        .filter(|&i| {
            let Some(v) = speed_at(i).filter(|&v| v > UPRIGHT_MIN_SPEED_MS) else {
                return false;
            };
            (v * turn_rate[i]).abs() < max_lateral
        })
        .collect()
}

/// Measure the rotation that takes `GRAV` onto the accelerometer's vertical,
/// over master samples where the accelerometer *is* the vertical.
///
/// Each sample gives one rotation: the axis `GRAV × â` and an angle whose sine
/// is that cross product's length and whose cosine is `GRAV · â`. They are
/// combined component-wise by median, which a motorcycle's accelerometer
/// needs: it puts outliers through every reading. See [`GravTilt`] for why the
/// result is the combined rotation and not the median of the per-sample angles.
///
/// The angle comes from both parts, `atan2(|cross|, dot)`, and not from the
/// cross product alone. `asin(|cross|)` cannot tell a small tilt from one near
/// 180° — two vectors pointing almost opposite ways have a short cross product
/// too — so an accelerometer reading upside down relative to `GRAV` came out
/// as a small, plausible correction and was applied.
///
/// `applied` is false when the tilt is too large to be a misalignment; the
/// caller does the rotating. `None` below [`GRAV_TILT_MIN_SAMPLES`].
fn fit_grav_tilt(grav: &[Vec3], acc: &[Vec3], reference: &[usize]) -> Option<GravTilt> {
    if reference.len() < GRAV_TILT_MIN_SAMPLES {
        return None;
    }
    let mut crosses = Vec::with_capacity(reference.len());
    let mut dots = Vec::with_capacity(reference.len());
    for &i in reference {
        let a = normalize(&acc[i]);
        crosses.push(cross(&grav[i], &a));
        dots.push(dot(&grav[i], &a));
    }
    let r = median_vec(&crosses);
    let angle = magnitude(&r).atan2(median(&dots));
    Some(GravTilt {
        angle_deg: angle.to_degrees(),
        axis: normalize(&r),
        samples: reference.len(),
        applied: angle.to_degrees() <= GRAV_TILT_MAX_DEG,
    })
}

/// Rate of turn about the vertical, in radians per second, positive to the
/// right. `None` without a gyro: a rate that was not measured is unknown, not
/// zero, and zero reads as dead straight to everything that gates on it.
///
/// Note the negation. `dot(GYRO, GRAV)` is the rotation rate about the *up*
/// axis, and in a right-handed frame a right-hand turn is clockwise seen from
/// above, hence negative. Negating it here means every lateral channel `gmax`
/// reports — lean angle, lean rate, yaw rate, lateral acceleration — is
/// positive to the right, so they can be compared and plotted together without
/// a per-channel sign table. The prototype left this one inverted relative to
/// the others, which is why its viewer carries a hardcoded list of channels to
/// multiply by −1. Whether the camera's gyro really is right-handed is checked
/// against GPS in [`resolve_lean_sign`].
fn turn_rate_series(gyro: Option<&[Vec3]>, grav: &[Vec3], n: usize) -> Option<Vec<f64>> {
    let gy = gyro?;
    let raw: Vec<f64> = (0..n).map(|i| -dot(&gy[i], &grav[i])).collect();
    Some(moving_average(&raw, 15))
}

/// Central-difference derivative of GPS speed on the master timeline, in m/s².
///
/// `None` where either end of the difference has no valid fix. Such a sample
/// is not coasting: its speed change was simply not measured, and the typical
/// case -- braking into a tunnel, accelerating out of a dropout -- is exactly
/// the one that must not pass as steady riding.
fn speed_derivative(
    times: &[f64],
    speed_at: &impl Fn(usize) -> Option<f64>,
    n: usize,
    hz: f64,
) -> Vec<Option<f64>> {
    // Half a second either side: shorter than that and GPS speed quantisation
    // dominates the derivative.
    let step = ((hz / 2.0) as usize).max(1);
    (0..n)
        .map(|i| {
            let a = i.saturating_sub(step);
            let b = (i + step).min(n - 1);
            let dt = times[b] - times[a];
            let (sa, sb) = (speed_at(a)?, speed_at(b)?);
            (dt > 0.0).then(|| (sb - sa) / dt)
        })
        .collect()
}

/// The sense in which the gyro turns `GRAV`: `+1` when `dg/dt = -ω × g`, as it
/// does for a right-handed gyro in a right-handed frame, and `-1` when it runs
/// the other way.
///
/// It runs the other way on the reference clip, strongly: every axis of
/// `dg/dt` correlates with `-ω × g` at r ≈ −0.94. Nothing is wrong with the
/// gyro. `GRAV` has two axes swapped relative to the IMU streams, and a swap
/// is a reflection, so the frame everything is resolved in is left-handed and
/// every cross product computed in it comes out reversed. Dot products do not
/// notice, which is why the rate of turn, `-ω · g`, is unaffected and why this
/// went unseen until a channel built on a cross product was checked against
/// the angle it describes.
///
/// Measured rather than assumed because it is a property of the camera's axis
/// conventions, which differ between models, and because the measurement
/// needs nothing but the two streams: no mount calibration, no GPS.
fn gyro_handedness(gyro: &[Vec3], grav: &[Vec3], times: &[f64]) -> f64 {
    let mut s = 0.0;
    for i in 1..grav.len().saturating_sub(1) {
        let dt = times[i + 1] - times[i - 1];
        if dt <= 0.0 {
            continue;
        }
        let dg = scale(&sub(&grav[i + 1], &grav[i - 1]), 1.0 / dt);
        s += dot(&dg, &scale(&cross(&gyro[i], &grav[i]), -1.0));
    }
    if s < 0.0 {
        -1.0
    } else {
        1.0
    }
}

/// Central-difference derivative of a series against its timestamps, zero at
/// the two ends.
fn central_difference(v: &[f64], times: &[f64]) -> Vec<f64> {
    let n = v.len();
    let mut d = vec![0.0; n];
    for i in 1..n.saturating_sub(1) {
        let dt = times[i + 1] - times[i - 1];
        if dt > 0.0 {
            d[i] = (v[i + 1] - v[i - 1]) / dt;
        }
    }
    d
}

/// Reject fixes whose speed disagrees with the corner the IMU measured.
///
/// In a turn, speed is lateral acceleration over rate of turn — `v = a / ω` —
/// and both of those come from the IMU, which does not lose lock. So wherever
/// the machine is plainly cornering, the IMU says how fast it is going, and a
/// receiver claiming a very different speed is wrong. The estimate is a median
/// over [`IMU_SPEED_HALF_WINDOW_S`] either side, and is only made where most of
/// that window is a well-defined corner — turning at
/// [`IMU_SPEED_MIN_TURN_DPS`] or more, loaded at [`IMU_SPEED_MIN_LATERAL_G`] or
/// more, the two agreeing on direction — because at corner entry and exit the
/// ratio of two small, differently filtered numbers is noise.
///
/// The tolerance is wide. The IMU's estimate is itself rough — on the
/// reference clip its ratio to GPS speed has a central 90 % of 0.5–1.5 — so
/// this is for speeds that are wrong by multiples, like the reacquiring
/// receiver described on [`Options::max_imu_speed_ratio`], not for refining
/// good ones. Returns how many fixes it rejected.
fn flag_speed_against_imu(
    gps: &mut [GpsFix],
    times: &[f64],
    yaw_rate_dps: &[f64],
    accel_lat_g: &[f64],
    ratio: f64,
) -> usize {
    if ratio <= 1.0 || times.is_empty() {
        return 0;
    }
    // The IMU's speed at each master sample, where it is well defined.
    let imu_speed: Vec<Option<f64>> = yaw_rate_dps
        .iter()
        .zip(accel_lat_g)
        .map(|(&y, &a)| {
            let cornering = y.abs() >= IMU_SPEED_MIN_TURN_DPS
                && a.abs() >= IMU_SPEED_MIN_LATERAL_G
                && y.signum() == a.signum();
            cornering.then(|| (a * G / y.to_radians()).abs())
        })
        .collect();

    let mut rejected = 0;
    for fix in gps.iter_mut().filter(|f| f.valid) {
        let from = times.partition_point(|&t| t < fix.time - IMU_SPEED_HALF_WINDOW_S);
        let to = times.partition_point(|&t| t <= fix.time + IMU_SPEED_HALF_WINDOW_S);
        if to <= from {
            continue;
        }
        let window: Vec<f64> = imu_speed[from..to].iter().flatten().copied().collect();
        // Most of the window has to be corner, or the estimate is the entry
        // or exit transient.
        if window.len() * 5 < (to - from) * 4 {
            continue;
        }
        let v_imu = median(&window);
        if fix.speed_2d * ratio < v_imu || fix.speed_2d > v_imu * ratio {
            fix.valid = false;
            fix.reject = Some(Reject::ImplausibleSpeed);
            rejected += 1;
        }
    }
    rejected
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
///
/// A sample earlier than every fix has none. Handing it the first fix instead,
/// as this once did, reported a position the bike had not yet reached: the
/// first 100 ms of every clip cut from a ride showed where it would be next.
fn build_gps_index(times: &[f64], gps: &[GpsFix]) -> Vec<Option<usize>> {
    let mut out = Vec::with_capacity(times.len());
    let mut j = 0usize;
    for &t in times {
        while j + 1 < gps.len() && gps[j + 1].time <= t {
            j += 1;
        }
        out.push(gps.get(j).filter(|f| f.time <= t).map(|_| j));
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
///
/// The caller checks the stream has [`GPS9_FIELDS`] values per sample.
fn parse_gps9(s: &Series, opts: &Options) -> Vec<GpsFix> {
    let fix_populated = gps9_reports_fix(s);

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
    flag_speed_jumps(&mut out, opts.max_speed_accel_ms2);
    out
}

/// Values per `GPS9` sample.
const GPS9_FIELDS: usize = 9;

/// The fewest values a `GPS5` sample can have and still be a position. The
/// remaining fields are optional and read as zero when missing.
const GPS5_MIN_FIELDS: usize = 2;

/// True when a `GPS9` stream actually populates its fix-type field.
fn gps9_reports_fix(s: &Series) -> bool {
    (0..s.len()).any(|i| s.sample(i)[8] > 0.0)
}

/// Decode the older five-field `GPS5`: lat, lon, alt, 2D speed, 3D speed.
///
/// It reports neither DOP nor fix type, so only a null position can be
/// rejected outright, and there is no telemetry clock to anchor absolute time.
///
/// The caller checks the stream has at least [`GPS5_MIN_FIELDS`] values per
/// sample.
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
    flag_speed_jumps(&mut out, opts.max_speed_accel_ms2);
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

/// Reject fixes whose speed jumps further than any machine can accelerate.
///
/// The position check compares a fix's movement against its speed, so it
/// trusts the speed; this is the check on the speed itself. It looks for a
/// spike: a fix whose speed is out of reach of the valid fix either side of
/// it, when those two are within reach of each other. That catches the lone
/// bad sample without ever having to decide which side of a sustained step is
/// the wrong one -- which from speed alone cannot be done, and which a walk
/// anchored on the last accepted fix gets wrong whenever the step's first
/// fix is the bad one, rejecting the good fixes after it instead. A step
/// through a corner is left to [`flag_speed_against_imu`], which has an
/// independent speed to settle it with.
///
/// Neighbours more than a second away do not count: across a gap the machine
/// really could have changed speed. Half a metre per second is added to the
/// limit for receiver noise, which is what adjacent fixes can disagree by at
/// a standstill.
fn flag_speed_jumps(gps: &mut [GpsFix], max_accel_ms2: f64) {
    const NOISE_MS: f64 = 0.5;
    const MAX_NEIGHBOUR_S: f64 = 1.0;
    if max_accel_ms2 <= 0.0 {
        return;
    }
    let valid: Vec<usize> = (0..gps.len()).filter(|&i| gps[i].valid).collect();
    let reach = |a: &GpsFix, b: &GpsFix| -> Option<bool> {
        let dt = (b.time - a.time).abs();
        (dt > 0.0 && dt <= MAX_NEIGHBOUR_S)
            .then(|| (b.speed_2d - a.speed_2d).abs() <= max_accel_ms2 * dt + NOISE_MS)
    };
    let spikes: Vec<usize> = valid
        .windows(3)
        .filter(|w| {
            let (p, i, q) = (&gps[w[0]], &gps[w[1]], &gps[w[2]]);
            reach(p, i) == Some(false) && reach(i, q) == Some(false) && reach(p, q) == Some(true)
        })
        .map(|w| w[1])
        .collect();
    for i in spikes {
        gps[i].valid = false;
        gps[i].reject = Some(Reject::ImplausibleSpeed);
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
fn pick_device(counts: impl IntoIterator<Item = ((u32, FourCc), usize)>) -> u32 {
    let mut score: BTreeMap<u32, usize> = BTreeMap::new();
    for ((dev, key), len) in counts {
        // Weight the streams the computation actually needs.
        let w = match key.as_bytes() {
            b"GRAV" => 100,
            b"GPS9" | b"GPS5" => 50,
            b"ACCL" | b"GYRO" => 20,
            _ => 1,
        };
        if len > 0 {
            *score.entry(dev).or_default() += w;
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
