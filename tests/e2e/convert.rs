//! `gmax convert` on a section. Kept to two tests: these are the slow ones.

use std::process::Command;

use crate::common::sections::section_or_skip;
use crate::common::which;
use crate::support::{argv, gmax_ok, probe, probe_duration, streams, Scratch};

fn tools_or_skip() -> bool {
    if which("ffmpeg").is_none() || which("ffprobe").is_none() {
        eprintln!("SKIP: ffmpeg/ffprobe are not installed");
        return false;
    }
    true
}

#[test]
fn a_preview_stitches_one_2_to_1_frame() {
    let video = section_or_skip!("hard_corner");
    if !tools_or_skip() {
        return;
    }
    let dir = Scratch::new("convert-preview");
    let out = dir.join("peak.jpg");
    // The frame at the peak of the corner.
    gmax_ok(argv![
        "--quiet",
        "convert",
        &video,
        "--preview",
        "--ss",
        "6",
        "-o",
        &out
    ]);
    let p = probe(&out).unwrap();
    let s = &streams(&p)[0];
    assert_eq!(s["codec_name"], "mjpeg");
    let (w, h) = (s["width"].as_u64().unwrap(), s["height"].as_u64().unwrap());
    assert_eq!(w, 2 * h, "{w}x{h} is not equirectangular");
    // Four cube faces across: the native angular resolution of the Max 2.
    assert_eq!((w, h), (3840, 1920));
}

#[test]
fn a_one_second_render_is_a_tagged_360_video_with_its_telemetry() {
    let video = section_or_skip!("hard_corner");
    if !tools_or_skip() {
        return;
    }
    let dir = Scratch::new("convert-render");
    let out = dir.join("hc_equirect.mp4");
    // Software H.264 at a small size: the cheapest encode every ffmpeg has.
    let run = gmax_ok(argv![
        "convert",
        &video,
        "--ss",
        "5",
        "--duration",
        "1",
        "--size",
        "960x480",
        "-q",
        "h264",
        "--accel",
        "cpu",
        "-o",
        &out,
    ]);
    assert!(run.stdout.contains("libx264"), "{}", run.stdout);

    let p = probe(&out).unwrap();
    let streams = streams(&p);
    let v: Vec<_> = streams
        .iter()
        .filter(|s| s["codec_type"] == "video")
        .collect();
    assert_eq!(v.len(), 1, "one stitched video stream");
    assert_eq!(v[0]["codec_name"], "h264");
    let (w, h) = (
        v[0]["width"].as_u64().unwrap(),
        v[0]["height"].as_u64().unwrap(),
    );
    assert_eq!((w, h), (960, 480));
    assert!(
        streams.iter().any(|s| s["codec_type"] == "audio"),
        "stereo audio dropped"
    );
    // The GPMF track is copied, so the render can be analysed like the source.
    assert!(
        streams.iter().any(|s| s["codec_tag_string"] == "gpmd"),
        "no GPMF data stream in the render"
    );
    let d = probe_duration(&p);
    assert!((d - 1.0).abs() < 0.1, "rendered {d} s for --duration 1");

    // And that copied track is readable telemetry.
    let csv = dir.join("render.csv");
    gmax_ok(argv!["--quiet", "telemetry", &out, "-o", &csv]);
    let rows = std::fs::read_to_string(&csv).unwrap().lines().count() - 1;
    assert!(rows >= 25, "{rows} telemetry rows from the render");

    // Declared spherical, which is what makes players offer 360 navigation.
    let Some(exiftool) = which("exiftool") else {
        eprintln!("SKIP (check only): exiftool is not installed, spherical metadata unchecked");
        return;
    };
    let tags = Command::new(exiftool)
        .args(["-s", "-XMP-GSpherical:all"])
        .arg(&out)
        .output()
        .expect("run exiftool");
    let tags = String::from_utf8_lossy(&tags.stdout);
    assert!(
        tags.contains("equirectangular"),
        "no GSpherical projection: {tags}"
    );
    assert!(
        tags.contains("Spherical") && tags.contains("True"),
        "{tags}"
    );
}

#[test]
fn chapters_are_cut_to_the_render_in_their_own_timebase() {
    // Regression: shifting chapters for --ss read every START/END as
    // milliseconds whatever the chapter's TIMEBASE said, so a chapter in
    // nanoseconds (as MKV tools export) was shifted by a millionth of the
    // right amount and fell outside the render.
    let video = section_or_skip!("corner_sequence");
    if !tools_or_skip() {
        return;
    }
    let dir = Scratch::new("convert-chapters");
    let chapters = dir.join("chapters.txt");
    std::fs::write(
        &chapters,
        ";FFMETADATA1\n\
         \n[CHAPTER]\nTIMEBASE=1/1000\nSTART=2200\nEND=2600\ntitle=milliseconds\n\
         \n[CHAPTER]\nTIMEBASE=1/1000000000\nSTART=3500000000\nEND=4200000000\ntitle=nanoseconds\n\
         \n[CHAPTER]\nTIMEBASE=1/90000\nSTART=810000\nEND=900000\ntitle=after the render\n",
    )
    .unwrap();
    let out = dir.join("chapters_equirect.mp4");
    gmax_ok(argv![
        "--quiet",
        "convert",
        &video,
        "--ss",
        "2",
        "--duration",
        "3",
        "--size",
        "480x240",
        "-q",
        "h264",
        "--accel",
        "cpu",
        "--chapters",
        &chapters,
        "-o",
        &out,
    ]);

    let shown = Command::new("ffprobe")
        .args(["-v", "error", "-show_chapters", "-of", "json"])
        .arg(&out)
        .output()
        .expect("run ffprobe");
    let json: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    let got: Vec<(String, f64)> = json["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["tags"]["title"].as_str().unwrap_or("").to_string(),
                c["start_time"].as_str().unwrap().parse().unwrap(),
            )
        })
        .collect();
    let titles: Vec<_> = got.iter().map(|(t, _)| t.as_str()).collect();
    // The chapter past the end of the render is dropped; both others survive.
    assert_eq!(titles, ["milliseconds", "nanoseconds"], "{got:?}");
    // MP4 chapters store only where each starts, and the muxer pins the first
    // to zero, so the second is the one whose shift can be checked: 3.5 s in
    // the section, 1.5 s into a render that begins at 2 s.
    assert!((got[1].1 - 1.5).abs() < 0.15, "nanosecond chapter starts at {}", got[1].1);
}
