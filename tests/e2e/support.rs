//! Running the binary under test and reading what it writes.
//!
//! The readers here are deliberately independent of `gmax`'s own parsers: a
//! CSV split on commas, a GPX walked with quick-xml, ffprobe for containers.
//! A round trip through the code under test would agree with itself whatever
//! it wrote.

#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use crate::common::sections::{gmax_binary, section};
use crate::common::{fixture, which, GPX_XSD};

/// What one run of `gmax` produced.
pub struct Run {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Run `gmax` with these arguments.
pub fn gmax<I, S>(args: I) -> Run
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = Command::new(gmax_binary())
        .args(args)
        .output()
        .expect("run gmax");
    Run {
        ok: out.status.success(),
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Run `gmax` and fail the test, with its stderr, if it does not succeed.
pub fn gmax_ok<I, S>(args: I) -> Run
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<_> = args
        .into_iter()
        .map(|a| a.as_ref().to_os_string())
        .collect();
    let run = gmax(&args);
    assert!(
        run.ok,
        "gmax {:?} failed ({:?}):\n{}",
        args, run.code, run.stderr
    );
    run
}

/// A private temporary directory, removed when the test ends.
///
/// Every output goes here, never beside the section: a sibling
/// `<section>.telemetry.csv` would change what the viewer and later runs read.
/// Set `GMAX_KEEP_SCRATCH=1` to keep them for inspection.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("gmax-e2e-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch directory");
        Scratch(dir)
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("GMAX_KEEP_SCRATCH").is_none() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// A CSV with a header row and no quoting, which is what gmax writes.
pub struct Csv {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Csv {
    pub fn read(path: &Path) -> Csv {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let mut lines = text.lines();
        let header: Vec<String> = lines
            .next()
            .unwrap_or_else(|| panic!("{} is empty", path.display()))
            .split(',')
            .map(str::to_string)
            .collect();
        let rows: Vec<Vec<String>> = lines
            .filter(|l| !l.is_empty())
            .map(|l| l.split(',').map(str::to_string).collect())
            .collect();
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(
                r.len(),
                header.len(),
                "{} row {} has the wrong width",
                path.display(),
                i + 2
            );
        }
        Csv { header, rows }
    }

    pub fn col(&self, name: &str) -> usize {
        self.header
            .iter()
            .position(|h| h == name)
            .unwrap_or_else(|| panic!("no column {name:?} in {:?}", self.header))
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn str(&self, row: usize, name: &str) -> &str {
        &self.rows[row][self.col(name)]
    }

    /// A numeric cell, `None` when blank (gmax's "not measured").
    pub fn num(&self, row: usize, name: &str) -> Option<f64> {
        let s = self.str(row, name);
        (!s.is_empty()).then(|| s.parse().unwrap_or_else(|e| panic!("{name}={s:?}: {e}")))
    }

    pub fn column(&self, name: &str) -> Vec<Option<f64>> {
        let c = self.col(name);
        self.rows
            .iter()
            .map(|r| (!r[c].is_empty()).then(|| r[c].parse().expect("numeric cell")))
            .collect()
    }
}

/// Seconds since the epoch for an RFC 3339 timestamp.
pub fn epoch(utc: &str) -> f64 {
    chrono::DateTime::parse_from_rfc3339(utc)
        .unwrap_or_else(|e| panic!("bad timestamp {utc:?}: {e}"))
        .timestamp_micros() as f64
        / 1e6
}

pub fn read_json(path: &Path) -> serde_json::Value {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
}

pub fn parse_json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("parsing JSON: {e}\n{text}"))
}

/// The whole telemetry CSV of a section, written once per test process.
///
/// Many tests read the same section's channels; this runs `gmax telemetry`
/// for each section at most once and hands every caller the same file.
pub fn section_csv(name: &str) -> Option<PathBuf> {
    static CACHE: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    let video = section(name)?;
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = map.get(name) {
        return Some(p.clone());
    }
    let dir = std::env::temp_dir().join(format!("gmax-e2e-{}-cache", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create cache directory");
    let out = dir.join(format!("{name}.telemetry.csv"));
    gmax_ok([
        OsStr::new("--quiet"),
        OsStr::new("telemetry"),
        video.as_os_str(),
        OsStr::new("-o"),
        out.as_os_str(),
    ]);
    map.insert(name.to_string(), out.clone());
    Some(out)
}

/// `ffprobe`'s view of a file's streams and format, or `None` without ffprobe.
pub fn probe(path: &Path) -> Option<serde_json::Value> {
    let ffprobe = which("ffprobe")?;
    let out = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .expect("run ffprobe");
    assert!(
        out.status.success(),
        "ffprobe {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    Some(serde_json::from_slice(&out.stdout).expect("ffprobe JSON"))
}

pub fn streams(probe: &serde_json::Value) -> Vec<serde_json::Value> {
    probe["streams"].as_array().cloned().unwrap_or_default()
}

/// The longest video stream's own duration, which is what a stream copy's
/// keyframe rounding moves; the container's can also include a leading GPMF
/// payload that starts before the first keyframe.
pub fn video_duration(probe: &serde_json::Value) -> f64 {
    streams(probe)
        .iter()
        .filter(|s| s["codec_type"] == "video")
        .filter_map(|s| s["duration"].as_str()?.parse::<f64>().ok())
        .fold(0.0, f64::max)
}

pub fn probe_duration(probe: &serde_json::Value) -> f64 {
    probe["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .expect("container duration")
}

/// The GPX 1.1 schema, wherever it can be found from this checkout.
///
/// `GPX_XSD` is relative to the main checkout; from a git worktree it is
/// found through `sources/`, which links back there. `GMAX_GPX_XSD` overrides.
pub fn gpx_xsd() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GMAX_GPX_XSD").map(PathBuf::from) {
        return p.exists().then_some(p);
    }
    if let Some(p) = fixture(GPX_XSD) {
        return Some(p);
    }
    let sources = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("sources");
    let checkout = std::fs::canonicalize(sources).ok()?.parent()?.to_path_buf();
    let p = checkout.join(GPX_XSD);
    p.exists().then_some(p)
}

/// Validate a GPX against the official schema, when xmllint and the schema are
/// both here; otherwise say so and carry on.
pub fn validate_gpx(path: &Path) {
    let (Some(xmllint), Some(xsd)) = (which("xmllint"), gpx_xsd()) else {
        eprintln!("SKIP (check only): xmllint or the GPX 1.1 schema is not available");
        return;
    };
    let out = Command::new(xmllint)
        .arg("--noout")
        .arg("--schema")
        .arg(&xsd)
        .arg(path)
        .output()
        .expect("run xmllint");
    assert!(
        out.status.success(),
        "{} does not validate against GPX 1.1:\n{}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// One `<trkpt>`, as far as these tests care.
#[derive(Debug, Default, Clone)]
pub struct TrackPoint {
    pub lat: f64,
    pub lon: f64,
    pub time: Option<String>,
    pub speed: Option<f64>,
    pub lean: Option<f64>,
    pub video_time: Option<f64>,
    pub source: Option<String>,
}

/// A GPX file: its raw text, for namespace checks, and its track points.
pub struct Gpx {
    pub text: String,
    pub points: Vec<TrackPoint>,
    /// `<gt:...>` elements in `<metadata><extensions>`, by local name.
    pub metadata: HashMap<String, String>,
}

pub fn read_gpx(path: &Path) -> Gpx {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let mut reader = Reader::from_str(&text);
    reader.config_mut().trim_text(true);
    let mut points = Vec::new();
    let mut metadata = HashMap::new();
    let mut current: Option<TrackPoint> = None;
    let mut stack: Vec<String> = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if name == "trkpt" {
                    let mut p = TrackPoint::default();
                    for a in e.attributes().flatten() {
                        let v: f64 = a.unescape_value().unwrap().parse().unwrap();
                        match a.key.local_name().as_ref() {
                            b"lat" => p.lat = v,
                            b"lon" => p.lon = v,
                            _ => {}
                        }
                    }
                    current = Some(p);
                }
                stack.push(name);
            }
            Ok(Event::End(_)) => {
                if stack.pop().as_deref() == Some("trkpt") {
                    points.push(current.take().unwrap());
                }
            }
            Ok(Event::Text(t)) => {
                let value = t.unescape().unwrap().into_owned();
                let leaf = stack.last().map(String::as_str).unwrap_or("");
                if let Some(p) = current.as_mut() {
                    match leaf {
                        "time" => p.time = Some(value),
                        "speed" => p.speed = value.parse().ok(),
                        "lean_deg" => p.lean = value.parse().ok(),
                        "video_time_s" => p.video_time = value.parse().ok(),
                        "gps_source" => p.source = Some(value),
                        _ => {}
                    }
                } else if stack.iter().any(|s| s == "metadata")
                    && stack.iter().any(|s| s == "extensions")
                {
                    metadata.insert(leaf.to_string(), value);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => panic!("{} is not well-formed XML: {e}", path.display()),
            _ => {}
        }
    }
    Gpx {
        text,
        points,
        metadata,
    }
}

/// The ride's measured clock offset between the camera and the phone, from
/// `gmax merge` over the whole reference: lean-angle correlation at +1.60 s
/// (|r| = 0.893, 1346 pairs).
///
/// A 15-second section has about 15 phone points against the 100 pairs the
/// lean alignment needs, so it cannot measure this itself; passing the ride's
/// value makes the section merge deterministic. The camera's GPS lag (+4.20 s
/// over the ride) is left out with `--no-gps-lag`, so the gap in the merged
/// track sits exactly where the section's CSV says the receiver lost lock.
pub const PHONE_TIME_OFFSET_S: &str = "1.6";

/// Build an argument list from strings and paths alike.
macro_rules! argv {
    ($($x:expr),* $(,)?) => {
        vec![$(std::ffi::OsString::from($x)),*]
    };
}
pub(crate) use argv;
