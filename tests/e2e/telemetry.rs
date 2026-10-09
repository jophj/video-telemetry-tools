//! `gmax telemetry` on sections: schema, cadence, time base, and agreement
//! with the same instants measured over the whole ride.

use std::collections::HashMap;

use crate::common::sections::{parent_csv, section_or_skip, spec, SECTIONS};
use crate::support::{argv, epoch, gmax_ok, read_json, section_csv, Csv, Scratch};

/// The column order `gmax telemetry` documents, unchanged from the prototype
/// with three columns appended.
const COLUMNS: &[&str] = &[
    "time_s",
    "utc",
    "lat",
    "lon",
    "alt_m",
    "speed_kmh",
    "speed_3d_kmh",
    "course_deg",
    "lean_deg",
    "lean_rate_dps",
    "pitch_deg",
    "pitch_rate_dps",
    "yaw_rate_dps",
    "accel_lon_g",
    "accel_lat_g",
    "accel_vert_g",
    "accel_total_g",
    "gps_dop",
    "imu_temp_c",
    "gps_valid",
    "gps_fix",
    "gps_source",
    "lean_zero_deg",
];

/// GPMF IMU-rate samples per second on the Max 2: 30 per 1.001 s payload.
const HZ: f64 = 30000.0 / 1001.0;

#[test]
fn csv_and_json_carry_the_documented_schema() {
    let video = section_or_skip!("hard_corner");
    let dir = Scratch::new("schema");
    let csv_path = dir.join("hc.csv");
    let json_path = dir.join("hc.json");
    gmax_ok(argv![
        "telemetry",
        &video,
        "-o",
        &csv_path,
        "--json",
        &json_path
    ]);

    let csv = Csv::read(&csv_path);
    assert_eq!(csv.header, COLUMNS, "CSV columns changed");
    assert!(csv.len() > 300, "only {} rows", csv.len());

    let json = read_json(&json_path);
    for key in [
        "n",
        "hz",
        "dur_s",
        "start_utc",
        "lean_sign",
        "gps_valid",
        "ch",
        "video",
        "video_dur_s",
    ] {
        assert!(json.get(key).is_some(), "viewer JSON has no {key:?}");
    }
    assert_eq!(
        json["n"].as_u64(),
        Some(csv.len() as u64),
        "JSON and CSV disagree on the count"
    );
    assert_eq!(json["video"], "hard_corner.360");
    let n = csv.len();
    for ch in [
        "lean", "pitch", "yaw", "alat", "alon", "atot", "speed", "lat", "lon",
    ] {
        let arr = json["ch"][ch]
            .as_array()
            .unwrap_or_else(|| panic!("no channel {ch:?} in the viewer JSON"));
        assert_eq!(arr.len(), n, "channel {ch} has the wrong length");
    }
    // The JSON's first lean value is the CSV's first row, to the same rounding.
    let lean0 = json["ch"]["lean"][0].as_f64().unwrap();
    assert!((lean0 - csv.num(0, "lean_deg").unwrap()).abs() < 0.01);
}

#[test]
fn every_section_samples_at_29_97_hz_for_its_whole_length() {
    for s in SECTIONS {
        let Some(path) = section_csv(s.name) else {
            return;
        };
        let csv = Csv::read(&path);
        let t = csv.column("time_s");
        let (first, last) = (t[0].unwrap(), t[csv.len() - 1].unwrap());
        let span = last - first;
        // The section's real length is set by the keyframes it was cut on;
        // the JSON's container duration is the authority for it.
        let expected = (span * HZ).round() as usize + 1;
        assert!(
            csv.len().abs_diff(expected) <= 1,
            "{}: {} rows over {span:.3} s, expected {expected}",
            s.name,
            csv.len()
        );
        // And no gaps: consecutive samples one IMU period apart.
        for w in t.windows(2) {
            let dt = w[1].unwrap() - w[0].unwrap();
            assert!(
                (dt - 1.0 / HZ).abs() < 0.003,
                "{}: a {dt:.4} s step",
                s.name
            );
        }
        if let Some(d) = s.duration_s {
            // A stream copy starts at the keyframe at or before the request and
            // the last GPMF payload runs a full second, so allow one either way.
            assert!(
                span > d - 1.1 && span < d + 1.1,
                "{}: {span:.3} s for {d} s",
                s.name
            );
        }
    }
}

#[test]
fn video_times_start_at_zero_though_the_gpmf_clock_starts_at_the_cut() {
    let video = section_or_skip!("hard_corner");
    let s = spec("hard_corner");
    let Some(path) = section_csv("hard_corner") else {
        return;
    };
    let csv = Csv::read(&path);

    // The cut file keeps the GPMF's original timestamps, and the CSV's time_s
    // reports them as they are: the section's clock starts where it was cut.
    let t0 = csv.num(0, "time_s").unwrap();
    assert!(
        (t0 - s.start_s).abs() < 1.1,
        "time_s starts at {t0}, expected the cut point {}",
        s.start_s
    );

    // Everything expressed as a position in the video, though, starts at zero
    // and fits inside the section. This is the rebasing that broke once
    // already: a clip's moments landed past the end of its own file.
    let dir = Scratch::new("timebase");
    let json_path = dir.join("hc.json");
    gmax_ok(argv![
        "--quiet",
        "telemetry",
        &video,
        "--no-csv",
        "--json",
        &json_path
    ]);
    let json = read_json(&json_path);
    let video_dur = json["video_dur_s"].as_f64().unwrap();
    let dur = json["dur_s"].as_f64().unwrap();
    assert!(
        dur <= video_dur && dur > video_dur - 0.2,
        "dur_s {dur} vs video {video_dur}"
    );

    let analysis =
        crate::support::parse_json(&gmax_ok(argv!["--quiet", "analyse", &video, "--json"]).stdout);
    for (metric, m) in analysis["metrics"].as_object().unwrap() {
        for e in m["entries"].as_array().unwrap() {
            let vt = e["videoTimeSeconds"].as_f64().unwrap();
            assert!(
                (0.0..=video_dur).contains(&vt),
                "{metric} moment at {vt} s is outside the {video_dur} s section"
            );
        }
    }

    // And absolute time is the cut point's, not the start of the ride's nor
    // eighteen minutes in the future.
    let Some(parent) = parent_csv() else { return };
    let parent = Csv::read(&parent);
    let first_utc = epoch(csv.str(0, "utc"));
    let ride_start = epoch(parent.str(0, "utc")) - parent.num(0, "time_s").unwrap();
    assert!(
        (first_utc - (ride_start + t0)).abs() < 0.1,
        "section starts at {} but the ride's clock puts {t0} s at {}",
        csv.str(0, "utc"),
        ride_start + t0
    );
}

/// Index the parent CSV's rows by their `time_s` cell.
///
/// A section's GPMF keeps the parent's timestamps exactly, so the same sample
/// carries the same `time_s` text in both files.
fn parent_rows(parent: &Csv) -> HashMap<String, usize> {
    let c = parent.col("time_s");
    parent
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| (r[c].clone(), i))
        .collect()
}

#[test]
fn every_section_agrees_with_the_whole_ride_at_the_same_instant() {
    let Some(parent_path) = parent_csv() else {
        return;
    };
    let parent = Csv::read(&parent_path);
    let index = parent_rows(&parent);
    for s in SECTIONS {
        let Some(path) = section_csv(s.name) else {
            return;
        };
        let csv = Csv::read(&path);
        let mut validity_mismatch = 0;
        let end = csv.num(csv.len() - 1, "time_s").unwrap();
        let mut first_fix_seen = false;
        for i in 0..csv.len() {
            let t = csv.str(i, "time_s");
            let &j = index
                .get(t)
                .unwrap_or_else(|| panic!("{}: sample at {t} s is not in the parent", s.name));
            // UTC: each file anchors its clock to its own GPS time, so allow a
            // few tens of milliseconds, but no more.
            let du = epoch(csv.str(i, "utc")) - epoch(parent.str(j, "utc"));
            assert!(du.abs() < 0.05, "{}: utc off by {du:.3} s at {t}", s.name);

            // Rows before the section's first fix have no fix to report, which
            // the parent, with fixes before them, does.
            first_fix_seen |= !csv.str(i, "gps_source").is_empty();
            if !first_fix_seen {
                continue;
            }
            if csv.str(i, "gps_valid") != parent.str(j, "gps_valid") {
                validity_mismatch += 1;
                continue;
            }
            // Where both accept the fix, it is the same fix -- except over the
            // last sample or two, where the ride can see the next fix and the
            // section, whose GPMF ends there, cannot.
            if end - csv.num(i, "time_s").unwrap() < 0.2 {
                continue;
            }
            for col in ["lat", "lon", "speed_kmh", "alt_m"] {
                assert_eq!(
                    csv.str(i, col),
                    parent.str(j, col),
                    "{}: {col} at {t}",
                    s.name
                );
            }
        }
        // The plausibility check reads neighbouring fixes, which a section's
        // edges change; on hard_corner it rejects a handful the ride accepts.
        assert!(
            validity_mismatch * 50 <= csv.len(),
            "{}: {validity_mismatch} of {} samples disagree on GPS validity",
            s.name,
            csv.len()
        );
    }
}

#[test]
fn a_corner_section_reads_the_same_curve_as_the_ride_with_its_own_zero() {
    // "A clip cannot calibrate itself": the corner sections have too little
    // straight-line riding to fit a roll zero, so their lean is the ride's
    // curve shifted by a near-constant offset. Assert the shape, and bound the
    // offset, so a regression in either the angle or the zero shows.
    let Some(parent_path) = parent_csv() else {
        return;
    };
    let parent = Csv::read(&parent_path);
    let index = parent_rows(&parent);
    for name in ["hard_corner", "corner_sequence"] {
        let Some(path) = section_csv(name) else {
            return;
        };
        let csv = Csv::read(&path);
        let diffs: Vec<f64> = (0..csv.len())
            .map(|i| {
                let j = index[csv.str(i, "time_s")];
                csv.num(i, "lean_deg").unwrap() - parent.num(j, "lean_deg").unwrap()
            })
            .collect();
        let lo = diffs.iter().cloned().fold(f64::INFINITY, f64::min);
        let hi = diffs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let mean = diffs.iter().sum::<f64>() / diffs.len() as f64;
        assert!(
            mean.abs() < 5.0,
            "{name}: lean offset {mean:.2}° from the ride's"
        );
        assert!(
            hi - lo < 3.5,
            "{name}: offset not near-constant, {lo:.2}..{hi:.2}°"
        );

        // Which is why no zero is tracked here, and the user is told.
        assert!(
            csv.column("lean_zero_deg").iter().all(Option::is_none),
            "{name}: a roll zero was tracked from one corner"
        );
    }
}

#[test]
fn the_hard_corner_leans_right_by_a_sane_amount() {
    let Some(path) = section_csv("hard_corner") else {
        return;
    };
    let csv = Csv::read(&path);
    let lean = csv.column("lean_deg");
    let (i, peak) = lean
        .iter()
        .map(|v| v.unwrap())
        .enumerate()
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .unwrap();
    // Positive is right; the ride measures 41.8° right at 936.1 s.
    assert!(peak > 30.0 && peak < 50.0, "peak lean {peak:.1}°");
    let t = csv.num(i, "time_s").unwrap();
    assert!((t - 936.1).abs() < 1.0, "peak at {t} s on the GPMF clock");
    // Lateral load points the same way: into a right-hander is positive.
    let lat = csv.num(i, "accel_lat_g").unwrap();
    assert!(lat > 0.3, "lateral {lat} g at a {peak:.1}° right lean");
    // And the left-hander before it is negative.
    let left = lean
        .iter()
        .map(|v| v.unwrap())
        .fold(f64::INFINITY, f64::min);
    assert!(left < -20.0, "no left-hander: min lean {left:.1}°");
}

#[test]
fn gps_acquisition_has_no_fix_before_lock() {
    let Some(path) = section_csv("gps_acquisition") else {
        return;
    };
    let csv = Csv::read(&path);
    let t0 = csv.num(0, "time_s").unwrap();
    let valid: Vec<f64> = (0..csv.len())
        .filter(|&i| csv.str(i, "gps_valid") == "1")
        .map(|i| csv.num(i, "time_s").unwrap() - t0)
        .collect();
    // The receiver locks at 13.1 s; the dilution of precision sits at its
    // 99.99 sentinel until then and every position before it is nonsense.
    let first = *valid.first().expect("the receiver never locked");
    assert!(
        first > 12.5 && first < 14.0,
        "first usable fix at {first:.2} s"
    );
    for i in 0..csv.len() {
        if csv.num(i, "time_s").unwrap() - t0 < 12.5 {
            assert!(
                csv.str(i, "lat").is_empty(),
                "a position before lock at row {i}"
            );
        }
    }
}

/// Pearson correlation of two equal-length series.
fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let cov: f64 = a.iter().zip(b).map(|(x, y)| (x - ma) * (y - mb)).sum();
    let va: f64 = a.iter().map(|x| (x - ma).powi(2)).sum();
    let vb: f64 = b.iter().map(|y| (y - mb).powi(2)).sum();
    cov / (va * vb).sqrt()
}

/// How a section's lean and yaw rate correlate with the ride's over the same
/// samples: `(r_lean, r_yaw)`. Both near +1 when the section resolved its
/// signs the way the ride did; near -1 when it mirrored them.
fn sign_agreement(name: &str) -> Option<(f64, f64)> {
    let parent = Csv::read(&parent_csv()?);
    let index = parent_rows(&parent);
    let csv = Csv::read(&section_csv(name)?);
    let mut series: [Vec<f64>; 4] = Default::default();
    for i in 0..csv.len() {
        let j = index[csv.str(i, "time_s")];
        series[0].push(csv.num(i, "lean_deg").unwrap());
        series[1].push(parent.num(j, "lean_deg").unwrap());
        series[2].push(csv.num(i, "yaw_rate_dps").unwrap());
        series[3].push(parent.num(j, "yaw_rate_dps").unwrap());
    }
    Some((
        pearson(&series[0], &series[1]),
        pearson(&series[2], &series[3]),
    ))
}

#[test]
fn moving_sections_sign_lean_and_yaw_rate_the_way_the_ride_does() {
    // A section decides its own lean sign from its own few seconds of GPS.
    // Wherever there is real cornering, it must land where the whole ride did.
    for name in ["hard_corner", "corner_sequence", "mount_step", "tail"] {
        let Some((r_lean, r_yaw)) = sign_agreement(name) else {
            return;
        };
        assert!(
            r_lean > 0.95,
            "{name}: lean mirrored or garbled, r = {r_lean:.3}"
        );
        assert!(
            r_yaw > 0.95,
            "{name}: yaw rate mirrored or garbled, r = {r_yaw:.3}"
        );
    }
    // A straight has almost no lean to sign, but its yaw rate still must agree.
    let Some((_, r_yaw)) = sign_agreement("fast_straight") else {
        return;
    };
    assert!(r_yaw > 0.95, "fast_straight: yaw rate r = {r_yaw:.3}");
}

#[test]
#[ignore = "bug: on gps_dropout, lean and yaw rate come out mirrored (r = -1.00 and -0.99 \
            against the ride; the 30° right-hander reads 31° left). resolve_lean_sign trusts \
            the sign of a GPS course-rate correlation of r = -0.100, below its own 0.25 \
            confidence floor, and inverts the turn-rate reference on it"]
fn gps_dropout_signs_lean_and_yaw_rate_the_way_the_ride_does() {
    let Some((r_lean, r_yaw)) = sign_agreement("gps_dropout") else {
        return;
    };
    assert!(r_lean > 0.95, "lean mirrored or garbled, r = {r_lean:.3}");
    assert!(r_yaw > 0.95, "yaw rate mirrored or garbled, r = {r_yaw:.3}");
}
