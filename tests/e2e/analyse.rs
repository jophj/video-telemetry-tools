//! `gmax analyse` on sections, and on the CSV and GPX made from them.

use serde_json::Value;

use crate::common::sections::section_or_skip;
use crate::support::{argv, gmax, gmax_ok, parse_json, section_csv, Csv, Scratch};

const METRICS: &[&str] = &[
    "lean", "accel", "decel", "speed", "curves", "lateral", "total",
];

fn analyse_json(args: Vec<std::ffi::OsString>) -> Value {
    let mut full = argv!["--quiet", "analyse"];
    full.extend(args);
    full.push("--json".into());
    parse_json(&gmax_ok(full).stdout)
}

fn entries<'a>(a: &'a Value, metric: &str) -> &'a Vec<Value> {
    a["metrics"][metric]["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("no {metric} entries"))
}

#[test]
fn the_json_report_has_its_documented_shape() {
    let video = section_or_skip!("corner_sequence");
    let a = analyse_json(argv![&video]);
    for key in ["cadence", "metrics", "options", "quality", "ride", "units"] {
        assert!(a.get(key).is_some(), "no {key:?} in the report");
    }
    for m in METRICS {
        let metric = &a["metrics"][*m];
        assert!(metric.is_object(), "metric {m} missing");
        for key in ["entries", "title", "unit", "skipped", "unavailable"] {
            assert!(metric.get(key).is_some(), "{m} has no {key:?}");
        }
    }
    // Camera samples are ranked at the receiver's 10 Hz, measured, not assumed.
    let hz = a["cadence"]["measuredHz"].as_f64().unwrap();
    assert!((hz - 10.0).abs() < 0.1, "measured {hz} Hz");
    assert_eq!(a["units"]["lean"], "degrees, signed, positive to the right");
    let dur = a["ride"]["durationSeconds"].as_f64().unwrap();
    assert!(dur > 11.0 && dur < 13.5, "ride lasts {dur} s");
    for e in entries(&a, "lean") {
        for key in [
            "rank",
            "leanDeg",
            "direction",
            "videoTimeSeconds",
            "timestamp",
        ] {
            assert!(e.get(key).is_some(), "lean entry has no {key:?}: {e}");
        }
    }
}

#[test]
fn the_hard_corner_is_found_inside_its_section() {
    let video = section_or_skip!("hard_corner");
    let a = analyse_json(argv![&video]);
    let top = &entries(&a, "lean")[0];
    let lean = top["leanDeg"].as_f64().unwrap();
    let vt = top["videoTimeSeconds"].as_f64().unwrap();
    assert_eq!(top["direction"], "right");
    assert!(lean > 30.0 && lean < 50.0, "lean #1 is {lean}°");
    // 936.1 s on the ride, 930 s cut point: about 6.1 s into the section.
    assert!((vt - 6.2).abs() < 1.0, "lean #1 at {vt} s");
}

#[test]
fn the_corner_sequence_finds_both_corners_and_the_peak_cornering_load() {
    let video = section_or_skip!("corner_sequence");
    let a = analyse_json(argv![&video]);
    let lean = entries(&a, "lean");
    let right = lean
        .iter()
        .find(|e| e["direction"] == "right")
        .expect("no right-hander");
    let left = lean
        .iter()
        .find(|e| e["direction"] == "left")
        .expect("no left-hander");
    assert!(right["leanDeg"].as_f64().unwrap() > 30.0);
    assert!(left["leanDeg"].as_f64().unwrap() < -30.0);
    // Right first, at 956.9 s; left ten seconds later, at 966.9 s.
    assert!(
        right["videoTimeSeconds"].as_f64().unwrap() < left["videoTimeSeconds"].as_f64().unwrap()
    );

    // The ride's peak cornering load, 0.83 g left at 967.2 s, is in here.
    let lateral = &entries(&a, "lateral")[0];
    let g = lateral["accelG"].as_f64().unwrap();
    assert!(g.abs() > 0.6 && g.abs() < 1.2, "cornering #1 is {g} g");
    assert!(!entries(&a, "curves").is_empty(), "no curve detected");
}

#[test]
fn a_csv_made_from_a_section_ranks_like_the_section_itself() {
    let video = section_or_skip!("hard_corner");
    let Some(csv) = section_csv("hard_corner") else {
        return;
    };
    let from_video = analyse_json(argv![&video, "--metric", "lean,lateral"]);
    let from_csv = analyse_json(argv![&csv, "--metric", "lean,lateral"]);
    for metric in ["lean", "lateral"] {
        let (v, c) = (
            &entries(&from_video, metric)[0],
            &entries(&from_csv, metric)[0],
        );
        let key = if metric == "lean" {
            "leanDeg"
        } else {
            "accelG"
        };
        let (vv, cv) = (v[key].as_f64().unwrap(), c[key].as_f64().unwrap());
        assert!(
            (vv - cv).abs() < 0.05,
            "{metric}: {vv} from the video, {cv} from its CSV"
        );
        // The CSV's time_s starts at the cut point, yet its video times still
        // start at zero. Not to the sample: the video is ranked at the 10 Hz
        // fix rate and the CSV at 29.97 Hz, and lateral #1 here is a tie
        // between two samples 0.17 s apart, which each source breaks its way.
        let (vt, ct) = (
            v["videoTimeSeconds"].as_f64().unwrap(),
            c["videoTimeSeconds"].as_f64().unwrap(),
        );
        assert!(
            (vt - ct).abs() < 0.3,
            "{metric}: at {vt} s from the video, {ct} s from its CSV"
        );
    }
}

#[test]
fn a_gpx_made_from_a_section_ranks_lean_but_not_load() {
    let video = section_or_skip!("hard_corner");
    let dir = Scratch::new("analyse-gpx");
    let gpx = dir.join("hc.gpx");
    gmax_ok(argv!["--quiet", "gpx", &video, "-o", &gpx]);
    let a = analyse_json(argv![&gpx]);
    // A GPX keeps lean only at the usable fixes, which miss the very peak.
    let lean = entries(&a, "lean")[0]["leanDeg"].as_f64().unwrap();
    assert!(
        lean > 30.0 && lean < 50.0,
        "lean #1 from the GPX is {lean}°"
    );
    let vt = entries(&a, "lean")[0]["videoTimeSeconds"].as_f64().unwrap();
    assert!((vt - 6.2).abs() < 1.5, "lean #1 from the GPX at {vt} s");
    // Cornering and combined load have no GPX representation, and say so.
    for m in ["lateral", "total"] {
        assert!(entries(&a, m).is_empty());
        assert!(
            a["metrics"][m]["unavailable"].is_string(),
            "{m} not marked unavailable"
        );
    }
}

#[test]
fn chapters_edl_and_cues_are_written_and_parse() {
    let video = section_or_skip!("corner_sequence");
    let dir = Scratch::new("analyse-files");
    let (ch, edl, cues) = (dir.join("ch.txt"), dir.join("cs.edl"), dir.join("cues.csv"));
    gmax_ok(argv![
        "analyse",
        &video,
        "--top",
        "2",
        "--chapters",
        &ch,
        "--edl",
        &edl,
        "--cues",
        &cues
    ]);
    let duration_ms = 13_100.0;

    let text = std::fs::read_to_string(&ch).unwrap();
    assert!(text.starts_with(";FFMETADATA1"), "{text}");
    let chapters = text.matches("[CHAPTER]").count();
    assert!(chapters >= 1);
    assert!(text.contains("TIMEBASE=1/1000"));
    let starts: Vec<f64> = text
        .lines()
        .filter_map(|l| l.strip_prefix("START="))
        .map(|v| v.parse().unwrap())
        .collect();
    let ends: Vec<f64> = text
        .lines()
        .filter_map(|l| l.strip_prefix("END="))
        .map(|v| v.parse().unwrap())
        .collect();
    assert_eq!(starts.len(), chapters);
    for (s, e) in starts.iter().zip(&ends) {
        assert!(0.0 <= *s && s < e && *e <= duration_ms, "chapter {s}..{e}");
    }

    let text = std::fs::read_to_string(&edl).unwrap();
    assert!(text.starts_with("TITLE: corner_sequence"), "{text}");
    assert!(text.contains("FCM: NON-DROP FRAME"));
    let events: Vec<&str> = text.lines().filter(|l| l.starts_with("00")).collect();
    assert_eq!(
        events.len(),
        chapters,
        "EDL events and chapters describe different windows"
    );
    for e in events {
        // Four HH:MM:SS:FF timecodes per event.
        let tcs = e
            .split_whitespace()
            .filter(|w| w.matches(':').count() == 3)
            .count();
        assert_eq!(tcs, 4, "{e}");
    }

    let cues = Csv::read(&cues);
    assert_eq!(
        cues.header[..4],
        ["start_s", "end_s", "duration_s", "metric"]
    );
    assert!(cues.len() >= 7, "{} cues for seven metrics", cues.len());
    for i in 0..cues.len() {
        let (s, e) = (
            cues.num(i, "start_s").unwrap(),
            cues.num(i, "end_s").unwrap(),
        );
        assert!(s >= 0.0 && e > s && e <= duration_ms / 1000.0);
        assert!(METRICS.contains(&cues.str(i, "metric")));
    }
}

#[test]
#[ignore = "fixed on fix/review-2: analyse's --merge-gap does not accept a negative value \
            (clap reads -1 as a flag), though its help says negative disables merging"]
fn a_negative_merge_gap_disables_merging() {
    let video = section_or_skip!("corner_sequence");
    let dir = Scratch::new("analyse-merge-gap");
    let (merged, separate) = (dir.join("merged.txt"), dir.join("separate.txt"));
    gmax_ok(argv![
        "--quiet",
        "analyse",
        &video,
        "--top",
        "2",
        "--chapters",
        &merged
    ]);
    let run = gmax(argv![
        "--quiet",
        "analyse",
        &video,
        "--top",
        "2",
        "--merge-gap",
        "-1",
        "--chapters",
        &separate
    ]);
    assert!(run.ok, "--merge-gap -1 was refused: {}", run.stderr);
    let count = |p: &std::path::Path| {
        std::fs::read_to_string(p)
            .unwrap()
            .matches("[CHAPTER]")
            .count()
    };
    assert!(
        count(&separate) > count(&merged),
        "merging was not disabled"
    );
}
