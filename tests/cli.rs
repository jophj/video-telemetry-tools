//! Command-line surface: help, exit codes, and argument validation.
//!
//! These run the real binary, so they also check that every subcommand is
//! actually wired up and that clap accepts the argument shapes documented in
//! the README.

mod common;

use std::path::PathBuf;
use std::process::{Command, Output};

fn binary() -> PathBuf {
    // The test binary lives beside the one under test.
    let mut p = std::env::current_exe().expect("test binary path");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join(if cfg!(windows) { "gmax.exe" } else { "gmax" })
}

fn run(args: &[&str]) -> Output {
    Command::new(binary())
        .args(args)
        .output()
        .expect("run gmax")
}

const SUBCOMMANDS: &[&str] = &[
    "convert",
    "clips",
    "telemetry",
    "gpx",
    "merge",
    "analyse",
    "viewer",
    "encoders",
];

#[test]
fn every_subcommand_is_wired_up_and_documented() {
    let top = run(&["--help"]);
    assert!(top.status.success());
    let text = String::from_utf8_lossy(&top.stdout);
    for name in SUBCOMMANDS {
        assert!(text.contains(name), "{name} missing from the top-level help");
        let help = run(&[name, "--help"]);
        assert!(help.status.success(), "{name} --help failed");
        let h = String::from_utf8_lossy(&help.stdout);
        assert!(!h.is_empty(), "{name} --help printed nothing");
    }
}

#[test]
fn version_is_reported() {
    let out = run(&["--version"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn a_missing_file_fails_with_a_message_naming_it() {
    let out = run(&["telemetry", "/nonexistent/nope.360"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("nope.360"), "stderr was: {err}");
    // And it says which operation failed, not just that something did.
    assert!(err.contains("reading"), "stderr was: {err}");
}

#[test]
fn a_file_that_is_not_a_video_is_refused_clearly() {
    let path = std::env::temp_dir().join("gmax-not-a-video.360");
    std::fs::write(&path, b"this is not an mp4 at all, not even close").unwrap();
    let out = run(&["telemetry", &path.to_string_lossy()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("ISO base media") || err.contains("moov"),
        "stderr was: {err}"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn an_unknown_metric_lists_the_valid_ones() {
    let out = run(&["analyse", "/dev/null", "--metric", "wobble"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("wobble"), "stderr was: {err}");
    // Naming the alternatives is the difference between an error and a hint.
    for valid in ["lean", "accel", "decel", "speed", "curves"] {
        assert!(err.contains(valid), "{valid} not offered: {err}");
    }
}

#[test]
fn negative_numbers_are_accepted_where_they_are_meaningful() {
    // Rotations, headings and merge offsets are routinely negative, and clap
    // reads a leading minus as a flag unless told otherwise.
    for args in [
        vec!["convert", "x.360", "--rotate", "-45:117.9:0", "--dry-run"],
        vec!["convert", "x.360", "--heading", "-90", "--dry-run"],
        vec!["clips", "x.360", "--merge-gap", "-1", "--dry-run"],
    ] {
        let out = run(&args);
        let err = String::from_utf8_lossy(&out.stderr);
        // It should fail on the missing input file, not on parsing.
        assert!(
            !err.contains("unexpected argument"),
            "{args:?} was rejected by the parser: {err}"
        );
    }
}

#[test]
fn a_bad_rotation_says_what_the_format_is() {
    let out = run(&["convert", "x.360", "--rotate", "1:2", "--dry-run"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("three colon-separated"), "stderr was: {err}");
}

#[test]
fn encoders_reports_what_this_machine_can_do() {
    if common::which("ffmpeg").is_none() {
        eprintln!("SKIP: ffmpeg is not installed");
        return;
    }
    let out = run(&["encoders"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("ffmpeg"), "no ffmpeg version line");
    // Software encoders are trusted from the build list, so at least one of
    // these should always be present on a usable ffmpeg.
    assert!(
        text.contains("libx264") || text.contains("libx265"),
        "no software encoder listed"
    );
    // And what each preset resolves to here, which is the point of the command.
    assert!(text.contains("--quality high"), "presets not resolved");
}

#[test]
fn merge_requires_an_external_source() {
    let out = run(&["merge", "x.360"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--with"), "stderr was: {err}");
}

#[test]
fn an_empty_gpx_is_refused_rather_than_merged() {
    let path = std::env::temp_dir().join("gmax-empty.gpx");
    std::fs::write(
        &path,
        r#"<gpx version="1.1" xmlns="http://www.topografix.com/GPX/1/1"><trk><trkseg/></trk></gpx>"#,
    )
    .unwrap();
    let out = run(&["merge", "x.360", "--with", &path.to_string_lossy()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no track points"), "stderr was: {err}");
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_clip_by_time_needs_a_start_and_refuses_the_ranking_options() {
    // --duration says how long the --ss clip runs, so it means nothing alone.
    let out = run(&["clips", "x.360", "--duration", "10"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--ss"), "stderr was: {err}");

    // The ranking options cannot shape a window that was given, so asking for
    // both is a misunderstanding worth reporting rather than half-honouring.
    for (flag, value) in [("--top", "3"), ("--pre", "2"), ("--metric", "lean")] {
        let out = run(&["clips", "x.360", "--ss", "10", flag, value]);
        assert!(!out.status.success(), "{flag} was accepted alongside --ss");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("cannot be used with"), "stderr was: {err}");
    }

    // But the telemetry options still apply: they shape the CSV written
    // beside the clip, which a hand-cut clip gets exactly like a ranked one.
    let out = run(&["clips", "x.360", "--ss", "10", "--max-dop", "3", "--dry-run"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("cannot be used with"), "stderr was: {err}");
}

#[test]
fn hand_cuts_need_no_telemetry_and_share_one_manifest() {
    if common::which("ffmpeg").is_none() {
        eprintln!("SKIP: ffmpeg is not installed");
        return;
    }
    // Two lens strips shaped like a .360's (3 x 32 px faces + 2 x 4 px of
    // overlap), and no GPMF track at all: the ride a hand cut exists for.
    let dir = std::env::temp_dir().join(format!("gmax-handcut-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("ride.360");
    let made = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc=size=104x32:rate=10:duration=6"])
        .args(["-f", "lavfi", "-i", "testsrc=size=104x32:rate=10:duration=6"])
        .args(["-map", "0", "-map", "1", "-c:v", "mpeg4", "-g", "10", "-f", "mov"])
        .arg(&input)
        .status()
        .expect("run ffmpeg");
    assert!(made.success(), "could not build the synthetic input");
    let input = input.to_str().unwrap();

    for start in ["1", "3"] {
        let out = run(&["clips", input, "--ss", start, "--duration", "1.5"]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "--ss {start} failed: {err}");
        assert!(err.contains("no telemetry"), "the missing CSV went unexplained: {err}");
    }

    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("ride_clips").join("ride_clips.json")).unwrap(),
    )
    .unwrap();
    let files: Vec<_> = manifest["clips"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["file"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(files, ["ride_cut_t0001_1p5s.360", "ride_cut_t0003_1p5s.360"]);
    for f in &files {
        assert!(dir.join("ride_clips").join(f).exists(), "{f} was not cut");
    }
    assert_eq!(manifest["clips"][1]["requested"]["startSeconds"], 3.0);

    // Cutting the same window again replaces its entry rather than doubling it.
    let out = run(&["clips", input, "--ss", "3", "--duration", "1.5", "--overwrite"]);
    assert!(out.status.success());
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("ride_clips").join("ride_clips.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["clips"].as_array().unwrap().len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A scratch directory unique to one test in this run.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gmax-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Two lens strips shaped like a .360's (3 x 32 px faces + 2 x 4 px overlap).
fn synthetic_360(path: &std::path::Path) {
    let made = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc=size=104x32:rate=10:duration=3"])
        .args(["-f", "lavfi", "-i", "testsrc=size=104x32:rate=10:duration=3"])
        .args(["-map", "0", "-map", "1", "-c:v", "mpeg4", "-g", "10", "-f", "mov"])
        .arg(path)
        .status()
        .expect("run ffmpeg");
    assert!(made.success(), "could not build the synthetic input");
}

#[test]
fn a_single_track_mp4_can_be_cut_by_time() {
    if common::which("ffmpeg").is_none() {
        eprintln!("SKIP: ffmpeg is not installed");
        return;
    }
    // One video track and no strip geometry: an ordinary .mp4, or a stitched
    // rendering. A stream copy needs neither two lenses nor a cubemap.
    let dir = scratch("mp4cut");
    let input = dir.join("ride.mp4");
    let made = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc=size=128x64:rate=10:duration=4"])
        .args(["-c:v", "mpeg4", "-g", "10"])
        .arg(&input)
        .status()
        .expect("run ffmpeg");
    assert!(made.success());

    let out = run(&["clips", input.to_str().unwrap(), "--ss", "1", "--duration", "1.5"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "the cut failed: {err}");
    // An .mp4 stays an .mp4, as a .360 stays a .360.
    let clip = dir.join("ride_clips").join("ride_cut_t0001_1p5s.mp4");
    assert!(clip.exists(), "{} was not written", clip.display());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_command_writes_over_its_own_input() {
    let dir = scratch("selfwrite");
    let input = dir.join("ride.360");
    let original = b"stand-in for an irreplaceable recording";
    std::fs::write(&input, original).unwrap();
    let same = input.to_str().unwrap().to_string();
    // Spelled differently, resolving to the same file.
    let respelled = dir.join(".").join("ride.360").to_str().unwrap().to_string();

    for args in [
        vec!["telemetry", &same, "-o", &respelled],
        vec!["telemetry", &same, "--no-csv", "--json", &same],
        vec!["gpx", &same, "-o", &respelled],
        vec!["convert", &same, "-o", &respelled, "--dry-run"],
        vec!["merge", &same, "--with", &same, "-o", &same],
    ] {
        let out = run(&args);
        assert!(!out.status.success(), "{args:?} was allowed");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("it is the input file"), "{args:?}: {err}");
    }
    assert_eq!(std::fs::read(&input).unwrap(), original);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_preview_stitches_one_frame_to_an_oddly_named_file() {
    if common::which("ffmpeg").is_none() {
        eprintln!("SKIP: ffmpeg is not installed");
        return;
    }
    let dir = scratch("preview");
    // A colon would read as a protocol and a leading dash as an option.
    let input = dir.join("-ride:1.360");
    synthetic_360(&input);
    let output = dir.join("-look:1.jpg");
    let out = Command::new(binary())
        .current_dir(&dir)
        // `--out=` and `--` get the names past clap; the point is ffmpeg.
        .args(["convert", "--preview", "--out=-look:1.jpg", "--", "-ride:1.360"])
        .output()
        .expect("run gmax");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "the preview failed: {err}");
    let jpeg = std::fs::read(&output).expect("the preview was not written");
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "not a JPEG");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_dry_run_quotes_the_ffmpeg_path_too() {
    let Some(ffmpeg) = common::which("ffmpeg") else {
        eprintln!("SKIP: ffmpeg is not installed");
        return;
    };
    if cfg!(windows) {
        return;
    }
    let dir = scratch("dryrun");
    let input = dir.join("ride.360");
    synthetic_360(&input);
    // Where Windows and many macOS users keep tools: a path with a space.
    let tools = dir.join("my tools");
    std::fs::create_dir_all(&tools).unwrap();
    let linked = tools.join("ffmpeg");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&ffmpeg, &linked).unwrap();

    let out = run(&[
        "convert",
        input.to_str().unwrap(),
        "--dry-run",
        "--accel",
        "cpu",
        "--ffmpeg",
        linked.to_str().unwrap(),
    ]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with(&format!("'{}' ", linked.display())),
        "the program path was not quoted: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
