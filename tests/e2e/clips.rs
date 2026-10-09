//! `gmax clips` on sections: ranked clips, hand cuts, clamping, dry runs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::sections::section_or_skip;
use crate::common::which;
use crate::support::{
    argv, gmax, gmax_ok, probe, probe_duration, read_json, section_csv, streams, video_duration,
    Csv, Scratch,
};

fn ffmpeg_or_skip() -> bool {
    if which("ffmpeg").is_none() {
        eprintln!("SKIP: ffmpeg is not installed");
        return false;
    }
    true
}

/// The files in a directory with this extension.
fn files_with(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(suffix))
        .collect();
    v.sort();
    v
}

#[test]
fn a_ranked_clip_comes_with_its_telemetry_and_a_manifest() {
    let video = section_or_skip!("corner_sequence");
    if !ffmpeg_or_skip() {
        return;
    }
    let dir = Scratch::new("clips-ranked");
    let out = dir.join("out");
    let run = gmax_ok(argv![
        "clips", &video, "--top", "1", "--metric", "lean", "-o", &out
    ]);
    assert!(run.stdout.contains("1 clip(s) planned"), "{}", run.stdout);

    let clips = files_with(&out, ".360");
    assert_eq!(clips.len(), 1, "{clips:?}");
    let clip = &clips[0];
    let name = clip.file_name().unwrap().to_string_lossy().to_string();
    assert!(name.starts_with("corner_sequence_lean01_"), "{name}");
    assert!(
        clip.with_extension("telemetry.csv").exists(),
        "no sibling telemetry"
    );

    let manifest = read_json(&out.join("corner_sequence_highlights.json"));
    let entry = &manifest["clips"][0];
    assert_eq!(entry["file"], name.as_str());
    let m = &entry["moments"][0];
    assert_eq!(m["metric"], "lean");
    assert_eq!(m["rank"], 1);
    let value = m["value"].as_f64().unwrap();
    assert!(value > 30.0 && value < 50.0, "lean #1 is {value}°");
    let at = m["videoTimeSeconds"].as_f64().unwrap();
    let (start, end) = (
        entry["startSeconds"].as_f64().unwrap(),
        entry["endSeconds"].as_f64().unwrap(),
    );
    assert!(
        start <= at && at <= end,
        "moment at {at} outside its clip {start}..{end}"
    );
    assert!(end <= manifest["sourceDurationSeconds"].as_f64().unwrap() + 1e-6);

    // A lossless copy keeping both lenses, the audio and the telemetry, but
    // not the timecode track the MOV muxer would duplicate.
    let Some(p) = probe(clip) else { return };
    let tags: Vec<String> = streams(&p)
        .iter()
        .map(|s| s["codec_tag_string"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(tags.iter().filter(|t| *t == "hvc1").count(), 2, "{tags:?}");
    assert!(tags.iter().any(|t| t == "gpmd"), "no GPMF track: {tags:?}");
    assert!(
        !tags.iter().any(|t| t == "tmcd"),
        "timecode track kept: {tags:?}"
    );
    // A stream copy starts at the keyframe at or before the request.
    let d = video_duration(&p);
    assert!(
        d >= end - start - 0.05 && d < end - start + 1.2,
        "clip lasts {d} s"
    );
}

#[test]
fn a_clips_sibling_csv_is_the_sections_own_rows() {
    // "A clip cannot calibrate itself": what is written beside a clip is the
    // source's telemetry, sliced, rather than anything re-derived from the
    // clip. So every row must be the source's row at the same instant, to the
    // character, with only time_s re-based to start at zero.
    let video = section_or_skip!("corner_sequence");
    if !ffmpeg_or_skip() {
        return;
    }
    let Some(parent_path) = section_csv("corner_sequence") else {
        return;
    };
    let dir = Scratch::new("clips-sibling");
    let out = dir.join("out");
    gmax_ok(argv![
        "--quiet", "clips", &video, "--top", "1", "--metric", "lean", "-o", &out
    ]);
    let sibling = Csv::read(&files_with(&out, ".telemetry.csv")[0]);
    let parent = Csv::read(&parent_path);
    assert_eq!(sibling.header, parent.header);

    let utc = parent.col("utc");
    let by_utc: HashMap<&str, usize> = parent
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| (r[utc].as_str(), i))
        .collect();
    let first = by_utc[sibling.str(0, "utc")];
    let origin = parent.num(first, "time_s").unwrap();
    assert_eq!(
        sibling.str(0, "time_s"),
        "0.000",
        "the sibling's clock does not start at zero"
    );
    for i in 0..sibling.len() {
        let j = *by_utc
            .get(sibling.str(i, "utc"))
            .unwrap_or_else(|| panic!("sibling row {i} is not an instant of the section"));
        assert_eq!(j, first + i, "sibling rows are not contiguous");
        for (c, name) in parent.header.iter().enumerate() {
            if name == "time_s" {
                let t = sibling.num(i, "time_s").unwrap();
                let expected = parent.num(j, "time_s").unwrap() - origin;
                assert!((t - expected).abs() < 0.0015, "time_s {t} for {expected}");
            } else {
                assert_eq!(
                    sibling.rows[i][c], parent.rows[j][c],
                    "{name} differs at row {i}"
                );
            }
        }
    }
    // And it covers the clip: the cut file's own length, give or take a frame.
    let clip = &files_with(&out, ".360")[0];
    if let Some(p) = probe(clip) {
        let span = sibling.num(sibling.len() - 1, "time_s").unwrap();
        let d = probe_duration(&p);
        assert!(
            (span - d).abs() < 1.1,
            "sibling covers {span} s of a {d} s clip"
        );
    }
}

#[test]
fn a_hand_cut_running_off_the_end_is_shortened_to_it() {
    let video = section_or_skip!("tail");
    if !ffmpeg_or_skip() {
        return;
    }
    let dir = Scratch::new("clips-clamp");
    let out = dir.join("out");
    gmax_ok(argv![
        "clips",
        &video,
        "--ss",
        "5",
        "--duration",
        "10",
        "-o",
        &out
    ]);
    let manifest = read_json(&out.join("tail_clips.json"));
    let source = manifest["sourceDurationSeconds"].as_f64().unwrap();
    let c = &manifest["clips"][0];
    let (start, end, dur) = (
        c["startSeconds"].as_f64().unwrap(),
        c["endSeconds"].as_f64().unwrap(),
        c["durationSeconds"].as_f64().unwrap(),
    );
    assert_eq!(start, 5.0);
    assert!(
        (end - source).abs() < 1e-6,
        "clamped to {end}, video is {source} s"
    );
    assert!((dur - (source - 5.0)).abs() < 1e-6);
    // What was asked for is kept beside what was cut.
    assert_eq!(c["requested"]["startSeconds"], 5.0);
    assert_eq!(c["requested"]["durationSeconds"], 10.0);
    assert!(c["moments"].as_array().unwrap().is_empty());

    let file = out.join(c["file"].as_str().unwrap());
    assert!(
        c["file"].as_str().unwrap().starts_with("tail_cut_t0005_"),
        "{}",
        c["file"]
    );
    assert!(file.exists());
    assert!(
        file.with_extension("telemetry.csv").exists(),
        "no sibling telemetry"
    );
    if let Some(p) = probe(&file) {
        // The video starts at the keyframe at or before 5 s (4.017 s here, the
        // one at 5.018 s being just too late), so it runs up to a keyframe
        // interval long. The container runs longer still: the GPMF payload
        // in effect at that keyframe began a second earlier.
        let d = video_duration(&p);
        assert!(
            d > dur - 0.1 && d < dur + 1.05,
            "the cut's video lasts {d} s, planned {dur}"
        );
        assert!(probe_duration(&p) < dur + 2.1);
    }
}

#[test]
fn a_hand_cut_starting_past_the_end_is_refused() {
    let video = section_or_skip!("tail");
    let dir = Scratch::new("clips-past");
    let out = dir.join("out");
    let run = gmax(argv!["clips", &video, "--ss", "20", "-o", &out]);
    assert!(!run.ok, "a cut past the end succeeded");
    assert!(run.stderr.contains("past the video"), "{}", run.stderr);
    assert!(
        !out.exists() || files_with(&out, ".360").is_empty(),
        "something was cut"
    );
}

#[test]
fn a_dry_run_plans_and_writes_nothing() {
    let video = section_or_skip!("corner_sequence");
    let dir = Scratch::new("clips-dry");
    let out = dir.join("out");
    let run = gmax_ok(argv![
        "clips",
        &video,
        "--top",
        "1",
        "--dry-run",
        "-o",
        &out
    ]);
    assert!(
        run.stdout.contains("clip(s) planned from"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("Lean #1"), "{}", run.stdout);
    assert!(!out.exists(), "a dry run created {}", out.display());

    // Merging disabled: each moment becomes its own window.
    let separate = gmax_ok(argv![
        "clips",
        &video,
        "--top",
        "1",
        "-n",
        "--merge-gap",
        "-1",
        "-o",
        &out
    ]);
    let count = |s: &str| {
        s.lines()
            .filter(|l| l.contains("..") && l.contains("s)"))
            .count()
    };
    assert!(
        count(&separate.stdout) > count(&run.stdout),
        "--merge-gap -1 merged anyway:\n{}",
        separate.stdout
    );
}
