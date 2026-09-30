//! Telemetry assertions, including parity against the prototype's CSV.

mod common;
use common::fixture_or_skip;

use gmax::telemetry::{self, Confidence, Reject, Telemetry};

fn compute() -> Option<Telemetry> {
    let path = common::fixture(common::REFERENCE_360)?;
    let mut f = gmax::mp4::Mp4::open(&path).unwrap();
    let g = gmax::gpmf::from_mp4(&mut f).unwrap();
    Some(telemetry::compute(&g, &telemetry::Options::default()).unwrap())
}

macro_rules! telemetry_or_skip {
    () => {
        match compute() {
            Some(t) => t,
            None => {
                eprintln!("SKIP: fixture {} is not present", common::REFERENCE_360);
                return;
            }
        }
    };
}

#[test]
fn master_timeline_runs_at_the_ntsc_rate() {
    let t = telemetry_or_skip!();
    assert_eq!(t.n, 46080, "one sample per video frame");
    assert_eq!(t.hz_nominal, 30.0);
    assert!((t.hz - 29.97003).abs() < 1e-4, "measured rate was {}", t.hz);
    // The prototype's last CSV row was at 1535.967 s because it assumed 30 Hz;
    // the clip is really 1537.5 s long.
    let last = t.times[t.n - 1];
    assert!((last - 1537.5).abs() < 0.1, "last sample at {last}");
}

#[test]
fn gps_quality_gate_matches_the_prototype_when_reduced_to_its_checks() {
    let path = fixture_or_skip!(common::REFERENCE_360);
    let mut f = gmax::mp4::Mp4::open(&path).unwrap();
    let g = gmax::gpmf::from_mp4(&mut f).unwrap();
    // With the plausibility checks off, the gate is the prototype's: dilution of
    // precision and a null position. The prototype's viewer reported
    // gps_fix_pct 54.9 for this clip.
    let t = telemetry::compute(
        &g,
        &telemetry::Options {
            max_speed_ratio: 0.0,
            max_speed_accel_ms2: 0.0,
            max_imu_speed_ratio: 0.0,
            ..Default::default()
        },
    )
    .unwrap();
    let pct = t.fix_percentage();
    assert!((pct - 54.9).abs() < 0.1, "fix percentage was {pct}");
    assert!(t
        .gps
        .iter()
        .filter(|f| !f.valid)
        .all(|f| f.reject == Some(Reject::HighDop)));
}

#[test]
fn the_plausibility_check_rejects_what_dop_cannot_see() {
    let t = telemetry_or_skip!();

    // This camera reports a DOP between 3.04 and 3.31 for every fix it accepts
    // -- a near-constant value that is plainly not a live quality estimate --
    // while its position track jumps. So a second check compares each fix
    // against the receiver's own Doppler speed, and it rejects fixes DOP alone
    // would have passed.
    let implausible = t
        .gps
        .iter()
        .filter(|f| f.reject == Some(Reject::Implausible))
        .count();
    assert!(implausible > 0, "the plausibility check caught nothing");
    // A small share: this is a filter, not a rewrite.
    let share = implausible as f64 / t.gps.len() as f64;
    assert!(share < 0.1, "rejected {:.1}% as implausible", share * 100.0);
    let pct = t.fix_percentage();
    assert!((50.0..55.0).contains(&pct), "fix percentage was {pct}");
}

#[test]
fn gps_fix_field_is_unusable_on_this_camera() {
    let t = telemetry_or_skip!();
    assert_eq!(t.gps.len(), 15375);

    // Every sample reports fix type 0, including the 55% with a three-metre
    // lock, so gating on `fix >= 2` would reject the entire recording. The
    // field is therefore only honoured when a recording ever reports a
    // non-zero value.
    assert!(t.gps.iter().all(|f| f.fix == Some(0)));
    assert!(t.valid_fix_count() > 8000, "the whole recording was rejected");
    assert!(t
        .warnings
        .iter()
        .any(|w| w.contains("fix type 0")), "the quality report should say so");

    // Sample 0: the camera has no lock, DOP pinned at the 99.99 sentinel and a
    // position off the coast of Norway rather than in the Apennines.
    let first = &t.gps[0];
    assert_eq!(first.dop, Some(99.99));
    assert!(!first.valid);
    assert!(first.lat > 60.0, "no-fix latitude was {}", first.lat);

    // The first usable fix, matching row `13.000` of the prototype's CSV.
    let good = t.gps.iter().find(|f| f.valid).expect("some valid fix");
    assert!((good.lat - 44.1725166).abs() < 1e-7);
    assert!((good.lon - 11.3394181).abs() < 1e-7);
    assert!((good.alt_m - 511.55).abs() < 0.01);
    assert_eq!(good.dop, Some(3.31));
    let utc = good.utc.unwrap().to_rfc3339();
    assert!(utc.starts_with("2026-09-08T11:07:00"), "first fix at {utc}");
}

#[test]
fn lean_is_signed_against_an_absolute_reference() {
    let t = telemetry_or_skip!();
    let s = &t.lean_sign;

    // Resolved with confidence, not guessed.
    assert_eq!(s.confidence, Confidence::High, "lean sign: {s:?}");

    // Stage 2: lean tracks the rightward turn rate strongly.
    assert!(s.correlation > 0.9, "|r| vs turn rate was {}", s.correlation);

    // Stage 1: that turn-rate signal was itself confirmed against GPS course
    // rate, whose sign depends on nothing we calibrated. Positive means our
    // assumed handedness was right.
    let gps_r = s.gps_correlation.expect("GPS confirmation");
    assert!(gps_r > 0.25, "convention r vs GPS course rate was {gps_r}");

    // On this clip the raw gravity-derived lean is backwards, which is the bug
    // the prototype papered over in its viewer.
    assert!(s.flipped, "expected the reference clip's lean to need inverting");
}

#[test]
fn lateral_channels_all_agree_on_which_way_is_right() {
    let t = telemetry_or_skip!();
    // The point of the sign work: lean angle, yaw rate and lateral
    // acceleration are three views of the same corner, so they must correlate
    // positively with each other. The prototype left yaw inverted relative to
    // the others, which is why its viewer carries a per-channel sign table.
    let alat = t.accel_lat_g.as_ref().unwrap();
    let r_yaw = gmax::math::pearson(&t.lean_deg, &t.yaw_rate_dps);
    let r_alat = gmax::math::pearson(&t.lean_deg, alat);
    assert!(r_yaw > 0.8, "r(lean, yaw) = {r_yaw}");
    assert!(r_alat > 0.5, "r(lean, accel_lat) = {r_alat}");
}

#[test]
fn derived_channels_stay_within_physical_limits() {
    let t = telemetry_or_skip!();
    let max = |v: &[f64]| v.iter().cloned().fold(f64::MIN, f64::max);
    let min = |v: &[f64]| v.iter().cloned().fold(f64::MAX, f64::min);

    // A motorcycle that leaned 60 degrees would be on its side.
    assert!(max(&t.lean_deg) < 60.0 && min(&t.lean_deg) > -60.0);
    // Total acceleration is 1 g at rest, and a road bike does not exceed ~2 g.
    let atot = t.accel_total_g.as_ref().unwrap();
    assert!(min(atot) > 0.0 && max(atot) < 2.5, "accel_total range");
    let median = gmax::math::median(atot);
    assert!((median - 1.0).abs() < 0.1, "accel_total median was {median}");
    // Vertical acceleration is gravity-removed, so it sits near zero.
    let avert = t.accel_vert_g.as_ref().unwrap();
    assert!(gmax::math::median(avert).abs() < 0.1);
    // IMU die temperature, not ambient.
    let temp = t.imu_temp_c.as_ref().unwrap();
    assert!(min(temp) > 20.0 && max(temp) < 90.0);
}

#[test]
fn distance_and_climb_respect_coverage_gaps() {
    let t = telemetry_or_skip!();
    // GPS is lost for eleven of twenty-five minutes on this clip, so coverage
    // is fragmented.
    let runs = t.coverage_runs();
    assert!(runs.len() > 10, "expected many coverage runs, got {}", runs.len());

    // Bridging those gaps reported 2.9 km of ascent over 15 km of riding.
    // Summing per run gives a figure a mountain road can actually produce.
    let climb = t.elevation_gain_m();
    let dist = t.distance_m() / 1000.0;
    assert!(dist > 10.0 && dist < 30.0, "distance was {dist} km");
    assert!(climb > 100.0 && climb < 2000.0, "climb was {climb} m");
    // A gradient over 20% averaged across the ride would not be a road.
    assert!(climb / (dist * 1000.0) < 0.2, "mean gradient too steep");
}

#[test]
fn csv_matches_the_prototype_except_where_it_was_wrong() {
    let t = telemetry_or_skip!();
    let proto = fixture_or_skip!(common::REFERENCE_CSV);

    let mut out = Vec::new();
    telemetry::output::write_csv_to(&t, &mut out).unwrap();
    let ours = String::from_utf8(out).unwrap();
    let theirs = std::fs::read_to_string(&proto).unwrap();

    let our_lines: Vec<&str> = ours.trim_end().lines().collect();
    let their_lines: Vec<&str> = theirs.trim_end().lines().collect();
    assert_eq!(our_lines.len(), their_lines.len(), "row count");
    assert_eq!(our_lines.len(), 46081, "header plus 46080 rows");

    // The prototype's 20 columns, in order, with ours appending three.
    let their_header: Vec<&str> = their_lines[0].split(',').collect();
    let our_header: Vec<&str> = our_lines[0].split(',').collect();
    assert_eq!(&our_header[..their_header.len()], &their_header[..]);
    assert_eq!(
        &our_header[their_header.len()..],
        &["gps_fix", "gps_source", "lean_zero_deg"]
    );

    // Column-wise comparison of the channels that should be identical.
    let col = |lines: &[&str], header: &[&str], name: &str| -> Vec<Option<f64>> {
        let idx = header.iter().position(|h| *h == name).unwrap();
        lines[1..]
            .iter()
            .map(|l| l.split(',').nth(idx).unwrap().parse::<f64>().ok())
            .collect()
    };
    let paired = |a: &[Option<f64>], b: &[Option<f64>]| -> (Vec<f64>, Vec<f64>) {
        let mut x = Vec::new();
        let mut y = Vec::new();
        for (p, q) in a.iter().zip(b) {
            if let (Some(p), Some(q)) = (p, q) {
                x.push(*p);
                y.push(*q);
            }
        }
        (x, y)
    };

    // Lean is inverted, and the roll zero is tracked out of it. Put the zero
    // back and a correlation of -1 proves that everything else about the
    // channel is still a pure sign flip.
    let ours_raw: Vec<Option<f64>> = col(&our_lines, &our_header, "lean_deg")
        .into_iter()
        .zip(col(&our_lines, &our_header, "lean_zero_deg"))
        .map(|(l, z)| Some(l? + z?))
        .collect();
    let (a, b) = paired(&ours_raw, &col(&their_lines, &their_header, "lean_deg"));
    let r = gmax::math::pearson(&a, &b);
    assert!(r < -0.999, "r(our raw lean, their lean) = {r}, expected -1");

    // The correction itself is a slow, small thing: it must not be doing the
    // work the lean angle is supposed to be doing. A few degrees of mount
    // drift is a mount; twenty degrees would be this tracking the riding.
    let zero: Vec<f64> = col(&our_lines, &our_header, "lean_zero_deg")
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(zero.len(), our_lines.len() - 1, "every row carries a zero");
    let span = zero.iter().cloned().fold(f64::MIN, f64::max)
        - zero.iter().cloned().fold(f64::MAX, f64::min);
    assert!(span < 20.0, "roll zero spans {span}°, which is not mount drift");

    // Yaw rate is negated to match lean's convention.
    let (a, b) = paired(
        &col(&our_lines, &our_header, "yaw_rate_dps"),
        &col(&their_lines, &their_header, "yaw_rate_dps"),
    );
    assert!(gmax::math::pearson(&a, &b) < -0.98);

    // Pitch and longitudinal acceleration are unchanged.
    for name in ["pitch_deg", "accel_lon_g"] {
        let (a, b) = paired(
            &col(&our_lines, &our_header, name),
            &col(&their_lines, &their_header, name),
        );
        let r = gmax::math::pearson(&a, &b);
        assert!(r > 0.99, "r({name}) = {r}, expected agreement");
    }

    // --- GPS: same fixes, better alignment -------------------------------
    //
    // Both files decode the same 15 375 GPS samples, so the *set* of fixes must
    // match. Which row each fix lands on does not, and should not: our GPS
    // timestamps come from the camera's own STMP (the first fix is at 89.8 ms),
    // whereas the prototype assumes fix j sits at exactly j/10 s. That is a
    // constant ~90 ms correction, so a row near a fix boundary picks up the
    // neighbouring fix.
    //
    // Testing set membership rather than row equality is what separates "we
    // decode the same data" from "we happen to align it the same way".
    let fix_tuples = |lines: &[&str], header: &[&str]| -> Vec<String> {
        let idx: Vec<usize> = ["lat", "lon", "alt_m", "gps_dop"]
            .iter()
            .map(|n| header.iter().position(|h| h == n).unwrap())
            .collect();
        let mut out = Vec::new();
        let mut last = String::new();
        for l in &lines[1..] {
            let f: Vec<&str> = l.split(',').collect();
            if f[idx[0]].is_empty() {
                continue;
            }
            let key = idx.iter().map(|&i| f[i]).collect::<Vec<_>>().join("|");
            if key != last {
                out.push(key.clone());
                last = key;
            }
        }
        out
    };
    let ours_fixes = fix_tuples(&our_lines, &our_header);
    let theirs_fixes: std::collections::HashSet<String> =
        fix_tuples(&their_lines, &their_header).into_iter().collect();
    let novel: Vec<&String> = ours_fixes
        .iter()
        .filter(|f| !theirs_fixes.contains(*f))
        .collect();
    assert!(
        novel.is_empty(),
        "we emitted {} fixes the prototype never saw, e.g. {:?}",
        novel.len(),
        &novel[..novel.len().min(3)]
    );
    assert!(
        ours_fixes.len() > 8000,
        "only {} distinct fixes reached the CSV",
        ours_fixes.len()
    );

    // And the residual alignment difference must be small in metres, which is
    // the unit that actually matters. Measured: median 0 m, p99 3.7 m, with a
    // handful of larger gaps where one file holds a stale fix across a coverage
    // boundary and the other does not.
    let lat_a = col(&our_lines, &our_header, "lat");
    let lon_a = col(&our_lines, &our_header, "lon");
    let lat_b = col(&their_lines, &their_header, "lat");
    let lon_b = col(&their_lines, &their_header, "lon");
    let mut separations = Vec::new();
    for i in 0..lat_a.len() {
        if let (Some(la), Some(oa), Some(lb), Some(ob)) =
            (lat_a[i], lon_a[i], lat_b[i], lon_b[i])
        {
            separations.push(gmax::math::haversine(la, oa, lb, ob));
        }
    }
    separations.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert!(separations.len() > 20000, "too few paired positions");
    let p50 = gmax::math::quantile_sorted(&separations, 0.5);
    let p99 = gmax::math::quantile_sorted(&separations, 0.99);
    assert!(p50 < 0.01, "median separation was {p50} m");
    assert!(p99 < 5.0, "99th percentile separation was {p99} m");
}

/// Whether master sample `i` is straight-line riding by GPS alone: moving, and
/// holding its course over ground to within three degrees for a second either
/// side.
///
/// Deliberately owes nothing to the gyro or the accelerometer. The calibration
/// chooses its own straight-line samples from speed and gyro rate of turn, so a
/// test that picked them the same way would pass by construction; course over
/// ground comes from the receiver's positions, which the calibration never
/// reads.
fn straight_by_gps(t: &Telemetry, i: usize) -> bool {
    let Some(fix) = t.valid_fix_at(i).filter(|f| f.speed_2d > 5.0) else {
        return false;
    };
    let course_at = |dt: f64| -> Option<f64> {
        let want = fix.time + dt;
        let j = t.gps.partition_point(|f| f.time < want - 0.05);
        t.gps
            .get(j)
            .filter(|f| f.valid && (f.time - want).abs() <= 0.05)
            .and_then(|f| f.course_deg)
    };
    let courses: Option<Vec<f64>> = [-1.0, -0.5, 0.0, 0.5, 1.0]
        .iter()
        .map(|&dt| course_at(dt))
        .collect();
    let Some(courses) = courses else { return false };
    courses
        .iter()
        .all(|&c| gmax::math::angle_delta(courses[0], c).abs() <= 3.0)
}

#[test]
fn upright_riding_reads_zero_everywhere_not_merely_on_average() {
    let t = telemetry_or_skip!();
    let z = t
        .lean_zero
        .as_ref()
        .expect("a full-length ride has enough straight-line riding to fit a zero");
    assert_eq!(z.offset_deg.len(), t.n);

    // Straight-line riding is the definition of upright: the machine is held up
    // by its own momentum and is not turning. Collect it per minute, because
    // the fault this guards against is a zero that is right on average and
    // wrong in every individual minute -- which is what a single whole-file
    // mount calibration gives you, and what put a 43-degree corner into the
    // clip index as 49.
    //
    // "Straight" is judged by GPS course, not by the gyro-and-speed rule the
    // zero is fitted from, or this would only be checking that the fit agrees
    // with itself.
    let mut by_minute: std::collections::BTreeMap<usize, Vec<f64>> = Default::default();
    for i in (0..t.n).filter(|&i| straight_by_gps(&t, i)) {
        by_minute
            .entry((t.times[i] / 60.0) as usize)
            .or_default()
            .push(t.lean_deg[i]);
    }
    let minutes: Vec<(usize, f64)> = by_minute
        .iter()
        .filter(|(_, v)| v.len() >= 30)
        .map(|(m, v)| (*m, gmax::math::median(v)))
        .collect();
    assert!(minutes.len() >= 8, "only {} minutes of straight riding by GPS", minutes.len());
    let worst = minutes
        .iter()
        .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
        .unwrap();
    // Measured: 2.4 degrees, in minute 20.
    assert!(
        worst.1.abs() < 3.0,
        "minute {} of upright riding reads {:+.2}°, so the roll zero is not being tracked",
        worst.0,
        worst.1
    );
}

#[test]
fn the_roll_zero_follows_mount_drift_without_chasing_the_riding() {
    // A synthetic ride at 30 Hz: ten minutes of cornering at +/-40 degrees on a
    // 20 s period, on a mount whose zero creeps linearly from -6 to +6 degrees.
    // Every fourth corner period the machine runs upright and straight.
    use std::f64::consts::TAU;
    let hz = 30.0;
    let n = (600.0 * hz) as usize;
    let times: Vec<f64> = (0..n).map(|i| i as f64 / hz).collect();
    let drift = |t: f64| -6.0 + 12.0 * t / 600.0;
    let upright_here = |t: f64| (t / 20.0) as usize % 4 == 3;
    let lean: Vec<f64> = times
        .iter()
        .map(|&t| drift(t) + if upright_here(t) { 0.0 } else { 40.0 * (t * TAU / 20.0).sin() })
        .collect();
    let upright: Vec<usize> = (0..n).filter(|&i| upright_here(times[i])).collect();

    let zero = telemetry::rolling_roll_zero(&lean, &times, &upright).expect("enough reference");
    assert_eq!(zero.len(), n);

    // It recovers the drift it was meant to recover.
    let worst = times
        .iter()
        .zip(&zero)
        .map(|(&t, &z)| (z - drift(t)).abs())
        .fold(0.0f64, f64::max);
    assert!(worst < 2.0, "worst zero error was {worst:.2} deg");

    // And it does not eat the cornering: correcting must leave the 40-degree
    // amplitude intact. A baseline that tracked the lean itself would flatten
    // it, which is the failure mode that makes this a median over a long
    // window rather than a short mean.
    let corrected: Vec<f64> = lean.iter().zip(&zero).map(|(l, z)| l - z).collect();
    let amplitude = corrected.iter().cloned().fold(f64::MIN, f64::max);
    assert!(
        (amplitude - 40.0).abs() < 2.0,
        "corrected peak lean was {amplitude:.2} deg, expected 40"
    );
}

#[test]
fn a_source_too_short_to_fit_a_zero_declines_rather_than_guessing() {
    // One corner and no straight-line riding: the honest answer is None, so
    // that `compute` leaves lean on the whole-file calibration and says so.
    // Fitting a zero to a single corner is how a cut clip comes to disagree
    // with the ride it was cut from.
    use std::f64::consts::TAU;
    let n = 330;
    let times: Vec<f64> = (0..n).map(|i| i as f64 / 30.0).collect();
    let lean: Vec<f64> = times.iter().map(|&t| -40.0 * (t * TAU / 22.0).sin()).collect();
    assert!(telemetry::rolling_roll_zero(&lean, &times, &[]).is_none());
    // Even a handful of upright samples is not enough to fit against.
    let few: Vec<usize> = (0..40).collect();
    assert!(telemetry::rolling_roll_zero(&lean, &times, &few).is_none());
}

#[test]
fn straight_level_riding_carries_no_cornering_load() {
    let t = telemetry_or_skip!();
    let tilt = t
        .grav_tilt
        .as_ref()
        .expect("a full-length ride contains coasting, straight-line riding");
    assert!(tilt.applied, "tilt of {:.1}° was not applied", tilt.angle_deg);

    // The physical fact the correction rests on: a machine rolling straight
    // and upright has nothing pushing it sideways. Any lateral reading there
    // is the camera's idea of vertical being wrong, and it lands in this
    // channel whole because nothing else references it.
    //
    // Straight is judged by GPS course alone. The correction was fitted over
    // samples chosen by gyro rate of turn and GPS speed change, so checking it
    // over those same samples would pass whatever the correction did.
    let alat = t.accel_lat_g.as_ref().expect("the camera has an IMU");
    let level: Vec<f64> = (0..t.n)
        .filter(|&i| straight_by_gps(&t, i))
        .map(|i| alat[i])
        .collect();
    assert!(level.len() > 200, "only {} straight samples", level.len());
    let bias = gmax::math::median(&level);
    assert!(
        bias.abs() < 0.03,
        "straight riding reports {bias:+.4} g across the machine ({:+.1}° of cornering load \
         that is not there)",
        bias.atan().to_degrees()
    );
}

#[test]
fn correcting_the_gravity_reference_moves_only_what_trusts_it() {
    let corrected = telemetry_or_skip!();
    let path = common::fixture(common::REFERENCE_360).unwrap();
    let mut f = gmax::mp4::Mp4::open(&path).unwrap();
    let g = gmax::gpmf::from_mp4(&mut f).unwrap();
    let raw = telemetry::compute(
        &g,
        &telemetry::Options {
            correct_grav: false,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(raw.grav_tilt.is_none() && corrected.grav_tilt.is_some());

    // Lean and pitch are angles between GRAV and a reference fitted from GRAV
    // itself, so rotating the whole stream barely moves them -- both sides
    // turn together. That near-invariance is what makes this safe to apply to
    // a channel as load-bearing as GRAV.
    //
    // Near, not exact: the correction also moves the turn rate, which decides
    // which samples count as straight, which shifts the reference the angles
    // are measured against. The body of the data is untouched and a thin tail
    // moves by a degree or two, so the assertion is on the median.
    for (name, a, b) in [
        ("lean", &corrected.lean_deg, &raw.lean_deg),
        ("pitch", &corrected.pitch_deg, &raw.pitch_deg),
    ] {
        let mut d: Vec<f64> = a.iter().zip(b).map(|(x, y)| (x - y).abs()).collect();
        d.sort_by(|p, q| p.partial_cmp(q).unwrap());
        let median = gmax::math::quantile_sorted(&d, 0.5);
        assert!(median < 0.2, "{name} moved by {median:.3}° under the correction");
    }

    // The acceleration channels have no such reference: they resolve force
    // against GRAV directly, so the tilt lands in them whole and taking it out
    // is the whole point. Straight-line riding -- by GPS course, which the
    // correction never reads -- must stop reporting cornering load it is not
    // carrying.
    let bias = |t: &Telemetry| {
        let alat = t.accel_lat_g.as_ref().unwrap();
        let v: Vec<f64> = (0..t.n)
            .filter(|&i| straight_by_gps(t, i))
            .map(|i| alat[i])
            .collect();
        gmax::math::median(&v)
    };
    let (before, after) = (bias(&raw), bias(&corrected));
    assert!(
        after.abs() < before.abs() / 2.0,
        "straight-line lateral load went from {before:+.4} g to {after:+.4} g"
    );

    // The correction must be a rotation and nothing else. Total acceleration
    // cannot show that -- it is the accelerometer's own magnitude and never
    // reads GRAV at all -- but the road-frame split can: longitudinal, lateral
    // and vertical are the accelerometer resolved onto three axes built from
    // GRAV, so they add back up to the total exactly when GRAV is still a unit
    // vector and the axes still square. A correction that scaled or skewed
    // GRAV would break this on every sample.
    assert_resolves_to_total(&corrected);
    assert_resolves_to_total(&raw);
}

/// Longitudinal, lateral and vertical acceleration must be an orthogonal split
/// of the total.
fn assert_resolves_to_total(t: &Telemetry) {
    let (lon, lat, vert, tot) = (
        t.accel_lon_g.as_ref().unwrap(),
        t.accel_lat_g.as_ref().unwrap(),
        t.accel_vert_g.as_ref().unwrap(),
        t.accel_total_g.as_ref().unwrap(),
    );
    let worst = (0..t.n)
        .map(|i| {
            let parts = lon[i].powi(2) + lat[i].powi(2) + (vert[i] + 1.0).powi(2);
            (parts.sqrt() - tot[i]).abs()
        })
        .fold(0.0f64, f64::max);
    assert!(worst < 1e-9, "the road-frame split misses the total by {worst} g");
}

#[test]
fn a_slice_carries_the_rides_calibration_onto_its_own_clock() {
    let t = telemetry_or_skip!();
    // Ten seconds from the middle of the ride, the way a clip is cut.
    let (from, to) = t.range_covering(600.0, 610.0);
    assert!(to - from > 250, "expected ~300 samples, got {}", to - from);
    let s = t.slice(from, to);

    assert_eq!(s.n, to - from);
    // Re-based: a standalone file's timeline starts at zero.
    assert!(s.times[0].abs() < 1e-9, "slice starts at {}", s.times[0]);
    assert!((s.duration_s - 10.0).abs() < 0.5, "span {}", s.duration_s);
    // ...but absolute time is preserved, because t0 moved with it.
    let (a, b) = (t.utc_of(t.times[from]).unwrap(), s.utc_of(0.0).unwrap());
    assert!(
        (a - b).num_milliseconds().abs() < 2,
        "slice start {b} against parent {a}"
    );

    // The point of the exercise: the numbers are the ride's, unchanged.
    assert_eq!(&t.lean_deg[from..to], &s.lean_deg[..]);
    assert_eq!(t.lean_sign.flipped, s.lean_sign.flipped);
    assert_eq!(
        t.grav_tilt.as_ref().map(|g| g.angle_deg),
        s.grav_tilt.as_ref().map(|g| g.angle_deg),
        "the gravity reference must come with it"
    );
    assert_eq!(
        t.lean_zero.as_ref().map(|z| z.offset_deg[from]),
        s.lean_zero.as_ref().map(|z| z.offset_deg[0]),
        "the roll zero must come with it"
    );

    // Fixes land inside the window and are re-based with the timeline, so the
    // viewer's index-to-position mapping still lines up. The one exception is
    // the fix already in effect when the slice begins, which comes with it at
    // its true, negative, time: without it the slice's first samples had no
    // fix, or worse, one from their future.
    assert!(!s.gps.is_empty(), "ten seconds of ride has GPS in it");
    let lead = s.gps.iter().filter(|f| f.time < -1e-9).count();
    assert!(lead <= 1, "{lead} fixes from before the slice");
    for f in &s.gps {
        assert!(
            f.time >= -0.2 && f.time <= s.duration_s + 1e-9,
            "fix at {} outside 0..{}",
            f.time,
            s.duration_s
        );
    }
    assert!(s.valid_fix_at(s.n / 2).is_some() || s.fix_at(s.n / 2).is_some());
}

#[test]
fn a_clip_calibrated_by_its_parent_disagrees_with_one_calibrating_itself() {
    let t = telemetry_or_skip!();
    // The fault this whole mechanism exists for, reproduced in miniature: take
    // the calibration a ten-second window would fit for itself and compare it
    // against the ride's. They differ by degrees, and the ride's is the one
    // that makes upright riding read zero.
    let (from, to) = t.range_covering(600.0, 610.0);
    let s = t.slice(from, to);
    let upright: Vec<usize> = (0..s.n)
        .filter(|&i| s.speed_at(i).unwrap_or(0.0) > 5.0 && s.yaw_rate_dps[i].abs() < 2.0)
        .collect();
    // Ten seconds cannot fit a roll zero, which is precisely why a clip must be
    // handed one rather than left to find its own.
    assert!(
        telemetry::rolling_roll_zero(&s.lean_deg, &s.times, &upright).is_none(),
        "ten seconds should be too little to fit a zero against"
    );
    // And it arrives carrying the parent's.
    assert!(s.lean_zero.is_some(), "the slice must bring one with it");
}

#[test]
fn absolute_time_is_anchored_to_the_first_sample_not_to_zero() {
    let t = telemetry_or_skip!();
    // `t0` is the clock at `times[0]`, so reading a time off the timeline must
    // measure from there. On a whole recording the first sample lands a frame
    // or two in and the distinction is invisible; on a clip, whose GPMF keeps
    // the parent's timestamps, times[0] is however far in it was cut from --
    // and adding that to t0 put the clip eighteen minutes into the future.
    let first = t.utc_at(0).expect("the reference clip has a clock");
    assert_eq!(first, t.t0.unwrap(), "utc_at(0) must be t0 itself");

    // A slice starting well into the ride is the case that used to break.
    let (from, to) = t.range_covering(900.0, 910.0);
    let s = t.slice(from, to);
    let want = t.utc_at(from).unwrap();
    let got = s.utc_at(0).unwrap();
    assert!(
        (want - got).num_milliseconds().abs() < 2,
        "sliced clip starts at {got}, parent says {want}"
    );

    // And the elapsed time between two samples is the same either way round.
    let mid = s.n / 2;
    let parent_gap = t.utc_at(from + mid).unwrap() - t.utc_at(from).unwrap();
    let slice_gap = s.utc_at(mid).unwrap() - s.utc_at(0).unwrap();
    assert_eq!(parent_gap.num_milliseconds(), slice_gap.num_milliseconds());
}

// ------------------------------------------------------------ synthetic rides
//
// Everything above needs the 8.8 GB reference clip. The rides below are built
// in memory from known kinematics, so each fault can be reproduced exactly --
// a gyro wired backwards, a frame of the wrong handedness, a receiver lying
// about its speed -- and the tests run on a fresh clone.

mod synthetic {
    use gmax::fourcc::FourCc;
    use gmax::gpmf::{Device, Gpmf, Payload, StreamChunk};
    use gmax::math::*;
    use std::f64::consts::{PI, TAU};

    pub enum Gps {
        Gps9,
        /// The older stream: no DOP, no fix type, and no clock.
        Gps5,
        None,
    }

    pub struct Ride {
        pub duration_s: f64,
        /// Where the GPMF clock starts. Non-zero emulates a clip stream-copied
        /// out of a longer recording, which keeps its parent's timestamps.
        pub clock_offset_s: f64,
        /// How far ahead of the IMU streams the receiver's clock starts. A cut
        /// clip's first fix usually lands a little before its first `GRAV`
        /// sample.
        pub gps_lead_s: f64,
        pub gyro: bool,
        /// Report the gyro with its sign reversed.
        pub gyro_backwards: bool,
        /// Swap two axes of every sensor, as the real camera's `GRAV` frame
        /// does relative to its IMU, making the frame left-handed.
        pub left_handed: bool,
        /// Report the accelerometer upside down.
        pub accel_backwards: bool,
        pub gps: Gps,
        /// What the receiver reports for a true speed at a time.
        pub speed_report: fn(f64, f64) -> f64,
    }

    impl Default for Ride {
        fn default() -> Self {
            Ride {
                duration_s: 300.0,
                clock_offset_s: 0.0,
                gps_lead_s: 0.0,
                gyro: true,
                gyro_backwards: false,
                left_handed: false,
                accel_backwards: false,
                gps: Gps::Gps9,
                speed_report: |_, v| v,
            }
        }
    }

    /// Speed, in m/s: a slow swell either side of 72 km/h, so that there is
    /// both coasting and acceleration for the calibration to find.
    pub fn speed(t: f64) -> f64 {
        20.0 + 3.0 * (t * TAU / 37.0).sin()
    }

    fn accel(t: f64) -> f64 {
        3.0 * TAU / 37.0 * (t * TAU / 37.0).cos()
    }

    /// Rate of turn, rad/s, positive right. A 20 s cycle: six seconds of
    /// right-hander, four of straight, six of left-hander, four of straight.
    pub fn turn_rate(t: f64) -> f64 {
        let phase = t.rem_euclid(20.0);
        let corner = |p: f64| 0.35 * (PI * p / 6.0).sin().powi(2);
        match phase {
            p if p < 6.0 => corner(p),
            p if (10.0..16.0).contains(&p) => -corner(p - 10.0),
            _ => 0.0,
        }
    }

    /// Lean, radians, positive right: whatever balances the turn.
    pub fn lean(t: f64) -> f64 {
        (speed(t) * turn_rate(t) / G).atan()
    }

    /// Pitch, radians, positive nose-up: a slow two-degree pitching.
    pub fn pitch(t: f64) -> f64 {
        2f64.to_radians() * (t * TAU / 7.0).sin()
    }

    fn deriv(f: fn(f64) -> f64, t: f64) -> f64 {
        (f(t + 1e-4) - f(t - 1e-4)) / 2e-4
    }

    type M3 = [[f64; 3]; 3];

    fn mul(m: &M3, v: &Vec3) -> Vec3 {
        [dot(&m[0], v), dot(&m[1], v), dot(&m[2], v)]
    }

    fn mat_mul(a: &M3, b: &M3) -> M3 {
        let mut out = [[0.0; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                out[i][j] = (0..3).map(|k| a[i][k] * b[k][j]).sum();
            }
        }
        out
    }

    fn transpose(m: &M3) -> M3 {
        let mut out = [[0.0; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                out[i][j] = m[j][i];
            }
        }
        out
    }

    /// Body-to-heading rotation. The heading frame is x ahead, y left, z up:
    /// a right-hand roll takes the top of the machine towards -y, and nose-up
    /// pitch takes its nose towards +z.
    fn attitude(t: f64) -> M3 {
        let (sr, cr) = lean(t).sin_cos();
        let (sp, cp) = (-pitch(t)).sin_cos();
        let rx = [[1.0, 0.0, 0.0], [0.0, cr, -sr], [0.0, sr, cr]];
        let ry = [[cp, 0.0, sp], [0.0, 1.0, 0.0], [-sp, 0.0, cp]];
        mat_mul(&ry, &rx)
    }

    /// The camera sits on the machine at an arbitrary angle.
    fn mount() -> M3 {
        let axis = normalize(&[1.0, 2.0, 0.5]);
        let e = |v: Vec3| rotate_about(&v, &axis, 30f64.to_radians());
        let (x, y, z) = (e([1.0, 0.0, 0.0]), e([0.0, 1.0, 0.0]), e([0.0, 0.0, 1.0]));
        // Columns are the images of the body axes.
        [[x[0], y[0], z[0]], [x[1], y[1], z[1]], [x[2], y[2], z[2]]]
    }

    /// Up, specific force and angular velocity at `t`, in the camera frame.
    fn sensors(t: f64) -> (Vec3, Vec3, Vec3) {
        let r = attitude(t);
        let rt = transpose(&r);
        let m = mount();
        let up = mul(&rt, &[0.0, 0.0, 1.0]);
        let v = speed(t);
        let force = mul(&rt, &[accel(t), -v * turn_rate(t), G]);
        // Yaw right is clockwise from above; nose-up pitch is about -y; roll
        // is about the body's own x.
        let roll_axis = mul(&r, &[1.0, 0.0, 0.0]);
        let w_heading = [
            deriv(lean, t) * roll_axis[0],
            deriv(lean, t) * roll_axis[1] - deriv(pitch, t),
            deriv(lean, t) * roll_axis[2] - turn_rate(t),
        ];
        let omega = mul(&rt, &w_heading);
        (mul(&m, &up), mul(&m, &force), mul(&m, &omega))
    }

    fn chunk(key: &str, stmp_s: f64, components: usize, values: Vec<f64>) -> StreamChunk {
        StreamChunk {
            key: FourCc::from_str_lossy(key),
            name: None,
            orin: None,
            units: Vec::new(),
            scal: Vec::new(),
            type_str: None,
            tmpc: None,
            stmp: Some((stmp_s * 1e6).round() as u64),
            tsmp: None,
            components,
            values,
            text: Vec::new(),
        }
    }

    /// Where the bike is at each GPS sample, by integrating heading and speed.
    fn track(duration_s: f64, hz: f64) -> Vec<(f64, f64)> {
        let dt = 1e-3;
        let (mut north, mut east, mut heading) = (0.0f64, 0.0f64, 0.3f64);
        let mut out = Vec::new();
        let mut next = 0.0;
        let mut t = 0.0;
        while t < duration_s + 1.0 {
            if t >= next - 1e-9 {
                out.push((north, east));
                next += 1.0 / hz;
            }
            heading += turn_rate(t) * dt;
            north += speed(t) * heading.cos() * dt;
            east += speed(t) * heading.sin() * dt;
            t += dt;
        }
        out
    }

    pub fn build(ride: &Ride) -> Gpmf {
        let payloads = ride.duration_s.round() as usize;
        let fix_hz = 10.0;
        let positions = track(ride.duration_s, fix_hz);
        let (lat0, lon0) = (44.0f64, 11.0f64);
        let m_per_deg = 111_320.0;
        let start = chrono::NaiveDate::from_ymd_opt(2026, 9, 8)
            .unwrap()
            .and_hms_opt(11, 0, 0)
            .unwrap()
            .and_utc();
        let days = (start.date_naive() - chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap())
            .num_days() as f64;
        let day_secs = 11.0 * 3600.0;

        // The real camera's GRAV frame has Y and Z swapped relative to its
        // IMU; `to_grav_frame` swaps them back for a stream with no ORIN, so a
        // raw sample is the frame vector with Y and Z exchanged.
        let raw = |v: Vec3| -> [f64; 3] {
            let v = if ride.left_handed { [v[0], v[2], v[1]] } else { v };
            [v[0], v[2], v[1]]
        };

        let mut out = Vec::with_capacity(payloads);
        for k in 0..payloads {
            let t0 = k as f64;
            let stmp = ride.clock_offset_s + t0;
            let mut streams = Vec::new();

            let mut grav = Vec::new();
            for i in 0..30 {
                let (up, _, _) = sensors(t0 + i as f64 / 30.0);
                let up = if ride.left_handed { [up[0], up[2], up[1]] } else { up };
                grav.extend_from_slice(&up);
            }
            streams.push(chunk("GRAV", stmp, 3, grav));

            let mut acc = Vec::new();
            let mut gyr = Vec::new();
            for i in 0..200 {
                let (_, f, w) = sensors(t0 + i as f64 / 200.0);
                let f = if ride.accel_backwards { scale(&f, -1.0) } else { f };
                let w = if ride.gyro_backwards { scale(&w, -1.0) } else { w };
                acc.extend_from_slice(&raw(f));
                gyr.extend_from_slice(&raw(w));
            }
            streams.push(chunk("ACCL", stmp, 3, acc));
            if ride.gyro {
                streams.push(chunk("GYRO", stmp, 3, gyr));
            }

            let mut fixes = Vec::new();
            for i in 0..10 {
                let t = t0 + i as f64 / fix_hz;
                let (north, east) = positions[k * 10 + i];
                let lat = lat0 + north / m_per_deg;
                let lon = lon0 + east / (m_per_deg * lat0.to_radians().cos());
                let v = (ride.speed_report)(t, speed(t));
                match ride.gps {
                    Gps::Gps9 => fixes.extend_from_slice(&[
                        lat, lon, 500.0, v, v, days, day_secs + t, 1.2, 3.0,
                    ]),
                    Gps::Gps5 => fixes.extend_from_slice(&[lat, lon, 500.0, v, v]),
                    Gps::None => {}
                }
            }
            let gps_stmp = stmp - ride.gps_lead_s;
            match ride.gps {
                Gps::Gps9 => streams.push(chunk("GPS9", gps_stmp, 9, fixes)),
                Gps::Gps5 => streams.push(chunk("GPS5", gps_stmp, 5, fixes)),
                Gps::None => {}
            }

            out.push(Payload {
                index: k,
                time: t0,
                duration: 1.0,
                devices: vec![Device {
                    id: 1,
                    name: Some("synthetic".into()),
                    streams,
                }],
            });
        }
        Gpmf {
            payloads: out,
            warnings: Vec::new(),
        }
    }

    pub fn compute(ride: &Ride) -> gmax::telemetry::Telemetry {
        gmax::telemetry::compute(&build(ride), &Default::default()).unwrap()
    }
}

use synthetic::Ride;

/// Central difference of a channel against the timeline.
fn derivative(v: &[f64], times: &[f64]) -> Vec<f64> {
    (1..v.len() - 1)
        .map(|i| (v[i + 1] - v[i - 1]) / (times[i + 1] - times[i - 1]))
        .collect()
}

#[test]
fn a_synthetic_ride_comes_out_the_way_it_went_in() {
    // The baseline the fault tests below perturb: a well-behaved camera on a
    // known ride must give back the lean, pitch and rates it was built from.
    let t = synthetic::compute(&Ride::default());
    let truth =
        |f: fn(f64) -> f64| -> Vec<f64> { t.times.iter().map(|&s| f(s).to_degrees()).collect() };
    let r_lean = gmax::math::pearson(&t.lean_deg, &truth(synthetic::lean));
    assert!(r_lean > 0.99, "r(lean, truth) = {r_lean}");
    let r_pitch = gmax::math::pearson(&t.pitch_deg, &truth(synthetic::pitch));
    assert!(r_pitch > 0.9, "r(pitch, truth) = {r_pitch}");
    let r_yaw = gmax::math::pearson(&t.yaw_rate_dps, &truth(synthetic::turn_rate));
    assert!(r_yaw > 0.99, "r(yaw rate, truth) = {r_yaw}");
    assert!(!t.lean_sign.turn_rate_inverted);
    assert_eq!(t.lean_sign.confidence, Confidence::High, "{:?}", t.lean_sign);
    assert!(t.lean_zero.is_some() && t.grav_tilt.is_some());
}

#[test]
fn yaw_rate_is_positive_right_even_when_the_gyro_turns_the_other_way() {
    // GPS course rate is the one reference whose sign owes nothing to the
    // camera. When it shows the gyro's rate of turn to be backwards, lean was
    // already signed against the corrected rate -- but the yaw channel itself
    // was written out uncorrected, reporting every right-hander as a left.
    let t = synthetic::compute(&Ride {
        gyro_backwards: true,
        ..Default::default()
    });
    assert!(t.lean_sign.turn_rate_inverted, "{:?}", t.lean_sign);

    let truth: Vec<f64> = t.times.iter().map(|&s| synthetic::turn_rate(s)).collect();
    let r = gmax::math::pearson(&t.yaw_rate_dps, &truth);
    assert!(r > 0.99, "r(yaw rate, true rightward turn rate) = {r}");
    // And it agrees with the other lateral channels, as it must.
    assert!(gmax::math::pearson(&t.lean_deg, &t.yaw_rate_dps) > 0.9);
    let r_lean = gmax::math::pearson(
        &t.lean_deg,
        &t.times.iter().map(|&s| synthetic::lean(s)).collect::<Vec<_>>(),
    );
    assert!(r_lean > 0.99, "lean must be right either way: r = {r_lean}");
}

#[test]
fn pitch_rate_turns_the_same_way_as_pitch_whatever_the_frame() {
    // Pitch rate sits beside pitch in every output, so it must be its rate:
    // positive while the nose is rising. It came out positive nose-down on the
    // reference clip, because it was read about an axis whose sign was fixed
    // by the lean calibration and in a frame whose handedness nobody checked.
    // The real camera's frame is left-handed and a gyro can be wired either
    // way, so all four combinations are tried.
    for (left_handed, gyro_backwards) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let t = synthetic::compute(&Ride {
            left_handed,
            gyro_backwards,
            ..Default::default()
        });
        let rate = t.pitch_rate_dps.as_ref().unwrap();
        let dpitch = derivative(&t.pitch_deg, &t.times);
        // `pitch_deg` is smoothed over half a second, so compare like with
        // like.
        let smooth = gmax::math::moving_average(rate, 15);
        let r = gmax::math::pearson(&smooth[1..t.n - 1], &dpitch);
        assert!(
            r > 0.9,
            "left-handed {left_handed}, gyro backwards {gyro_backwards}: \
             r(pitch rate, d(pitch)/dt) = {r}"
        );
        // And pitch itself is nose-up positive.
        let truth: Vec<f64> = t.times.iter().map(|&s| synthetic::pitch(s)).collect();
        assert!(gmax::math::pearson(&t.pitch_deg, &truth) > 0.9);
    }
}

#[test]
fn pitch_rate_turns_the_same_way_as_pitch_on_the_reference_clip() {
    let t = telemetry_or_skip!();
    let rate = t.pitch_rate_dps.as_ref().unwrap();
    let dpitch = derivative(&t.pitch_deg, &t.times);
    let smooth = gmax::math::moving_average(rate, 15);
    let r = gmax::math::pearson(&smooth[1..t.n - 1], &dpitch);
    // Measured: 0.81. It was -0.2 when the rate was read nose-down positive.
    assert!(r > 0.6, "r(pitch rate, d(pitch)/dt) = {r}");
}

#[test]
fn without_a_gyro_nothing_passes_for_straight_line_riding() {
    // A missing gyro used to read as a turn rate of exactly zero, which every
    // gate took for dead straight: every corner of the ride went into the
    // mount calibration, the roll zero and the gravity fit.
    let t = synthetic::compute(&Ride {
        gyro: false,
        ..Default::default()
    });
    assert!(t.grav_tilt.is_none(), "the gravity fit ran on {:?}", t.grav_tilt);
    assert!(t.lean_zero.is_none(), "a roll zero was fitted to unknown riding");
    assert!(
        t.warnings.iter().any(|w| w.contains("no GYRO stream")),
        "{:?}",
        t.warnings
    );
    // A rate that was never measured is not zero, and the CSV leaves it blank.
    assert!(t.yaw_rate_dps.iter().all(|v| v.is_nan()));
    let mut csv = Vec::new();
    telemetry::output::write_csv_to(&t, &mut csv).unwrap();
    let csv = String::from_utf8(csv).unwrap();
    let col = telemetry::output::COLUMNS.iter().position(|c| *c == "yaw_rate_dps").unwrap();
    let row = csv.lines().nth(100).unwrap();
    assert_eq!(row.split(',').nth(col), Some(""), "row: {row}");
    // Lean itself still works: it needs only gravity.
    assert!(t.lean_deg.iter().any(|l| l.abs() > 20.0));
}

#[test]
fn an_upside_down_accelerometer_is_not_mistaken_for_a_small_tilt() {
    // Two vectors pointing almost opposite ways have a short cross product,
    // just like two nearly parallel ones, so an angle taken from the cross
    // product alone read an accelerometer 180 degrees out as a tilt of a few
    // degrees -- and applied it.
    let t = synthetic::compute(&Ride {
        accel_backwards: true,
        ..Default::default()
    });
    let tilt = t.grav_tilt.as_ref().expect("the ride has coasting to measure it over");
    assert!(tilt.angle_deg > 170.0, "measured {:.1}°", tilt.angle_deg);
    assert!(!tilt.applied, "a {:.0}° rotation was applied to GRAV", tilt.angle_deg);
    assert!(t.warnings.iter().any(|w| w.contains("too much to be a sensor")));

    // Whereas a healthy camera measures next to nothing, since the synthetic
    // GRAV is exact.
    let good = synthetic::compute(&Ride::default());
    let tilt = good.grav_tilt.as_ref().unwrap();
    assert!(tilt.applied && tilt.angle_deg < 1.0, "measured {:.2}°", tilt.angle_deg);
}

#[test]
fn a_speed_the_imu_cannot_have_been_doing_is_rejected() {
    // The reference clip's receiver, reacquiring in a hairpin, reported a
    // third of the true speed through a corner whose lateral load and rate of
    // turn pin the speed down -- smoothly, so no jump check sees it, and with
    // positions just as wrong, so the position check does not either. Here the
    // receiver does the same through one right-hander.
    let t = synthetic::compute(&Ride {
        speed_report: |t, v| if (120.0..126.0).contains(&t) { v * 0.3 } else { v },
        ..Default::default()
    });
    let rejected: Vec<f64> = t
        .gps
        .iter()
        .filter(|f| f.reject == Some(Reject::ImplausibleSpeed))
        .map(|f| f.time)
        .collect();
    assert!(rejected.len() >= 10, "only {} fixes rejected", rejected.len());
    assert!(
        rejected.iter().all(|&s| (120.0..126.0).contains(&s)),
        "an honest fix was rejected: {rejected:?}"
    );
    // The mid-corner speed no longer reaches anything a reader sees.
    let (i, _) = t.times.iter().enumerate().find(|(_, &s)| s >= 123.0).unwrap();
    assert_eq!(t.speed_at(i), None);
}

#[test]
fn a_speed_that_jumps_further_than_a_machine_can_accelerate_is_rejected() {
    let t = synthetic::compute(&Ride {
        // One fix ten metres a second out, on a straight where the IMU has no
        // corner to check it against.
        speed_report: |t, v| if (8.0..8.05).contains(&t) { v + 10.0 } else { v },
        ..Default::default()
    });
    let rejected: Vec<f64> = t
        .gps
        .iter()
        .filter(|f| f.reject == Some(Reject::ImplausibleSpeed))
        .map(|f| f.time)
        .collect();
    assert_eq!(rejected.len(), 1, "rejected {rejected:?}");
    assert!((rejected[0] - 8.0).abs() < 1e-6);
}

#[test]
fn a_ride_with_no_gps_can_still_rank_its_lean() {
    // With no receiver there is no clock and no position, but lean and the IMU
    // channels are as good as ever. Samples used to be dropped for want of a
    // UTC time, leaving "need at least 2 samples" and nothing else.
    let t = synthetic::compute(&Ride {
        gps: synthetic::Gps::None,
        ..Default::default()
    });
    assert!(t.t0.is_none());
    assert!(!t.t0_is_fallback, "an absent clock is not a fallback clock");

    let samples = gmax::moments::samples_from_telemetry(&t);
    assert_eq!(samples.len(), t.n, "sampled on the IMU's own timeline");
    assert!(samples.iter().all(|s| s.pos.is_none() && s.speed_ms.is_none()));
    let a = gmax::moments::analyse(&samples, &Default::default()).unwrap();
    assert!(!a.has_absolute_clock());
    let lean = a.metrics.iter().find(|m| m.id == gmax::moments::MetricId::Lean).unwrap();
    assert!(lean.entries[0].value.abs() > 30.0, "top lean {}", lean.entries[0].value);
    // Times from the start of the recording must not print as 1970.
    let text = gmax::moments::output::render_text(&a);
    assert!(!text.contains("1970"), "{text}");
    assert!(text.contains("no GPS clock"), "{text}");
    let json = gmax::moments::output::render_json(&a);
    assert!(json["ride"]["start"].is_null());
    assert!(json["metrics"]["lean"]["entries"][0]["timestamp"].is_null());
}

#[test]
fn a_ride_whose_gps_has_no_clock_keeps_its_positions() {
    // GPS5 reports positions but no time. They are still worth having.
    let t = synthetic::compute(&Ride {
        gps: synthetic::Gps::Gps5,
        ..Default::default()
    });
    assert!(t.t0.is_none() && !t.t0_is_fallback);
    let samples = gmax::moments::samples_from_telemetry(&t);
    assert_eq!(samples.len(), t.gps.len(), "one sample per fix");
    assert!(samples.iter().filter(|s| s.pos.is_some()).count() > samples.len() / 2);
    let a = gmax::moments::analyse(&samples, &Default::default()).unwrap();
    assert!(!a.has_absolute_clock());
    assert!(a.distance_m > 1000.0, "distance {}", a.distance_m);
}

#[test]
fn moments_on_a_cut_clip_land_inside_the_clip() {
    // A stream copy keeps its parent's GPMF timestamps, so this clip's clock
    // starts eighteen minutes in. Reading those timestamps as positions in the
    // video put every moment past the end of the file, and planning dropped
    // them all.
    let ride = Ride {
        duration_s: 120.0,
        clock_offset_s: 1098.0,
        gps_lead_s: 0.04,
        ..Default::default()
    };
    let t = synthetic::compute(&ride);
    assert!(t.times[0] >= 1098.0);
    let samples = gmax::moments::samples_from_telemetry(&t);
    let first = samples[0].video_time_s.unwrap();
    assert!(first.abs() < 0.2, "the clip's first sample is {first} s into it");
    // Including the fix taken just before the first IMU sample, which would
    // otherwise sit a few milliseconds before the start of the video.
    assert!(samples.iter().all(|s| s.video_time_s.unwrap() >= 0.0));

    let a = gmax::moments::analyse(&samples, &Default::default()).unwrap();
    let opts = gmax::moments::clips::PlanOptions {
        duration_s: Some(120.0),
        ..Default::default()
    };
    let (clips, dropped) = gmax::moments::clips::plan(&a, &opts);
    assert!(!clips.is_empty());
    assert!(
        dropped.iter().all(|d| !d.reason.contains("outside the video")),
        "{:?}",
        dropped.iter().map(|d| &d.reason).collect::<Vec<_>>()
    );
}

#[test]
fn a_slice_shows_no_position_it_has_not_reached() {
    let t = synthetic::compute(&Ride::default());
    // Start the slice between two fixes, as a cut almost always does.
    let (from, to) = t.range_covering(100.05, 110.0);
    let s = t.slice(from, to);
    // Its first samples are covered by the fix taken just before the cut.
    let fix = s.fix_at(0).expect("the fix in effect at the cut");
    assert!(fix.time <= 0.0, "the slice's first sample shows a fix from {} s", fix.time);
    for i in 0..s.n {
        if let Some(f) = s.fix_at(i) {
            assert!(f.time <= s.times[i] + 1e-9, "sample {i} shows a fix from its future");
        }
    }
    // And an empty slice is empty, not a panic.
    let empty = t.slice(from, from);
    assert_eq!(empty.n, 0);
    assert!(empty.utc_of(0.0).is_none());
}

#[test]
fn the_roll_zero_bridges_a_long_corner_sequence_from_the_riding_either_side() {
    // Upright riding for two minutes, five minutes of corners with no straight
    // in them, then three upright again -- on a mount whose zero sits at -6
    // degrees before and +6 after. Where there is nothing to measure, the zero
    // must come from the measurements either side, not from the whole-file
    // median: that is the static zero this exists to improve on, and falling
    // back to it put a step into the lean at both ends of the gap.
    let hz = 30.0;
    let n = (600.0 * hz) as usize;
    let times: Vec<f64> = (0..n).map(|i| i as f64 / hz).collect();
    let cornering = |t: f64| (120.0..420.0).contains(&t);
    let zero_at = |t: f64| if t < 270.0 { -6.0 } else { 6.0 };
    let lean: Vec<f64> = times
        .iter()
        .map(|&t| {
            let corner = if cornering(t) { 40.0 * (t * std::f64::consts::TAU / 20.0).sin() } else { 0.0 };
            zero_at(t) + corner
        })
        .collect();
    let upright: Vec<usize> = (0..n).filter(|&i| !cornering(times[i])).collect();
    let zero = telemetry::rolling_roll_zero(&lean, &times, &upright).unwrap();

    let at = |t: f64| zero[(t * hz) as usize];
    // Either side of the gap, the local zero.
    assert!((at(60.0) + 6.0).abs() < 0.5, "zero before the corners {}", at(60.0));
    assert!((at(500.0) - 6.0).abs() < 0.5, "zero after the corners {}", at(500.0));
    // The widened window still reaches upright riding for its 90 s half-width
    // into the corners, so the zero is measured to 210 s and from 330 s. In
    // between it runs straight from one to the other. The whole-file median
    // it used to drop to is +6 here -- there is more riding after the gap
    // than before it -- so the old fallback read +6 all the way across.
    for &(t, want) in &[(220.0, -5.0), (270.0, 0.0), (320.0, 5.0)] {
        assert!((at(t) - want).abs() < 1.0, "zero at {t} s is {:.2}, expected {want}", at(t));
    }
}
