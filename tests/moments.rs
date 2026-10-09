//! The moments engine: cadence independence, detection, ranking.

mod common;
use common::fixture_or_skip;

use gmax::moments::{self, MetricId, Options, Sample};

/// A synthetic ride, generated at whatever cadence is asked for.
///
/// Lean is a 40-degree sine of period 8 s, so a 120 s ride contains 30
/// half-cycles and therefore about 30 corners. Speed is a 40 s sine about
/// 25 m/s, whose derivative peaks at 10 * 2π/40 = 1.571 m/s². Those two figures
/// are the ground truth every cadence must recover.
///
/// Lateral g is `tan(lean)`, the load a machine at that angle would actually
/// be carrying, rather than a constant. A ride that leans 40° while measuring
/// 0.02 g across itself is not a ride, and the engine now says so.
fn synthetic(hz: f64, duration_s: f64, jitter: f64) -> Vec<Sample> {
    use std::f64::consts::TAU;
    let n = (duration_s * hz) as usize;
    let t0 = 1_788_601_214.0f64;
    (0..n)
        .map(|i| {
            let base = i as f64 / hz;
            // A deterministic wobble, to exercise the irregular-cadence path.
            let t = base + if jitter > 0.0 { ((i * 7919) % 101) as f64 / 101.0 * jitter } else { 0.0 };
            Sample {
                t: t0 + t,
                pos: Some((44.17 + t * 1e-5, 11.33 + t * 1e-5)),
                ele_m: Some(1000.0),
                speed_ms: Some(25.0 + 10.0 * (t * TAU / 40.0).sin()),
                lean_deg: Some(40.0 * (t * TAU / 8.0).sin()),
                accel_lon_g: Some(0.01),
                accel_lat_g: Some((40.0 * (t * TAU / 8.0).sin()).to_radians().tan()),
                accel_total_g: Some(1.0),
                video_time_s: Some(t),
            }
        })
        .collect()
}

#[test]
fn cadence_is_measured_not_assumed() {
    for hz in [1.0, 10.0, 50.0] {
        let s = synthetic(hz, 120.0, 0.0);
        let c = moments::derive_cadence(&s, 5.0);
        let measured = c.hz.unwrap();
        assert!(
            (measured - hz).abs() < hz * 0.01,
            "at {hz} Hz the measured rate was {measured}"
        );
        assert!(c.regular, "an evenly spaced ride should read as regular");
        // The gap threshold scales with the cadence, so "a gap" means the same
        // thing at every rate.
        assert!(
            (c.gap_threshold - 5.0 / hz).abs() < 1e-6,
            "gap threshold {} at {hz} Hz",
            c.gap_threshold
        );
    }
}

#[test]
fn a_long_gap_does_not_move_the_median() {
    let mut s = synthetic(10.0, 60.0, 0.0);
    // Shift the back half a hundred seconds into the future.
    for sample in s.iter_mut().skip(300) {
        sample.t += 100.0;
    }
    let c = moments::derive_cadence(&s, 5.0);
    assert!((c.hz.unwrap() - 10.0).abs() < 0.1, "hz was {:?}", c.hz);
    assert_eq!(c.gaps.len(), 1);
    assert!((c.max_dt - 100.1).abs() < 0.01);
}

#[test]
fn jittered_sampling_reads_as_irregular() {
    let s = synthetic(10.0, 120.0, 0.08);
    let c = moments::derive_cadence(&s, 5.0);
    assert!(!c.regular, "p5..p95 was {:.4}..{:.4}", c.p5_dt, c.p95_dt);
}

#[test]
fn fewer_than_two_samples_yields_no_cadence() {
    let c = moments::derive_cadence(&synthetic(1.0, 1.0, 0.0)[..1], 5.0);
    assert!(c.hz.is_none());
    assert!(!c.regular);
    assert!(c.gaps.is_empty());
}

#[test]
fn results_are_the_same_at_every_cadence() {
    // The keystone. Nothing in the engine may use a sample count as a proxy for
    // elapsed time, and this is what proves it: the same ride, sampled at rates
    // an order of magnitude apart, must agree on how many corners it contains,
    // where the densest window is, and how long that window is.
    let mut counts = Vec::new();
    let mut window_counts = Vec::new();
    let mut peak_leans = Vec::new();
    let mut peak_accels = Vec::new();

    for hz in [1.0, 5.0, 10.0, 25.0, 50.0] {
        let s = synthetic(hz, 120.0, 0.0);
        let a = moments::analyse(&s, &Options::default()).unwrap();

        // About 30 half-cycles of a 8 s sine over 120 s.
        let curves = a.quality.curves_detected;
        assert!(
            (28..=31).contains(&curves),
            "at {hz} Hz, {curves} curves detected"
        );
        counts.push(curves);

        let curve_metric = a.metrics.iter().find(|m| m.id == MetricId::Curves).unwrap();
        let top = &curve_metric.entries[0];
        window_counts.push(top.curve_count.unwrap());
        // A ten-second window is ten seconds long at every rate.
        assert!(
            (top.span_s() - 10.0).abs() < 1e-6,
            "at {hz} Hz the window was {:.4} s",
            top.span_s()
        );

        let lean = a.metrics.iter().find(|m| m.id == MetricId::Lean).unwrap();
        let peak = lean.entries[0].value.abs();
        // A sampled maximum can never exceed the true amplitude.
        assert!(peak <= 40.0 + 1e-9, "at {hz} Hz peak lean was {peak}");
        peak_leans.push(peak);

        // The speed sine's derivative peaks at 10 * 2π/40.
        let accel = a.metrics.iter().find(|m| m.id == MetricId::Accel).unwrap();
        let p = accel.entries[0].value;
        assert!(
            (p - 10.0 * std::f64::consts::TAU / 40.0).abs() < 0.05,
            "at {hz} Hz peak acceleration was {p}"
        );
        peak_accels.push(p);

        // Ranked moments stay at least min-gap apart in *seconds*.
        for m in &a.metrics {
            for (i, x) in m.entries.iter().enumerate() {
                for y in &m.entries[i + 1..] {
                    let gap = (x.t - y.t).abs();
                    let required = if m.id == MetricId::Curves { 10.0 } else { 5.0 };
                    assert!(
                        gap >= required - 1e-9,
                        "{:?} entries {:.3} s apart at {hz} Hz",
                        m.id,
                        gap
                    );
                }
            }
        }
    }

    // The densest window holds the same number of corners at every rate.
    assert!(
        window_counts.windows(2).all(|w| w[0] == w[1]),
        "densest window counts differed across cadences: {window_counts:?}"
    );
    // Peak lean is recovered better as sampling gets finer, and nearly exactly
    // at 50 Hz. This is the R2b point: a reported maximum is a maximum of the
    // samples present.
    assert!(
        peak_leans.last().unwrap() > &39.9,
        "at 50 Hz peak lean was only {:?}",
        peak_leans.last()
    );
    assert!(
        peak_leans[0] <= peak_leans[peak_leans.len() - 1],
        "peak lean should not get worse with finer sampling: {peak_leans:?}"
    );
}

#[test]
fn non_maximum_suppression_keeps_distinct_moments() {
    // Four candidates: a cluster at 100, 101, 102 and one at 200. With a
    // five-second guard the answer is two moments, not four.
    let t0 = 1_788_601_214.0;
    let samples: Vec<Sample> = [(100.0, 10.0), (101.0, 11.0), (102.0, 12.0), (200.0, 9.0)]
        .iter()
        .map(|&(t, lean)| Sample {
            t: t0 + t,
            pos: Some((44.0, 11.0)),
            lean_deg: Some(lean),
            speed_ms: Some(20.0),
            ..Default::default()
        })
        .collect();
    let a = moments::analyse(
        &samples,
        &Options {
            metrics: vec![MetricId::Lean],
            ..Default::default()
        },
    )
    .unwrap();
    let e = &a.metrics[0].entries;
    assert_eq!(e.len(), 2, "got {} entries", e.len());
    // Returned in score order: the 12-degree peak first.
    assert!((e[0].value - 12.0).abs() < 1e-9);
    assert!((e[1].value - 9.0).abs() < 1e-9);
}

#[test]
fn ties_break_to_the_earlier_moment() {
    let t0 = 1_788_601_214.0;
    let samples: Vec<Sample> = [(10.0, 30.0), (100.0, 30.0)]
        .iter()
        .map(|&(t, lean)| Sample {
            t: t0 + t,
            pos: Some((44.0, 11.0)),
            lean_deg: Some(lean),
            ..Default::default()
        })
        .collect();
    let a = moments::analyse(
        &samples,
        &Options {
            top: 1,
            metrics: vec![MetricId::Lean],
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        (a.metrics[0].entries[0].offset_s - 0.0).abs() < 1e-9,
        "the tie should go to the earlier moment"
    );
}

#[test]
fn lean_limit_is_discovered_not_hardcoded() {
    let mk = |leans: &[f64]| -> Vec<Sample> {
        leans
            .iter()
            .enumerate()
            .map(|(i, &l)| Sample {
                t: 1_788_601_214.0 + i as f64,
                lean_deg: Some(l),
                ..Default::default()
            })
            .collect()
    };
    // A repeated extreme means the sensor is clipping.
    let l = moments::detect_lean_limit(&mk(&[-55.0, 55.0, 30.0, 55.0, 12.0])).unwrap();
    assert_eq!(l.value, 55.0);
    assert_eq!(l.samples, 3);
    // Nothing here knows the number 55.
    let l = moments::detect_lean_limit(&mk(&[-42.5, 42.5, 10.0])).unwrap();
    assert_eq!(l.value, 42.5);
    // A unique maximum is a real peak, not a clip.
    assert!(moments::detect_lean_limit(&mk(&[10.0, 20.0, 30.0])).is_none());
    assert!(moments::detect_lean_limit(&[]).is_none());
}

#[test]
fn curve_detection_uses_hysteresis() {
    let mk = |leans: &[f64]| -> Vec<Sample> {
        leans
            .iter()
            .enumerate()
            .map(|(i, &l)| Sample {
                t: 1_788_601_214.0 + i as f64,
                lean_deg: Some(l),
                ..Default::default()
            })
            .collect()
    };
    // Two separate corners, returning through neutral between them.
    assert_eq!(
        moments::detect_curves(&mk(&[0.0, 10.0, 25.0, 10.0, 0.0, 12.0, 30.0, 8.0, 0.0]), 15.0).len(),
        2
    );
    // A sign reversal starts a new corner, and each is timed at its own peak.
    let c = moments::detect_curves(&mk(&[0.0, 20.0, 30.0, -25.0, -30.0, 0.0]), 15.0);
    assert_eq!(c.len(), 2);
    assert_eq!(c[0].lean_deg, 30.0);
    assert_eq!(c[1].lean_deg, -30.0);
    // Nothing beyond the threshold is not a corner.
    assert!(moments::detect_curves(&mk(&[0.0, 5.0, 10.0, 5.0, 0.0]), 15.0).is_empty());
    // The point of the neutral band: dipping to 8, above the 7.5 half-threshold,
    // is mid-corner and not the end of one.
    assert_eq!(
        moments::detect_curves(&mk(&[0.0, 20.0, 25.0, 8.0, 22.0, 30.0, 0.0]), 15.0).len(),
        1
    );
    // A corner still in progress when the ride ends still happened.
    assert_eq!(moments::detect_curves(&mk(&[0.0, 20.0, 30.0]), 15.0).len(), 1);
}

#[test]
fn acceleration_is_gated_on_gaps_and_on_physics() {
    let mk = |pts: &[(f64, f64)]| -> Vec<Sample> {
        pts.iter()
            .map(|&(t, v)| Sample {
                t: 1_788_601_214.0 + t,
                speed_ms: Some(v),
                ..Default::default()
            })
            .collect()
    };
    // +10 m/s over 2 s is 5 m/s², attributed to the later sample.
    let a = moments::derive_acceleration_from_gps(&mk(&[(0.0, 10.0), (2.0, 20.0)]), 5.0, 15.0);
    assert_eq!(a[0], None, "the first sample can have no derivative");
    assert_eq!(a[1], Some(5.0));
    // Across a gap the bike accelerated unobserved.
    let a = moments::derive_acceleration_from_gps(&mk(&[(0.0, 10.0), (10.0, 20.0)]), 5.0, 15.0);
    assert_eq!(a[1], None);
    // Above the ceiling it is not a measurement.
    let a = moments::derive_acceleration_from_gps(&mk(&[(0.0, 0.0), (1.0, 100.0)]), 5.0, 15.0);
    assert_eq!(a[1], None);
    // A missing speed either side yields nothing.
    let mut s = mk(&[(0.0, 10.0), (1.0, 20.0)]);
    s[0].speed_ms = None;
    assert_eq!(moments::derive_acceleration_from_gps(&s, 5.0, 15.0)[1], None);
}

#[test]
fn a_missing_channel_only_skips_its_own_metric() {
    // Lean present, speed absent: the lean metric still works.
    let mut s = synthetic(10.0, 60.0, 0.0);
    for sample in &mut s {
        sample.speed_ms = None;
    }
    let a = moments::analyse(&s, &Options::default()).unwrap();
    let lean = a.metrics.iter().find(|m| m.id == MetricId::Lean).unwrap();
    assert!(!lean.entries.is_empty(), "lean should survive missing speed");
    let speed = a.metrics.iter().find(|m| m.id == MetricId::Speed).unwrap();
    assert!(speed.unavailable.is_some(), "speed should report unavailable");
    assert_eq!(a.quality.missing_speed, s.len());
}

#[test]
fn position_may_be_unknown_while_lean_is_not() {
    // On the reference clip the receiver has no usable fix for 45% of the ride
    // while the IMU works throughout, so a moment can be real but unplaceable.
    let mut s = synthetic(10.0, 60.0, 0.0);
    for sample in &mut s {
        sample.pos = None;
    }
    let a = moments::analyse(&s, &Options::default()).unwrap();
    let lean = a.metrics.iter().find(|m| m.id == MetricId::Lean).unwrap();
    assert!(!lean.entries.is_empty());
    assert!(lean.entries[0].pos.is_none());
    // And no position means no distance, rather than a nonsense one.
    assert_eq!(a.distance_m, 0.0);
}

#[test]
fn analysis_is_deterministic() {
    let s = synthetic(10.0, 120.0, 0.0);
    let a = moments::analyse(&s, &Options::default()).unwrap();
    let b = moments::analyse(&s, &Options::default()).unwrap();
    let ja = serde_json::to_string(&moments::output::render_json(&a)).unwrap();
    let jb = serde_json::to_string(&moments::output::render_json(&b)).unwrap();
    assert_eq!(ja, jb);
}

#[test]
fn offsets_carry_correctly_when_rounding_up() {
    // .9996 s must carry into the next second rather than printing ".1000".
    assert_eq!(moments::output::format_offset(0.9996), "+00:00:01.000");
    assert_eq!(moments::output::format_offset(0.0), "+00:00:00.000");
    assert_eq!(moments::output::format_offset(3661.5), "+01:01:01.500");
    assert_eq!(moments::output::format_offset(-1.25), "-00:00:01.250");
}

// ---------------------------------------------------------- real material

#[test]
fn analyses_the_reference_clip() {
    let path = fixture_or_skip!(common::REFERENCE_360);
    let mut f = gmax::mp4::Mp4::open(&path).unwrap();
    let g = gmax::gpmf::from_mp4(&mut f).unwrap();
    let t = gmax::telemetry::compute(&g, &Default::default()).unwrap();
    let samples = moments::samples_from_telemetry(&t);
    assert_eq!(samples.len(), 15375);

    let a = moments::analyse(&samples, &Options::default()).unwrap();
    // The receiver runs at 10 Hz, whatever the IMU does.
    assert!((a.cadence.hz.unwrap() - 10.0).abs() < 0.05);
    // Distance must agree with what the telemetry module reports, to within
    // rounding: both apply the same coverage rule.
    let expected = t.distance_m() / 1000.0;
    let got = a.distance_m / 1000.0;
    assert!(
        (got - expected).abs() < 0.05,
        "analyse says {got:.2} km, telemetry says {expected:.2} km"
    );
    // A 25-minute ride is not thousands of kilometres: the no-fix samples must
    // not contribute a position.
    assert!(got > 5.0 && got < 40.0, "distance was {got} km");

    // The camera has a vehicle-frame IMU, so cornering load is available where
    // a phone export has nothing to offer.
    let lateral = a.metrics.iter().find(|m| m.id == MetricId::Lateral).unwrap();
    assert!(!lateral.entries.is_empty());
    assert!(lateral.entries[0].value.abs() > 0.4, "peak cornering load looks low");
    assert!(lateral.entries[0].value.abs() < 1.5, "peak cornering load looks impossible");
}

#[test]
fn chapters_are_shifted_and_clipped_onto_a_segment() {
    // Chapters are written in the source's time base. Embedding them in a
    // segment cut with --ss without adjustment puts a 25-second file's markers
    // twenty minutes in, because ffmpeg takes the timestamps literally.
    let text = "\
;FFMETADATA1
title=GS010527

[CHAPTER]
TIMEBASE=1/1000
START=100000
END=110000
title=Before the segment

[CHAPTER]
TIMEBASE=1/1000
START=950000
END=968000
title=Straddles the start

[CHAPTER]
TIMEBASE=1/1000
START=970000
END=975000
title=Inside

[CHAPTER]
TIMEBASE=1/1000
START=1200000
END=1210000
title=After the segment
";
    // The segment runs 955..980 s.
    let out = gmax::moments::clips::shift_ffmetadata(text, 955.0, Some(25.0));

    // The file header survives.
    assert!(out.starts_with(";FFMETADATA1"));
    assert!(out.contains("title=GS010527"));

    // Only the two overlapping chapters remain.
    assert_eq!(out.matches("[CHAPTER]").count(), 2, "got:\n{out}");
    assert!(!out.contains("Before the segment"));
    assert!(!out.contains("After the segment"));

    // The straddling one begins before the segment does, so it is trimmed to
    // the segment's start and rebased to zero.
    assert!(out.contains("title=Straddles the start"));
    assert!(out.contains("START=0\n"), "got:\n{out}");
    assert!(out.contains("END=13000\n"), "got:\n{out}");

    // The wholly-inside one keeps its length, shifted.
    assert!(out.contains("START=15000\n"), "got:\n{out}");
    assert!(out.contains("END=20000\n"), "got:\n{out}");

    // Every chapter ends up inside the segment.
    for line in out.lines() {
        for prefix in ["START=", "END="] {
            if let Some(v) = line.strip_prefix(prefix) {
                let ms: i64 = v.trim().parse().unwrap();
                assert!((0..=25_000).contains(&ms), "{line} is outside the segment");
            }
        }
    }
}

#[test]
fn chapters_are_untouched_without_a_duration_limit() {
    let text = ";FFMETADATA1\n\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=10000\nEND=20000\ntitle=x\n";
    // Only a start offset: everything after it is kept, shifted.
    let out = gmax::moments::clips::shift_ffmetadata(text, 5.0, None);
    assert!(out.contains("START=5000\n"), "got:\n{out}");
    assert!(out.contains("END=15000\n"), "got:\n{out}");
}

/// A ride of identical corners, leaning `lean` degrees with `supported`
/// degrees' worth of lateral load measured across the machine.
fn corners(lean: f64, supported: f64) -> Vec<Sample> {
    use std::f64::consts::TAU;
    let t0 = 1_788_601_214.0f64;
    (0..1200)
        .map(|i| {
            let t = i as f64 / 10.0;
            let shape = (t * TAU / 20.0).sin();
            Sample {
                t: t0 + t,
                pos: Some((44.17, 11.33)),
                speed_ms: Some(20.0),
                lean_deg: Some(lean * shape),
                accel_lat_g: Some((supported * shape).to_radians().tan()),
                ..Default::default()
            }
        })
        .collect()
}

fn lean_warning(samples: &[Sample]) -> Option<String> {
    let a = moments::analyse(
        samples,
        &Options {
            metrics: vec![MetricId::Lean],
            ..Default::default()
        },
    )
    .unwrap();
    a.warnings.into_iter().find(|w| w.starts_with("lean #"))
}

#[test]
fn a_lean_the_lateral_load_cannot_support_is_flagged() {
    // 45 degrees of lean measured against 0.58 g -- the load of a 30-degree
    // corner. Lateral g is read against gravity and owes nothing to the mount
    // calibration, so the disagreement means the angle is being measured
    // against the wrong idea of upright. This is the shape of the fault that
    // put a 43-degree corner into the clip index as 49.
    let w = lean_warning(&corners(45.0, 30.0)).expect("a 15-degree disagreement should be flagged");
    assert!(w.contains("45°"), "{w}");
    assert!(w.contains("30°"), "{w}");
    assert!(w.contains("15°"), "{w}");

    // An under-reading is the same fault seen from the other side.
    assert!(lean_warning(&corners(20.0, 35.0)).is_some());
}

#[test]
fn lean_that_the_lateral_load_supports_passes_quietly() {
    // A real corner leans a few degrees further than atan(a_lat / g) -- the
    // contact patch sits inboard of the tyre's centreline -- so agreement
    // means close, not exact.
    assert_eq!(lean_warning(&corners(40.0, 36.0)), None);
    assert_eq!(lean_warning(&corners(40.0, 40.0)), None);

    // Nothing to check against is not a finding: a GPX track carries lean but
    // no lateral acceleration, and must not be accused on that account.
    let mut blind = corners(45.0, 30.0);
    for s in &mut blind {
        s.accel_lat_g = None;
    }
    assert_eq!(lean_warning(&blind), None);
}

#[test]
fn a_clip_asked_for_by_time_is_clamped_to_the_video() {
    use gmax::moments::clips::{self, PlanOptions};

    let opts = PlanOptions {
        duration_s: Some(1537.5),
        ..Default::default()
    };

    let c = clips::manual(300.0, Some(8.5), &opts).expect("a clip well inside the video");
    assert_eq!(c.start_s, 300.0);
    assert_eq!(c.end_s, 308.5);
    assert!(c.moments.is_empty(), "nothing was ranked, so nothing chose it");

    // A duration running off the end is shortened to the end rather than
    // handed to ffmpeg to fail on.
    let c = clips::manual(1500.0, Some(600.0), &opts).expect("a clip overrunning the end");
    assert_eq!(c.end_s, 1537.5);

    // No duration means the rest of the video.
    let c = clips::manual(1000.0, None, &opts).expect("a clip to the end");
    assert_eq!(c.end_s, 1537.5);

    // And a start past the end is refused, naming both times.
    let err = clips::manual(2000.0, Some(5.0), &opts).unwrap_err().to_string();
    assert!(err.contains("2000") && err.contains("1537"), "error was: {err}");

    // As is a window too short to be a clip, or a negative one.
    assert!(clips::manual(10.0, Some(0.2), &opts).is_err());
    assert!(clips::manual(10.0, Some(-5.0), &opts).is_err());
    assert!(clips::manual(-1.0, Some(5.0), &opts).is_err());
}

#[test]
fn a_hand_cut_clip_is_named_by_its_window() {
    use gmax::moments::clips::{self, PlanOptions};

    let opts = PlanOptions {
        duration_s: Some(600.0),
        ..Default::default()
    };
    // No moment to name it after, so the window does. The fractional second
    // becomes a `p`, because a second dot in a filename reads as an extension.
    let c = clips::manual(300.0, Some(8.5), &opts).unwrap();
    assert_eq!(c.name("GS010527"), "GS010527_cut_t0300_8p5s");
    let c = clips::manual(12.0, Some(30.0), &opts).unwrap();
    assert_eq!(c.name("GS010527"), "GS010527_cut_t0012_30s");
}

#[test]
fn a_curvy_window_is_clipped_from_its_start_not_from_its_hardest_corner() {
    use gmax::moments::clips::{self, PlanOptions};
    // Three corners in ten seconds, the hardest last: 20, 25 and 40 degrees,
    // peaking at 100, 104 and 108 s. The window is 100-110 s, so its clip must
    // run from the run-in before 100 s to the run-out after 110 s. Anchoring it
    // on the hardest corner cut 102-122 s instead: most of the corners the
    // window was chosen for, missed.
    let t0 = 1_788_601_214.0f64;
    let peaks = [(100.0, 20.0), (104.0, -25.0), (108.0, 40.0)];
    let samples: Vec<Sample> = (0..2000)
        .map(|i| {
            let t = i as f64 / 10.0;
            let lean = peaks
                .iter()
                .map(|&(at, peak)| peak * (-((t - at) / 0.8).powi(2)).exp())
                .sum::<f64>();
            Sample {
                t: t0 + t,
                lean_deg: Some(lean),
                video_time_s: Some(t),
                ..Default::default()
            }
        })
        .collect();
    let a = moments::analyse(
        &samples,
        &Options {
            metrics: vec![MetricId::Curves],
            top: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let entry = &a.metrics[0].entries[0];
    assert_eq!(entry.curve_count, Some(3));
    assert_eq!(entry.video_time_s, Some(100.0), "the window starts at its first corner");
    let peak = entry.peak_lean_deg.unwrap();
    assert!((peak - 40.0).abs() < 1e-6, "the hardest corner is kept for the label: {peak}");

    let opts = PlanOptions {
        duration_s: Some(200.0),
        ..Default::default()
    };
    let (clips, _) = clips::plan(&a, &opts);
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].start_s, 100.0 - opts.pre_s);
    assert_eq!(clips[0].end_s, 110.0 + opts.post_s);
}

#[test]
fn a_telemetry_csv_from_a_cut_clip_gives_video_times_into_the_clip() {
    // `gmax telemetry` on a stream-copied clip writes the GPMF's own
    // timestamps, which start wherever the clip was cut from. A moment's video
    // time is a position in *this* file, so it is measured from the first row.
    let csv = "\
time_s,utc,lat,lon,speed_kmh,lean_deg
1098.060,2026-09-08T11:25:05.100Z,44.1,11.3,60,5
1098.160,2026-09-08T11:25:05.200Z,44.1,11.3,60,10
1098.260,2026-09-08T11:25:05.300Z,,,,20
";
    let s = moments::samples_from_csv(csv).unwrap();
    let v: Vec<f64> = s.iter().map(|s| s.video_time_s.unwrap()).collect();
    assert!((v[0]).abs() < 1e-9 && (v[2] - 0.2).abs() < 1e-9, "video times {v:?}");
    // The clock is untouched.
    assert!((s[0].t - 1_788_866_705.1).abs() < 1e-3, "t {}", s[0].t);
}

#[test]
fn a_telemetry_csv_with_no_clock_is_timed_from_its_first_row() {
    // A recording with no GPS time writes blank utc cells. Every row used to be
    // dropped for it, and with them the lean the file still carried.
    let csv = "\
time_s,utc,lat,lon,speed_kmh,lean_deg
0.060,,,,,5
0.160,,,,,30
0.260,,,,,10
";
    let s = moments::samples_from_csv(csv).unwrap();
    assert_eq!(s.len(), 3);
    assert!((s[1].t - 0.1).abs() < 1e-9);
    let a = moments::analyse(&s, &Options::default()).unwrap();
    assert!(!a.has_absolute_clock());
}

#[test]
fn chapters_are_shifted_in_their_own_timebase() {
    // One chapter at 1/90000 and one with no TIMEBASE at all, which ffmpeg
    // reads as milliseconds.
    let text = ";FFMETADATA1\n\n\
        [CHAPTER]\nTIMEBASE=1/90000\nSTART=900000\nEND=1800000\ntitle=ten to twenty\n\n\
        [CHAPTER]\nSTART=30000\nEND=40000\ntitle=thirty to forty\n";
    // The segment runs 15..35 s.
    let out = gmax::moments::clips::shift_ffmetadata(text, 15.0, Some(20.0));
    assert_eq!(out.matches("[CHAPTER]").count(), 2, "got:\n{out}");
    // 10..20 s trimmed to 15..20 s, rebased to 0..5 s, still at 1/90000.
    assert!(out.contains("TIMEBASE=1/90000\nSTART=0\nEND=450000\n"), "got:\n{out}");
    // 30..40 s trimmed to 30..35 s, rebased to 15..20 s, in milliseconds.
    assert!(out.contains("START=15000\nEND=20000\n"), "got:\n{out}");
}
