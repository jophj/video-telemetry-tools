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
