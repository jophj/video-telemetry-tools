//! `gmax merge`, and `clips --gps`, with the phone's GPX of the same ride.
//!
//! Assertions stay on what a merge must do — fill the camera's gap with the
//! phone's positions and say how it aligned them — rather than on its
//! internals (speeds, DOP and video times on substituted points), which are
//! still being refined.

use std::path::PathBuf;

use crate::common::sections::section_or_skip;
use crate::common::{fixture, which, REFERENCE_PHONE_GPX};
use crate::support::{
    argv, epoch, gmax_ok, read_gpx, section_csv, validate_gpx, Csv, Scratch, PHONE_TIME_OFFSET_S,
};

fn phone_or_skip() -> Option<PathBuf> {
    let p = fixture(REFERENCE_PHONE_GPX);
    if p.is_none() {
        eprintln!("SKIP: fixture {REFERENCE_PHONE_GPX} is not present on this machine");
    }
    p
}

/// The UTC span, in epoch seconds, of the longest run of samples without a
/// usable fix in a section's CSV.
fn longest_gap(csv: &Csv) -> (f64, f64) {
    let mut best = (0.0, 0.0);
    let mut start: Option<f64> = None;
    for i in 0..csv.len() {
        let t = crate::support::epoch(csv.str(i, "utc"));
        let valid = csv.str(i, "gps_valid") == "1";
        match (valid, start) {
            (false, None) => start = Some(t),
            (true, Some(s)) => {
                if t - s > best.1 - best.0 {
                    best = (s, t);
                }
                start = None;
            }
            _ => {}
        }
    }
    best
}

#[test]
fn the_phone_fills_the_cameras_gps_dropout() {
    let video = section_or_skip!("gps_dropout");
    let Some(phone) = phone_or_skip() else { return };
    let Some(csv_path) = section_csv("gps_dropout") else {
        return;
    };
    let csv = Csv::read(&csv_path);
    let (gap_start, gap_end) = longest_gap(&csv);
    // Lost from 304.9 s to 313.3 s on the ride.
    assert!(
        gap_end - gap_start > 7.0,
        "the dropout is only {} s",
        gap_end - gap_start
    );

    let dir = Scratch::new("merge");
    let out = dir.join("merged.gpx");
    let run = gmax_ok(argv![
        "merge",
        &video,
        "--with",
        &phone,
        "-o",
        &out,
        "--time-offset",
        PHONE_TIME_OFFSET_S,
        "--no-gps-lag",
    ]);
    // The alignment is always reported, and here it is the one given.
    assert!(
        run.stdout.contains("alignment: given on the command line"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("+1.600 s"), "{}", run.stdout);

    let gpx = read_gpx(&out);
    assert!(!gpx.points.is_empty());
    let times: Vec<f64> = gpx
        .points
        .iter()
        .map(|p| epoch(p.time.as_deref().unwrap()))
        .collect();
    for w in times.windows(2) {
        assert!(w[1] > w[0], "merged points go backwards: {w:?}");
    }
    // Inside the gap the camera had nothing; the phone has a position for it.
    let filled = gpx
        .points
        .iter()
        .zip(&times)
        .filter(|(_, &t)| t > gap_start + 0.5 && t < gap_end - 0.5)
        .filter(|(p, _)| p.source.as_deref() == Some("external"))
        .count();
    assert!(
        filled >= 50,
        "only {filled} substituted points inside the dropout"
    );
    // Every point says where its position came from.
    for p in &gpx.points {
        let s = p.source.as_deref().unwrap_or("");
        assert!(
            ["gopro", "external", "interpolated"].contains(&s),
            "source {s:?}"
        );
    }
    // The run is auditable from the file alone.
    for key in ["merge_method", "merge_offset_s", "merge_points_external"] {
        assert!(gpx.metadata.contains_key(key), "no {key} in the metadata");
    }
    assert!(
        gpx.metadata["merge_points_external"]
            .parse::<usize>()
            .unwrap()
            >= filled
    );
    validate_gpx(&out);
}

#[test]
fn a_section_too_short_to_align_says_so() {
    // Fifteen seconds hold about fifteen phone points, against the hundred
    // pairs a lean alignment needs. The merge must report that it could not
    // align rather than present an alignment it did not measure.
    let video = section_or_skip!("gps_dropout");
    let Some(phone) = phone_or_skip() else { return };
    let dir = Scratch::new("merge-auto");
    let out = dir.join("merged.gpx");
    let run = gmax_ok(argv!["merge", &video, "--with", &phone, "-o", &out]);
    assert!(run.stdout.contains("alignment:"), "{}", run.stdout);
    let gpx = read_gpx(&out);
    assert_eq!(
        gpx.metadata
            .get("merge_alignment_reliable")
            .map(String::as_str),
        Some("false"),
        "an alignment from fifteen seconds was reported reliable"
    );
    assert!(!run.stderr.is_empty(), "no warning about the alignment");
    validate_gpx(&out);
}

#[test]
fn clips_with_gps_writes_the_repaired_positions_beside_the_clip() {
    let video = section_or_skip!("gps_dropout");
    let Some(phone) = phone_or_skip() else { return };
    if which("ffmpeg").is_none() {
        eprintln!("SKIP: ffmpeg is not installed");
        return;
    }
    let dir = Scratch::new("merge-clips");
    let out = dir.join("out");
    gmax_ok(argv![
        "--quiet", "clips", &video, "--gps", &phone, "--top", "1", "--metric", "lean", "-o", &out
    ]);
    let sibling = std::fs::read_dir(&out)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with(".telemetry.csv"))
        .expect("no sibling telemetry");
    let csv = Csv::read(&sibling);
    let external = (0..csv.len())
        .filter(|&i| csv.str(i, "gps_source") == "external" && csv.str(i, "gps_valid") == "1")
        .count();
    assert!(external > 0, "no repaired positions beside the clip");
    // A repaired row has a position.
    for i in 0..csv.len() {
        if csv.str(i, "gps_source") == "external" && csv.str(i, "gps_valid") == "1" {
            assert!(!csv.str(i, "lat").is_empty() && !csv.str(i, "lon").is_empty());
        }
    }
}
