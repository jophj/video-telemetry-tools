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

    // The prototype's 20 columns, in order, with ours appending two.
    let their_header: Vec<&str> = their_lines[0].split(',').collect();
    let our_header: Vec<&str> = our_lines[0].split(',').collect();
    assert_eq!(&our_header[..their_header.len()], &their_header[..]);
    assert_eq!(&our_header[their_header.len()..], &["gps_fix", "gps_source"]);

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

    // Lean is inverted and nothing else about it changed: a correlation of
    // exactly -1 proves the fix is a pure sign flip.
    let (a, b) = paired(
        &col(&our_lines, &our_header, "lean_deg"),
        &col(&their_lines, &their_header, "lean_deg"),
    );
    let r = gmax::math::pearson(&a, &b);
    assert!(r < -0.999, "r(our lean, their lean) = {r}, expected -1");

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
