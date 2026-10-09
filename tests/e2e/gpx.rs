//! `gmax gpx` on sections, read back independently.

use crate::common::sections::section_or_skip;
use crate::support::{argv, epoch, gmax_ok, read_gpx, section_csv, validate_gpx, Csv, Scratch};

const TPX_V2: &str = "http://www.garmin.com/xmlschemas/TrackPointExtension/v2";

#[test]
fn a_section_exports_a_valid_gpx_with_one_point_per_usable_fix() {
    let video = section_or_skip!("mount_step");
    let dir = Scratch::new("gpx");
    let out = dir.join("mount_step.gpx");
    let run = gmax_ok(argv!["gpx", &video, "-o", &out]);
    let gpx = read_gpx(&out);

    // Speed and course live in the Garmin extension, v2 (v1 has no speed).
    assert!(
        gpx.text.contains(TPX_V2),
        "TrackPointExtension v2 namespace missing"
    );
    assert!(
        !gpx.text.contains("TrackPointExtension/v1"),
        "v1 namespace declared"
    );
    // Every fix is usable on this section: 10 Hz for 11 s.
    let n = gpx.points.len();
    assert!(n > 100 && n <= 112, "{n} track points");
    assert!(
        run.stdout.contains(&format!("({n} track points)")),
        "{}",
        run.stdout
    );

    // Strictly increasing times, inside the section's own span.
    let Some(csv_path) = section_csv("mount_step") else {
        return;
    };
    let csv = Csv::read(&csv_path);
    let start = epoch(csv.str(0, "utc"));
    let end = epoch(csv.str(csv.len() - 1, "utc"));
    let times: Vec<f64> = gpx
        .points
        .iter()
        .map(|p| epoch(p.time.as_deref().unwrap()))
        .collect();
    for w in times.windows(2) {
        assert!(w[1] > w[0], "track point times go backwards: {w:?}");
    }
    assert!(times[0] >= start - 1.1 && times[n - 1] <= end + 0.01);

    for p in &gpx.points {
        assert!(p.speed.is_some(), "a point with no speed");
        assert!(p.lean.is_some(), "a point with no lean");
        // Positions in the video, from its start, not the ride's.
        let vt = p.video_time.expect("a point with no video time");
        assert!((-0.5..12.0).contains(&vt), "video time {vt}");
        assert_eq!(p.source.as_deref(), Some("gopro"));
        // Somewhere near Florence, which is where this ride was.
        assert!((43.8..44.3).contains(&p.lat) && (11.0..11.5).contains(&p.lon));
    }
    validate_gpx(&out);
}

#[test]
fn the_gpx_of_gps_acquisition_starts_at_lock() {
    let video = section_or_skip!("gps_acquisition");
    let dir = Scratch::new("gpx-acq");
    let out = dir.join("acq.gpx");
    gmax_ok(argv!["--quiet", "gpx", &video, "-o", &out]);
    let gpx = read_gpx(&out);
    assert!(!gpx.points.is_empty(), "no track points after lock");
    // No point from the thirteen seconds without a lock.
    for p in &gpx.points {
        let vt = p.video_time.unwrap();
        assert!(
            vt > 12.5,
            "a track point at {vt} s, before the receiver locked"
        );
    }
    validate_gpx(&out);
}

#[test]
fn a_plain_gpx_without_extensions_still_validates() {
    let video = section_or_skip!("tail");
    let dir = Scratch::new("gpx-plain");
    let out = dir.join("tail.gpx");
    gmax_ok(argv![
        "--quiet",
        "gpx",
        &video,
        "-o",
        &out,
        "--no-extensions"
    ]);
    let gpx = read_gpx(&out);
    assert!(!gpx.points.is_empty());
    assert!(gpx
        .points
        .iter()
        .all(|p| p.lean.is_none() && p.speed.is_none()));
    validate_gpx(&out);
}
