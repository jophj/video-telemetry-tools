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
    // With the plausibility check off, the gate is the prototype's: dilution of
    // precision and a null position. The prototype's viewer reported
    // gps_fix_pct 54.9 for this clip.
    let t = telemetry::compute(
        &g,
        &telemetry::Options {
            max_speed_ratio: 0.0,
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
    let mut by_minute: std::collections::BTreeMap<usize, Vec<f64>> = Default::default();
    for i in 0..t.n {
        let turning = t.yaw_rate_dps[i].abs() >= 2.0;
        if turning || t.speed_at(i).unwrap_or(0.0) <= 5.0 {
            continue;
        }
        by_minute
            .entry((t.times[i] / 60.0) as usize)
            .or_default()
            .push(t.lean_deg[i]);
    }
    let worst = by_minute
        .iter()
        .filter(|(_, v)| v.len() >= 30)
        .map(|(m, v)| (m, gmax::math::median(v)))
        .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
        .expect("the reference clip contains straight-line riding");
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

    // The physical fact the correction rests on: a machine rolling straight,
    // upright, and neither braking nor driving has only gravity acting on it,
    // so there is nothing to push it sideways. Any lateral reading there is
    // the camera's idea of vertical being wrong, and it lands in this channel
    // whole because nothing else references it.
    let alat = t.accel_lat_g.as_ref().expect("the camera has an IMU");
    let speed = |i: usize| t.speed_at(i).unwrap_or(0.0);
    let level: Vec<f64> = (0..t.n)
        .filter(|&i| {
            speed(i) > 5.0 && t.yaw_rate_dps[i].abs() < 2.0 && t.accel_lon_g.as_ref().unwrap()[i].abs() < 0.05
        })
        .map(|i| alat[i])
        .collect();
    assert!(level.len() > 200, "only {} level samples", level.len());
    let bias = gmax::math::median(&level);
    assert!(
        bias.abs() < 0.04,
        "straight, level riding reports {bias:+.4} g across the machine ({:+.1}° of cornering \
         load that is not there)",
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
    // is the whole point. Straight-line riding must stop reporting cornering
    // load it is not carrying.
    let bias = |t: &Telemetry| {
        let alat = t.accel_lat_g.as_ref().unwrap();
        let v: Vec<f64> = (0..t.n)
            .filter(|&i| t.speed_at(i).unwrap_or(0.0) > 5.0 && t.yaw_rate_dps[i].abs() < 2.0)
            .map(|i| alat[i])
            .collect();
        gmax::math::median(&v)
    };
    let (before, after) = (bias(&raw), bias(&corrected));
    assert!(
        after.abs() < before.abs() / 2.0,
        "straight-line lateral load went from {before:+.4} g to {after:+.4} g"
    );

    // Total acceleration is a magnitude and owes nothing to any frame, so it
    // must come through untouched. If this moves, the correction is not a
    // rotation and something is badly wrong.
    let (x, y) = (
        corrected.accel_total_g.as_ref().unwrap(),
        raw.accel_total_g.as_ref().unwrap(),
    );
    let worst = x.iter().zip(y).map(|(a, b)| (a - b).abs()).fold(0.0f64, f64::max);
    assert!(worst < 1e-9, "total acceleration moved by {worst}");
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
    // viewer's index-to-position mapping still lines up.
    assert!(!s.gps.is_empty(), "ten seconds of ride has GPS in it");
    for f in &s.gps {
        assert!(
            f.time >= -1e-9 && f.time <= s.duration_s + 1e-9,
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
