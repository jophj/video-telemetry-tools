//! Short sections of the reference ride, cut on first use.
//!
//! The reference recording is 8.55 GB and 25 minutes long, which is too big
//! and too slow to push through every end-to-end test. These are a handful of
//! windows chosen from its telemetry for what they exercise — GPS acquisition,
//! a hard corner, a GPS dropout, a mount slip, the end of the file — cut out
//! once, losslessly, into `sources/sections/` and reused from then on.
//!
//! The cut is made by ffmpeg directly, with the same arguments `gmax clips`
//! uses (`render::clip_args`), but *not* through `gmax`: a fixture that
//! depended on the code under test could not catch that code breaking.
//!
//! Safe under parallel test threads (one cut at a time per process, behind a
//! mutex) and parallel test processes (each writes a private temporary name
//! and renames it into place, so nobody ever sees half a file).
//!
//! Sections are never committed: `sources/` is gitignored. Pre-cut them all
//! with `cargo test --test e2e -- --ignored cut_all_sections`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use super::{fixture, which, REFERENCE_360};

/// Where cut sections, and the parent telemetry they are compared with, live.
pub const SECTIONS_DIR: &str = "sources/sections";

/// One window of the reference ride.
#[derive(Debug, Clone, Copy)]
pub struct Section {
    pub name: &'static str,
    /// Requested start in the reference, seconds. A stream copy begins at the
    /// keyframe at or before it; the Max 2 puts one every 1.001 s.
    pub start_s: f64,
    /// Requested length; `None` runs to the end of the recording.
    pub duration_s: Option<f64>,
    /// Why this window, with the figures measured on the full ride that chose it.
    pub why: &'static str,
}

/// The catalogue. Chosen from `gmax telemetry` and `gmax analyse --json` on the
/// whole reference (both read only its GPMF track), not by eye.
///
/// About 78 s in all, roughly 440 MB once cut.
pub const SECTIONS: &[Section] = &[
    Section {
        name: "gps_acquisition",
        start_s: 0.0,
        duration_s: Some(15.0),
        why: "The first 15 s, standing still: the receiver has no lock until 13.1 s \
              (DOP at the 99.99 sentinel), then 56 usable fixes. Lean flat at about \
              -11.5° raw with the bike on its stand.",
    },
    Section {
        name: "hard_corner",
        start_s: 930.0,
        duration_s: Some(10.0),
        why: "The ride's hardest lean, 41.8° right at 936.1 s (analyse lean #1), with \
              -31° left before it and 0.70 g of cornering load. GPS lost from 928.4 s and \
              patchy until 937.1 s, and its speeds are rejected as implausible around \
              936 s, so lean is all IMU.",
    },
    Section {
        name: "corner_sequence",
        start_s: 956.0,
        duration_s: Some(12.0),
        why: "A right-left pair: 39.0° right at 956.9 s (lean #2) and 38.7° left at \
              966.9 s (lean #3), with the ride's peak cornering load, 0.83 g left, at \
              967.2 s (lateral #1). 114 of 360 GPS samples usable.",
    },
    Section {
        name: "fast_straight",
        start_s: 1351.0,
        duration_s: Some(10.0),
        why: "Full throttle and hard braking on a straight with a clean lock: 85 to \
              159 km/h (speed #1 at 1356.7 s) and back to 111 km/h, lean within ±5°, \
              peak combined load 1.40 g at 1358.4 s (total #1). 276 of 300 fixes usable.",
    },
    Section {
        name: "gps_dropout",
        start_s: 301.0,
        duration_s: Some(15.0),
        why: "Lock, loss, reacquisition at 56-76 km/h: usable to 304.9 s, lost from \
              304.9 s to 313.3 s through a 30° right-hander, usable again after. The \
              phone's track covers it (it starts at 176 s), so merging has a gap to fill.",
    },
    Section {
        name: "mount_step",
        start_s: 780.0,
        duration_s: Some(10.0),
        why: "The mount slipping: the ride's tracked lean zero climbs from +0.4° to \
              +3.2° within these 10 s (the steepest step in the recording, 1.8° per 5 s \
              at 783 s), through a 31° left and 19° right, with every fix usable.",
    },
    Section {
        name: "tail",
        start_s: 1530.0,
        duration_s: None,
        why: "The last 7.5 s, to the end of the recording at 1537.5 s: for clamping a \
              duration that runs off the end and refusing a start past it. Usable GPS, \
              a 24° left at 63-75 km/h.",
    },
];

/// Look a section up by name. Panics on an unknown one, which is a typo in a test.
pub fn spec(name: &str) -> &'static Section {
    SECTIONS
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no section named {name:?} in the catalogue"))
}

pub fn sections_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(SECTIONS_DIR)
}

pub fn section_path(name: &str) -> PathBuf {
    sections_dir().join(format!("{name}.360"))
}

/// One cut at a time within a process. Parallel test threads asking for the
/// same section wait for the first to finish and then find the file there.
static CUT_LOCK: Mutex<()> = Mutex::new(());

/// The named section, cutting it from the reference on first use.
///
/// `None`, with a SKIP note on stderr, when the section is not cut and cannot
/// be — no reference recording, or no ffmpeg. An existing section is used
/// even when the reference is absent.
pub fn section(name: &str) -> Option<PathBuf> {
    let spec = spec(name);
    let path = section_path(name);
    if path.exists() {
        return Some(path);
    }
    let _guard = CUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if path.exists() {
        return Some(path);
    }
    let Some(reference) = fixture(REFERENCE_360) else {
        eprintln!(
            "SKIP: section {name} is not cut and the reference {REFERENCE_360} is not present"
        );
        return None;
    };
    let (Some(ffmpeg), Some(ffprobe)) = (which("ffmpeg"), which("ffprobe")) else {
        eprintln!("SKIP: section {name} is not cut and ffmpeg/ffprobe are not installed");
        return None;
    };
    match cut(&ffmpeg, &ffprobe, &reference, spec, &path) {
        Ok(()) => Some(path),
        Err(e) => panic!("cutting section {name}: {e}"),
    }
}

/// Resolve a section or print a skip note and return from the test.
#[allow(unused_macros)]
macro_rules! section_or_skip {
    ($name:expr) => {
        match crate::common::sections::section($name) {
            Some(p) => p,
            None => return,
        }
    };
}
#[allow(unused_imports)]
pub(crate) use section_or_skip;

/// The data streams tagged `tmcd`, which `gmax clips` also drops: the MOV
/// muxer would otherwise write a second, conflicting timecode track.
fn timecode_streams(ffprobe: &Path, input: &Path) -> Result<Vec<usize>, String> {
    let out = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=index,codec_tag_string",
            "-of",
            "json",
        ])
        .arg(input)
        .output()
        .map_err(|e| format!("running ffprobe: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("ffprobe JSON: {e}"))?;
    Ok(v["streams"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["codec_tag_string"] == "tmcd")
        .filter_map(|s| s["index"].as_u64().map(|i| i as usize))
        .collect())
}

fn cut(
    ffmpeg: &Path,
    ffprobe: &Path,
    reference: &Path,
    s: &Section,
    dest: &Path,
) -> Result<(), String> {
    std::fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // Hidden, unique per process and attempt, and still ending in .360 so
    // nothing has to guess what it is.
    let tmp = dest.with_file_name(format!(
        ".{}.{}.{nanos}.part.360",
        s.name,
        std::process::id()
    ));
    eprintln!(
        "cutting section {} ({:.1} s + {}) from {} ...",
        s.name,
        s.start_s,
        s.duration_s
            .map(|d| format!("{d:.1} s"))
            .unwrap_or_else(|| "to the end".into()),
        reference.display()
    );
    let started = std::time::Instant::now();

    // The same shape as gmax's own clip_args: input seek, every stream but the
    // timecode, stream copy including the GPMF track, zeroed timestamps, MOV.
    let mut cmd = Command::new(ffmpeg);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-ss",
        &format!("{:.3}", s.start_s),
    ])
    .arg("-i")
    .arg(reference);
    if let Some(d) = s.duration_s {
        cmd.args(["-t", &format!("{d:.3}")]);
    }
    cmd.args(["-map", "0"]);
    for i in timecode_streams(ffprobe, reference)? {
        cmd.args(["-map", &format!("-0:{i}")]);
    }
    cmd.args([
        "-c",
        "copy",
        "-copy_unknown",
        "-avoid_negative_ts",
        "make_zero",
        "-f",
        "mov",
        "-write_tmcd",
        "0",
    ])
    .arg(&tmp);
    let out = cmd.output().map_err(|e| format!("running ffmpeg: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    // Atomic on one filesystem: a concurrent reader sees no file or a whole one.
    std::fs::rename(&tmp, dest).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("renaming into place: {e}")
    })?;
    let mb = dest.metadata().map(|m| m.len() as f64 / 1e6).unwrap_or(0.0);
    eprintln!(
        "cut {} ({mb:.1} MB) in {:.1} s",
        dest.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// The test binary's sibling `gmax`, the binary under test.
pub fn gmax_binary() -> PathBuf {
    let mut p = std::env::current_exe().expect("test binary path");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join(if cfg!(windows) { "gmax.exe" } else { "gmax" })
}

fn modified(p: &Path) -> Option<SystemTime> {
    p.metadata().and_then(|m| m.modified()).ok()
}

static PARENT_CSV: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The whole reference ride's telemetry CSV, as the binary under test writes it.
///
/// What a section's telemetry is checked against: the same instant, measured
/// with the whole ride's calibration. Generating it reads only the GPMF track,
/// so it takes seconds, but it is still done at most once per test process and
/// cached as `sources/sections/parent.telemetry.csv` between runs. The cache
/// is rebuilt whenever it is older than the `gmax` binary, so a code change is
/// never compared against output from the code before it. With no reference
/// recording an existing cache is used as it is.
pub fn parent_csv() -> Option<PathBuf> {
    PARENT_CSV
        .get_or_init(|| {
            let cache = sections_dir().join("parent.telemetry.csv");
            let binary = gmax_binary();
            let fresh = match (modified(&cache), modified(&binary)) {
                (Some(c), Some(b)) => c >= b,
                (Some(_), None) => true,
                _ => false,
            };
            let Some(reference) = fixture(REFERENCE_360) else {
                if cache.exists() {
                    return Some(cache);
                }
                eprintln!("SKIP: no parent telemetry: {REFERENCE_360} is not present");
                return None;
            };
            if fresh {
                return Some(cache);
            }
            std::fs::create_dir_all(sections_dir()).ok()?;
            let tmp = cache.with_file_name(format!(".parent.{}.part.csv", std::process::id()));
            let out = Command::new(&binary)
                .args(["--quiet", "telemetry"])
                .arg(&reference)
                .arg("-o")
                .arg(&tmp)
                .output()
                .expect("run gmax telemetry on the reference");
            assert!(
                out.status.success(),
                "gmax telemetry on the reference failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            std::fs::rename(&tmp, &cache).expect("move the parent CSV into place");
            Some(cache)
        })
        .clone()
}
