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

fn at(t: f64) -> DateTime<Utc> {
    t0() + Duration::microseconds((t * 1e6).round() as i64)
}

fn seconds(p: &TrackPoint) -> f64 {
    (p.time.unwrap() - t0()).num_microseconds().unwrap() as f64 / 1e6
}

/// Merge with the clocks taken as they are and no lag correction, so a test
/// sees substitution alone.
fn substitute_only() -> Options {
    Options { align: merge::Align::None, lag: Lag::None, ..Default::default() }
}

#[test]
fn a_sparse_source_fills_a_gap_despite_a_one_second_max_dt() {
    // A 0.5 Hz source has neighbours two seconds apart. Requiring both within
    // max_dt of the instant refused every slot more than a second from one of
    // them, which for a 1 Hz source with jitter is most of them.
    let mut base = synthetic(600, 10.0, 0.0, 0.0);
    base.points.retain(|p| !(20.0..30.0).contains(&seconds(p)));
    let external = synthetic(30, 0.5, 0.0, 0.0);

    let slots = merge::slots_from_gpx(&base);
    let (merged, report) = merge::merge(&slots, &external, &substitute_only());
    assert_eq!(report.points_unpositioned, 0, "{}", report.summary());
    // And what fills the gap is an interpolation, not a held sample: the
    // synthetic ride runs due north at a known rate.
    for p in merged.points.iter().filter(|p| p.source.as_deref() == Some("external")) {
        let want = 44.0 + seconds(p) * 1e-4;
        assert!((p.lat - want).abs() < 1e-7, "lat {} at {:.1} s", p.lat, seconds(p));
    }
}

#[test]
fn across_a_long_gap_only_the_nearest_point_within_max_dt_is_used() {
    // The external track has a ten-second hole: too long to interpolate
    // across, so a slot there takes the nearest external point only when it
    // is within max_dt, and otherwise stays unpositioned.
    let mut base = synthetic(600, 10.0, 0.0, 0.0);
    base.points.retain(|p| !(20.0..40.0).contains(&seconds(p)));
    let mut external = synthetic(120, 2.0, 0.0, 0.0);
    external.points.retain(|p| !(25.1..34.9).contains(&seconds(p)));

    let slots = merge::slots_from_gpx(&base);
    let (merged, report) = merge::merge(&slots, &external, &substitute_only());
    assert!(report.points_unpositioned > 0);
    for p in &merged.points {
        let t = seconds(p);
        assert!(
            !(26.05..33.95).contains(&t),
            "a slot at {t:.1} s was positioned from a sample more than max_dt away"
        );
        if (25.0..26.0).contains(&t) {
            assert!((p.lat - (44.0 + 25.0 * 1e-4)).abs() < 1e-9, "held the wrong sample");
        }
    }
}

#[test]
fn interpolation_never_crosses_a_segment_boundary() {
    // Two external samples one second apart would normally be interpolated
    // between, but a segment break says the receiver stopped logging there.
    let mut external = Gpx::default();
    for (i, t) in [8.0, 9.0, 10.0, 11.0, 12.0, 13.0].into_iter().enumerate() {
        external.points.push(TrackPoint {
            lat: 44.0 + t * 1e-4,
            lon: 11.0,
            time: Some(at(t)),
            segment: usize::from(i >= 3),
            ..Default::default()
        });
    }
    let slots: Vec<merge::Slot> = [10.3, 10.7]
        .into_iter()
        .map(|t| merge::Slot {
            time: at(t),
            video_time_s: None,
            fix: None,
            lean_deg: None,
            accel_ms2: None,
        })
        .collect();
    let (merged, _) = merge::merge(&slots, &external, &substitute_only());
    let lats: Vec<f64> = merged.points.iter().map(|p| p.lat).collect();
    assert_eq!(lats, vec![44.0 + 10.0 * 1e-4, 44.0 + 11.0 * 1e-4]);
}

#[test]
fn external_lean_filling_a_gap_arrives_in_our_sign_convention() {
    // Where the camera has no lean of its own the external source's is used,
    // and when the two disagree in polarity it must be negated first, or the
    // repaired stretch leans the wrong way.
    let mut base = synthetic(600, 10.0, 0.0, 0.0);
    base.points.retain(|p| !(20.0..30.0).contains(&seconds(p)));
    let mut external = synthetic(120, 2.0, 0.0, 0.0);
    for p in &mut external.points {
        p.lean_deg = p.lean_deg.map(|l| -l);
    }
    let slots = merge::slots_from_gpx(&base);

    for align in [merge::Align::Auto, merge::Align::None] {
        let (merged, report) =
            merge::merge(&slots, &external, &Options { align, lag: Lag::None, ..Default::default() });
        assert_eq!(report.lean_polarity_agrees, Some(false));
        let filled: Vec<&TrackPoint> = merged
            .points
            .iter()
            .filter(|p| (20.5..29.5).contains(&seconds(p)))
            .collect();
        assert!(filled.len() > 50);
        let ours: Vec<f64> = filled.iter().map(|p| synthetic_lean(seconds(p))).collect();
        let got: Vec<f64> = filled.iter().map(|p| p.lean_deg.unwrap()).collect();
        let r = gmax::math::pearson(&ours, &got);
        assert!(r > 0.5, "filled lean correlates at r = {r:.2} with ours ({align:?})");
    }
}

#[test]
fn a_lean_match_at_the_edge_of_the_search_is_flagged() {
    // The true offset is +3 s and the search only reaches +2.5 s, so the best
    // it can find sits on the edge — which is the search running out, not an
    // answer.
    let base = synthetic(600, 10.0, 0.0, 0.0);
    let external = synthetic(120, 2.0, 3.0, 0.0);
    let slots = merge::slots_from_gpx(&base);
    let (_, report) = merge::merge(&slots, &external, &Options { span: 2.5, ..Default::default() });
    assert!((report.offset_s - 2.5).abs() < 1e-6, "offset {:+.2}", report.offset_s);
    assert!(!report.alignment_reliable);
    assert!(
        report.warnings.iter().any(|w| w.contains("edge")),
        "{:?}",
        report.warnings
    );
    // A lag measured against an alignment in doubt is not applied.
    assert!(!report.gps_lag_applied);
}

#[test]
fn a_weak_lean_match_is_flagged() {
    // Lean that has nothing to do with ours: some offset always scores best,
    // but a best of |r| = 0.3 is no alignment at all.
    let base = synthetic(600, 10.0, 0.0, 0.0);
    let mut external = synthetic(120, 2.0, 0.0, 0.0);
    let mut state = 0x2545_f491_4f6c_dd1du64;
    for p in &mut external.points {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        p.lean_deg = Some((state % 8000) as f64 / 100.0 - 40.0);
    }
    let slots = merge::slots_from_gpx(&base);
    let (_, report) = merge::merge(&slots, &external, &Options { lag: Lag::None, ..Default::default() });
    assert!(report.score < 0.5, "|r| was {}", report.score);
    assert!(!report.alignment_reliable);
    assert!(report.warnings.iter().any(|w| w.contains("weak")), "{:?}", report.warnings);
}

#[test]
fn the_gps_lag_is_searched_for_around_the_lean_offset() {
    // Clock offset +10 s, camera positions four seconds stale: the positional
    // optimum is at +14 s. Searched either side of zero with a 12 s span, it
    // was clipped to +12 and the lag to 2 s.
    let lat = |t: f64| 44.0 + t * 1e-4;
    let mut base = Gpx::default();
    for i in 0..1200 {
        let t = i as f64 / 10.0;
        base.points.push(TrackPoint {
            lat: lat(t - 4.0),
            lon: 11.0,
            time: Some(at(t)),
            speed_ms: Some(11.12),
            lean_deg: Some(synthetic_lean(t)),
            ..Default::default()
        });
    }
    let mut external = Gpx::default();
    for i in 0..240 {
        // Jittered, as a phone's timestamps are; on an exact grid the
        // positional fit has a flat bottom a quarter of a second wide.
        let stamp = -10.0 + i as f64 / 2.0 + 0.2 * (i as f64 * 1.7).sin();
        let truth = stamp + 10.0;
        external.points.push(TrackPoint {
            lat: lat(truth),
            lon: 11.0,
            time: Some(at(stamp)),
            speed_ms: Some(11.12),
            lean_deg: Some(synthetic_lean(truth)),
            ..Default::default()
        });
    }
    let slots = merge::slots_from_gpx(&base);
    let (_, report) = merge::merge(&slots, &external, &Options { span: 12.0, ..Default::default() });
    assert!((report.offset_s - 10.0).abs() < 0.2, "offset {:+.2}", report.offset_s);
    let lag = report.gps_lag_s.expect("lag measured");
    assert!((lag - 4.0).abs() < 0.3, "lag {lag:+.2}");
    assert!(report.gps_lag_applied);
    assert!(report.alignment_reliable, "{:?}", report.warnings);
}

#[test]
fn the_lag_warning_quotes_this_rides_own_speed() {
    let base = synthetic(600, 10.0, 0.0, 0.0);
    let external = synthetic(120, 2.0, 0.0, 0.0);
    let slots = merge::slots_from_gpx(&base);
    let (_, report) = merge::merge(
        &slots,
        &external,
        &Options { align: merge::Align::None, lag: Lag::Fixed(4.0), ..Default::default() },
    );
    // 11.12 m/s, not the reference ride's 16.2.
    let w = report.warnings.iter().find(|w| w.contains("re-timed")).expect("a lag warning");
    assert!(w.contains("40 km/h") && w.contains("about 44 m"), "{w}");
}

#[test]
fn bridging_the_cameras_own_rejected_fix_is_labelled_as_interpolation() {
    let base = synthetic(600, 10.0, 0.0, 0.0);
    let mut slots = merge::slots_from_gpx(&base);
    slots[300].fix = None; // rejected by the quality gate
    let rejected_at = slots[300].time;

    // With no external coverage there, the camera's own neighbours are the
    // only way to place it, and the result says so.
    let external = synthetic(10, 2.0, 0.0, 0.0);
    let (merged, report) = merge::merge(&slots, &external, &substitute_only());
    let p = merged.points.iter().find(|p| p.time == Some(rejected_at)).unwrap();
    assert_eq!(p.source.as_deref(), Some("interpolated"));
    assert_eq!(report.points_interpolated, 1);

    // With external coverage, a real measurement beats bridging the gap.
    let external = synthetic(120, 2.0, 0.0, 0.0);
    let (merged, _) = merge::merge(&slots, &external, &substitute_only());
    let p = merged.points.iter().find(|p| p.time == Some(rejected_at)).unwrap();
    assert_eq!(p.source.as_deref(), Some("external"));
}

#[test]
fn fixes_a_lag_correction_leaves_behind_are_invalidated_not_left_stale() {
    // Re-timing the camera's positions by three seconds leaves its last three
    // seconds of slots with no camera position to take. Where the external
    // track does not reach either, those fixes used to keep their uncorrected
    // positions, tens of metres behind their corrected neighbours.
    let base = synthetic(300, 10.0, 0.0, 0.0);
    let mut slots = merge::slots_from_gpx(&base);
    for (k, s) in slots.iter_mut().enumerate() {
        s.video_time_s = Some(k as f64 / 10.0);
    }
    let mut fixes: Vec<gmax::telemetry::GpsFix> = slots
        .iter()
        .map(|s| {
            let f = s.fix.as_ref().unwrap();
            gmax::telemetry::GpsFix {
                time: s.video_time_s.unwrap(),
                lat: f.lat,
                lon: f.lon,
                alt_m: 0.0,
                speed_2d: 11.12,
                speed_3d: 11.12,
                dop: None,
                fix: None,
                utc: Some(s.time),
                course_deg: None,
                valid: true,
                reject: None,
                source: gmax::telemetry::Source::Gopro,
            }
        })
        .collect();
    let external = synthetic(40, 2.0, 0.0, 0.0); // 0..19.5 s
    let (merged, report) = merge::merge(
        &slots,
        &external,
        &Options { align: merge::Align::None, lag: Lag::Fixed(3.0), prefer: Prefer::Gopro, ..Default::default() },
    );
    assert!(report.points_unpositioned > 0);

    merge::apply_to_fixes(&mut fixes, 0.0, true, &merged);
    let by_time = |vt: f64| merged.points.iter().find(|p| (p.video_time_s.unwrap() - vt).abs() < 1e-6);
    for f in &fixes {
        match by_time(f.time) {
            Some(p) => {
                assert!(f.valid);
                assert_eq!((f.lat, f.lon), (p.lat, p.lon), "fix at {:.1} s not corrected", f.time);
            }
            None => assert!(!f.valid, "fix at {:.1} s kept a stale position", f.time),
        }
    }
    assert!(fixes.iter().any(|f| !f.valid));
}

#[test]
fn a_bogus_timestamp_does_not_fill_a_year_with_slots() {
    // One point a year late made the gap filler create one empty slot per
    // sample interval across the whole year.
    let mut base = synthetic(600, 10.0, 0.0, 0.0);
    let mut late = base.points.last().unwrap().clone();
    late.time = Some(late.time.unwrap() + Duration::days(365));
    base.points.push(late);
    let slots = merge::slots_from_gpx(&base);
    assert_eq!(slots.len(), 601);

    let (_, report) = merge::merge(&slots, &synthetic(120, 2.0, 0.0, 0.0), &substitute_only());
    assert!(
        report.warnings.iter().any(|w| w.contains("left unfilled")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn problems_reading_the_external_file_reach_the_merge_report() {
    let base = synthetic(600, 10.0, 0.0, 0.0);
    let mut external = synthetic(120, 2.0, 0.0, 0.0);
    external.warnings.push("3 point timestamp(s) could not be parsed".into());
    let (_, report) = merge::merge(&merge::slots_from_gpx(&base), &external, &substitute_only());
    assert!(report.warnings.iter().any(|w| w.starts_with("external GPX: 3 point")));
}

#[test]
fn merged_gpx_validates_against_the_official_schema() {
    let xsd = fixture_or_skip!(common::GPX_XSD);
    let Some(xmllint) = common::which("xmllint") else {
        eprintln!("SKIP: xmllint is not installed");
        return;
    };
    let mut base = synthetic(600, 10.0, 0.0, 0.0);
    base.points.retain(|p| !(20.0..30.0).contains(&seconds(p)));
    let slots = merge::slots_from_gpx(&base);
    let (merged, report) = merge::merge(&slots, &synthetic(120, 2.0, 0.0, 0.0), &Options::default());
    assert!(report.points_external > 0);

    let out = std::env::temp_dir().join(format!("gmax-merge-schema-{}.gpx", std::process::id()));
    gmax::gpx::write_file(&merged, &out, &gmax::gpx::WriteOptions::default()).unwrap();
    let status = std::process::Command::new(xmllint)
        .args(["--noout", "--nonet", "--schema"])
        .arg(&xsd)
        .arg(&out)
        .output()
        .expect("run xmllint");
    let _ = std::fs::remove_file(&out);
    assert!(
        status.status.success(),
        "xmllint rejected the merged GPX:\n{}",
        String::from_utf8_lossy(&status.stderr)
    );
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
        report.points_gopro
            + report.points_external
            + report.points_interpolated
            + report.points_unpositioned,
        slots.len(),
        "points must balance against the base timeline"
    );
    assert_eq!(merged.points.len(), report.points_total);
    // The camera's own good fixes are never lost, unless a lag correction
    // re-timed them off the end of the track.
    if !report.gps_lag_applied {
        assert!(report.points_total >= t.valid_fix_count());
    }
    // Positional noise is measured for both sources on real data.
    assert!(report.camera_dispersion.is_some());
}

#[test]
fn measures_the_cameras_gps_latency_on_real_material() {
    let base_path = fixture_or_skip!(common::REFERENCE_360);
    // Needs the matching phone track, which the caller must have generated.
    // The phone track recorded alongside it (generate with motion-analyse --gpx).
    let ext_path = &fixture_or_skip!(common::REFERENCE_PHONE_GPX);

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

#[test]
fn merged_positions_go_back_onto_the_fixes_they_replace() {
    let base_path = fixture_or_skip!(common::REFERENCE_360);
    let ext_path = fixture_or_skip!(common::EXTERNAL_GPX);

    let mut f = gmax::mp4::Mp4::open(&base_path).unwrap();
    let g = gmax::gpmf::from_mp4(&mut f).unwrap();
    let mut t = gmax::telemetry::compute(&g, &Default::default()).unwrap();
    let before_valid = t.valid_fix_count();
    let was_valid: Vec<bool> = t.gps.iter().map(|f| f.valid).collect();
    let before_lean = t.lean_deg.clone();
    let before_alat = t.accel_lat_g.clone();

    let external = gmax::gpx::read_file(&ext_path).unwrap();
    let slots = merge::slots_from_telemetry(&t);
    let (merged, _) = merge::merge(&slots, &external, &Options::default());
    let repaired = merge::apply_to_telemetry(&mut t, &merged);

    // Writing back must not change the shape of the timeline: the fixes are
    // the same fixes, at the same instants, with better positions in them.
    assert_eq!(t.gps.len(), slots.len(), "fix count changed");
    // The only good fixes a merge may lose are ones a lag correction left
    // with no corrected position, and those it must lose rather than keep
    // stale.
    let invalidated = was_valid
        .iter()
        .zip(&t.gps)
        .filter(|(was, now)| **was && !now.valid)
        .count();
    assert_eq!(
        t.valid_fix_count() + invalidated,
        before_valid + repaired,
        "the repaired count must match the fixes that became usable"
    );

    // The point of writing back rather than ranking the merged GPX: the IMU
    // channels are untouched, so the lateral and total-load metrics that have
    // no GPX representation survive the merge.
    assert_eq!(before_lean, t.lean_deg, "lean must not move");
    assert_eq!(before_alat, t.accel_lat_g, "lateral load must not move");

    // Every repaired fix carries a position now, and says where it came from.
    for fix in t.gps.iter().filter(|f| f.source != gmax::telemetry::Source::Gopro) {
        assert!(fix.valid, "a substituted fix must be usable");
        assert!(fix.lat.is_finite() && fix.lon.is_finite());
    }
}
