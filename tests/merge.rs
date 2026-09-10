//! Merging an external GPS source into a GoPro track.

mod common;
use common::fixture_or_skip;

use chrono::{DateTime, Duration, Utc};
use gmax::gpx::{Gpx, TrackPoint};
use gmax::merge::{self, Lag, Options, Prefer};

fn t0() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-08T11:00:00Z")
        .unwrap()
        .into()
}

/// Lean angle for the synthetic ride.
///
/// Deliberately aperiodic: three sine components at incommensurate periods.
/// A single sine would be a poor test fixture, because correlating it against
/// itself gives an equally good match at every multiple of its period, so the
/// recovered offset would be ambiguous by construction. Real lean is
/// aperiodic, which is why alignment on real footage finds one sharp peak.
fn synthetic_lean(t: f64) -> f64 {
    use std::f64::consts::TAU;
    35.0 * (t * TAU / 8.0).sin()
        + 18.0 * (t * TAU / 3.7 + 1.1).sin()
        + 9.0 * (t * TAU / 1.31 + 0.4).sin()
}

/// A synthetic ride: a straight run north with an aperiodic lean signal, so
/// alignment has something unambiguous to lock onto.
fn synthetic(n: usize, hz: f64, lean_phase: f64, start_s: f64) -> Gpx {
    let mut g = Gpx::default();
    for i in 0..n {
        let t = start_s + i as f64 / hz;
        g.points.push(TrackPoint {
            lat: 44.0 + t * 1e-4,
            lon: 11.0,
            ele_m: Some(500.0 + t),
            time: Some(t0() + Duration::microseconds((t * 1e6) as i64)),
            speed_ms: Some(11.12),
            course_deg: Some(0.0),
            lean_deg: Some(synthetic_lean(t + lean_phase)),
            ..Default::default()
        });
    }
    g
}

#[test]
fn recovers_a_known_clock_offset_from_lean_angle() {
    // Simulate an external device whose clock runs three seconds slow: at its
    // timestamp t it reports the lean that really happened at t + 3. The offset
    // that puts it back on the base clock is therefore +3.
    let base = synthetic(600, 10.0, 0.0, 0.0);
    let external = synthetic(120, 2.0, 3.0, 0.0);

    let slots = merge::slots_from_gpx(&base);
    let (_, report) = merge::merge(
        &slots,
        &external,
        &Options {
            // No positional fit to derive a lag from in this synthetic case.
            lag: Lag::None,
            ..Default::default()
        },
    );
    assert!(report.method.starts_with("lean"), "method was {}", report.method);
    assert!(
        (report.offset_s - 3.0).abs() < 0.2,
        "offset was {:+.3}, expected +3",
        report.offset_s
    );
    assert!(report.score > 0.9, "|r| was {}", report.score);
    // Same polarity on both sides.
    assert_eq!(report.lean_polarity_agrees, Some(true));
}

#[test]
fn detects_when_the_external_lean_is_signed_the_other_way() {
    let base = synthetic(600, 10.0, 0.0, 0.0);
    let mut external = synthetic(120, 2.0, 0.0, 0.0);
    for p in &mut external.points {
        p.lean_deg = p.lean_deg.map(|l| -l);
    }
    let slots = merge::slots_from_gpx(&base);
    let (_, report) = merge::merge(&slots, &external, &Options { lag: Lag::None, ..Default::default() });
    // Alignment still works -- it is sign-agnostic -- but the disagreement is
    // reported so the two lean channels are not naively mixed.
    assert!((report.offset_s).abs() < 0.2, "offset {:+.3}", report.offset_s);
    assert_eq!(report.lean_polarity_agrees, Some(false));
    assert!(report.summary().contains("signed opposite"));
}

#[test]
fn never_invents_a_position_outside_the_external_track() {
    // A base with a gap in the middle, and an external source that covers only
    // the first half.
    let mut base = synthetic(600, 10.0, 0.0, 0.0);
    base.points.retain(|p| {
        let t = (p.time.unwrap() - t0()).num_milliseconds() as f64 / 1000.0;
        !(20.0..40.0).contains(&t)
    });
    let external = synthetic(60, 2.0, 0.0, 0.0); // 0..30 s only

    let slots = merge::slots_from_gpx(&base);
    let (merged, report) = merge::merge(
        &slots,
        &external,
        &Options { align: merge::Align::None, lag: Lag::None, ..Default::default() },
    );

    // The 20-30 s part of the gap can be filled; 30-40 s cannot.
    assert!(report.points_external > 0, "nothing was substituted");
    assert!(
        report.points_unpositioned > 0,
        "the part of the gap beyond the external track should stay empty"
    );
    let ext_end = external.points.last().unwrap().time.unwrap();
    for p in merged.points.iter().filter(|p| p.source.as_deref() == Some("external")) {
        assert!(
            p.time.unwrap() <= ext_end + Duration::seconds(1),
            "substituted a point at {} beyond the external track's end {}",
            p.time.unwrap(),
            ext_end
        );
    }
}

#[test]
fn camera_imu_channels_survive_a_substitution() {
    // Lean comes from the camera's IMU and is unaffected by GPS quality, so a
    // substituted position must keep the camera's own lean rather than the
    // external source's.
    let mut base = synthetic(600, 10.0, 0.0, 0.0);
    // Blank the positions in the middle while keeping the lean.
    for p in &mut base.points {
        let t = (p.time.unwrap() - t0()).num_milliseconds() as f64 / 1000.0;
        if (20.0..30.0).contains(&t) {
            p.lat = 0.0;
            p.lon = 0.0;
        }
    }
    let mut slots = merge::slots_from_gpx(&base);
    // Mark the blanked span unusable, as the quality gate would.
    for s in &mut slots {
        if s.fix.as_ref().map(|f| f.lat == 0.0).unwrap_or(false) {
            s.fix = None;
        }
    }
    // Give the external source a distinctly different lean.
    let mut external = synthetic(120, 2.0, 0.0, 0.0);
    for p in &mut external.points {
        p.lean_deg = Some(99.0);
    }

    let (merged, report) = merge::merge(
        &slots,
        &external,
        &Options { align: merge::Align::None, lag: Lag::None, ..Default::default() },
    );
    assert!(report.points_external > 0);
    for p in merged.points.iter().filter(|p| p.source.as_deref() == Some("external")) {
        assert_ne!(
            p.lean_deg,
            Some(99.0),
            "a substituted point took the external source's lean instead of the camera's"
        );
    }
}

#[test]
fn a_fixed_offset_bypasses_the_search() {
    let base = synthetic(600, 10.0, 0.0, 0.0);
    let external = synthetic(120, 2.0, 0.0, 0.0);
    let slots = merge::slots_from_gpx(&base);
    let (_, report) = merge::merge(
        &slots,
        &external,
        &Options {
            align: merge::Align::Fixed(-7.25),
            lag: Lag::None,
            ..Default::default()
        },
    );
    assert_eq!(report.offset_s, -7.25);
    assert!(report.method.contains("command line"));
}

#[test]
fn prefer_gopro_keeps_every_camera_position() {
    let base = synthetic(600, 10.0, 0.0, 0.0);
    let external = synthetic(120, 2.0, 0.0, 0.0);
    let slots = merge::slots_from_gpx(&base);
    let (merged, report) = merge::merge(
        &slots,
        &external,
        &Options {
            prefer: Prefer::Gopro,
            align: merge::Align::None,
            lag: Lag::None,
            ..Default::default()
        },
    );
    assert_eq!(report.points_external, 0);
    assert!(merged
        .points
        .iter()
        .all(|p| p.source.as_deref() == Some("gopro")));
}

// ---------------------------------------------------------- real material

#[test]
fn repairs_the_reference_clip_from_the_phone_track() {
    let base_path = fixture_or_skip!(common::REFERENCE_360);
    let ext_path = fixture_or_skip!(common::EXTERNAL_GPX);

    let mut f = gmax::mp4::Mp4::open(&base_path).unwrap();
    let g = gmax::gpmf::from_mp4(&mut f).unwrap();
    let t = gmax::telemetry::compute(&g, &Default::default()).unwrap();
    let slots = merge::slots_from_telemetry(&t);
    assert_eq!(slots.len(), 15375, "one slot per GPS sample, good or bad");

    // ride.gpx is a different ride from the reference clip, so it will not
    // align; this only exercises the path on real data volumes.
    let external = gmax::gpx::read_file(&ext_path).unwrap();
    let (merged, report) = merge::merge(&slots, &external, &Options::default());

    // Every slot is accounted for exactly once.
    assert_eq!(
        report.points_gopro + report.points_external + report.points_unpositioned,
        slots.len(),
        "points must balance against the base timeline"
    );
    assert_eq!(merged.points.len(), report.points_total);
    // The camera's own good fixes are never lost.
    assert!(report.points_gopro + report.points_external >= t.valid_fix_count());
    // Positional noise is measured for both sources on real data.
    assert!(report.camera_dispersion.is_some());
}

#[test]
fn measures_the_cameras_gps_latency_on_real_material() {
    let base_path = fixture_or_skip!(common::REFERENCE_360);
    // Needs the matching phone track, which the caller must have generated.
    let ext_path = std::path::Path::new("/tmp/phone.gpx");
    if !ext_path.exists() {
        eprintln!("SKIP: /tmp/phone.gpx not present (generate with motion-analyse --gpx)");
        return;
    }

    let mut f = gmax::mp4::Mp4::open(&base_path).unwrap();
    let g = gmax::gpmf::from_mp4(&mut f).unwrap();
    let t = gmax::telemetry::compute(&g, &Default::default()).unwrap();
    let slots = merge::slots_from_telemetry(&t);
    let external = gmax::gpx::read_file(ext_path).unwrap();

    let (_, report) = merge::merge(&slots, &external, &Options::default());

    // Alignment locks onto lean with a strong, correctly-signed correlation.
    assert!(report.method.starts_with("lean"));
    assert!(report.score > 0.85, "|r| was {}", report.score);
    assert_eq!(
        report.lean_polarity_agrees,
        Some(true),
        "the phone's lean should now agree in polarity with ours, which is an \
         independent confirmation of the lean sign fix"
    );
    assert!(
        (report.offset_s - 1.6).abs() < 0.3,
        "clock offset was {:+.2} s, expected about +1.6",
        report.offset_s
    );

    // And the camera's positions are measurably stale relative to them.
    let lag = report.gps_lag_s.expect("a lag should be measurable");
    assert!(
        (2.0..6.0).contains(&lag),
        "GPS lag was {lag:+.2} s, expected about +4"
    );
    assert!(report.gps_lag_applied);

    // The phone is the cleaner receiver by an order of magnitude, so the
    // quality gate should hand it the overlap.
    let (c, e) = (
        report.camera_dispersion.unwrap(),
        report.external_dispersion.unwrap(),
    );
    assert!(e * 5.0 < c, "dispersions were camera {c:.3}, external {e:.3}");
    assert!(report.external_preferred_on_quality);
    assert!(report.points_external > report.points_gopro);
}
