//! Regression goldens: a compact summary of each section, compared field by
//! field against `tests/golden/<section>.json` with explicit tolerances.
//!
//! The summary is about core telemetry, moments and clip planning — the
//! things a change to calibration, GPS gating or ranking would move — and not
//! about merge internals, which are still being refined.
//!
//! A deliberate change is accepted by re-blessing:
//!
//! ```text
//! GMAX_BLESS=1 cargo test --test e2e golden
//! ```
//!
//! and reviewing the diff of `tests/golden/` like any other.

use std::path::PathBuf;

use serde_json::{json, Map, Value};

use crate::common::sections::{section, spec};
use crate::support::{argv, gmax_ok, parse_json, read_gpx, read_json, section_csv, Csv, Scratch};

/// How closely a field must match its golden value.
#[derive(Debug, Clone, Copy)]
enum Tol {
    Exact,
    Abs(f64),
    /// Relative, with an absolute floor for values near zero.
    Rel(f64, f64),
}

/// The tolerance for each field, by name. Explicit counts first, then by unit
/// suffix, so a new field gets a sensible default from its name.
fn tolerance(key: &str) -> Tol {
    match key {
        "samples" | "clips.count" => Tol::Exact,
        "video_duration_s" => Tol::Abs(0.05),
        // GPS gating looks at neighbouring fixes, so a few samples either way.
        "gps.valid_samples" => Tol::Abs(5.0),
        "gps.track_points" => Tol::Abs(2.0),
        "moments.curves_detected" | "moments.curves.value_count" => Tol::Abs(1.0),
        k if k.ends_with("_s") => Tol::Abs(0.3),
        k if k.ends_with("_deg") => Tol::Abs(1.0),
        k if k.ends_with("_g") => Tol::Abs(0.03),
        k if k.ends_with("_mps2") => Tol::Abs(0.5),
        k if k.ends_with("_kmh") => Tol::Abs(1.0),
        k if k.ends_with("_m") => Tol::Rel(0.05, 3.0),
        _ => Tol::Exact,
    }
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.json"))
}

fn blessing() -> bool {
    std::env::var("GMAX_BLESS")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

fn round(x: f64, dp: i32) -> Value {
    let k = 10f64.powi(dp);
    json!((x * k).round() / k)
}

/// The top entry's value and video time for each metric.
fn top_moments(fields: &mut Map<String, Value>, analysis: &Value) {
    for (metric, key, suffix) in [
        ("lean", "leanDeg", "deg"),
        ("accel", "accelMps2", "mps2"),
        ("decel", "accelMps2", "mps2"),
        ("speed", "speedKmh", "kmh"),
        ("curves", "curveCount", "count"),
        ("lateral", "accelG", "g"),
        ("total", "accelG", "g"),
    ] {
        let top = analysis["metrics"][metric]["entries"].get(0);
        let value = top.and_then(|e| e[key].as_f64()).map(|v| round(v, 3));
        let at = top
            .and_then(|e| e["videoTimeSeconds"].as_f64())
            .map(|v| round(v, 3));
        fields.insert(
            format!("moments.{metric}.value_{suffix}"),
            value.unwrap_or(Value::Null),
        );
        fields.insert(
            format!("moments.{metric}.video_s"),
            at.unwrap_or(Value::Null),
        );
    }
}

/// The section's summary, from the binary under test.
fn summarise(name: &str) -> Option<Map<String, Value>> {
    let video = section(name)?;
    let csv = Csv::read(&section_csv(name)?);
    let dir = Scratch::new(&format!("golden-{name}"));
    let mut f = Map::new();

    // Telemetry.
    let n = csv.len();
    f.insert("samples".into(), json!(n));
    let t: Vec<f64> = csv
        .column("time_s")
        .into_iter()
        .map(Option::unwrap)
        .collect();
    let t0 = t[0];
    let json_path = dir.join("t.json");
    gmax_ok(argv![
        "--quiet",
        "telemetry",
        &video,
        "--no-csv",
        "--json",
        &json_path
    ]);
    let tj = read_json(&json_path);
    f.insert(
        "video_duration_s".into(),
        round(tj["video_dur_s"].as_f64().unwrap(), 3),
    );
    f.insert(
        "lean.sign_flipped".into(),
        tj["lean_sign"]["flipped"].clone(),
    );
    f.insert(
        "lean.sign_confidence".into(),
        tj["lean_sign"]["confidence"].clone(),
    );

    // GPS.
    let valid: Vec<usize> = (0..n).filter(|&i| csv.str(i, "gps_valid") == "1").collect();
    f.insert("gps.valid_samples".into(), json!(valid.len()));
    f.insert(
        "gps.first_fix_video_s".into(),
        valid
            .first()
            .map(|&i| round(t[i] - t0, 3))
            .unwrap_or(Value::Null),
    );
    let speed = csv
        .column("speed_kmh")
        .into_iter()
        .flatten()
        .fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))));
    f.insert(
        "speed.max_kmh".into(),
        speed.map(|v| round(v, 2)).unwrap_or(Value::Null),
    );
    let gpx_path = dir.join("t.gpx");
    gmax_ok(argv!["--quiet", "gpx", &video, "-o", &gpx_path]);
    f.insert(
        "gps.track_points".into(),
        json!(read_gpx(&gpx_path).points.len()),
    );

    // Lean.
    let lean: Vec<f64> = csv
        .column("lean_deg")
        .into_iter()
        .map(Option::unwrap)
        .collect();
    let (pi, peak) = lean
        .iter()
        .copied()
        .enumerate()
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .unwrap();
    f.insert(
        "lean.right_peak_deg".into(),
        json!(lean.iter().copied().fold(f64::MIN, f64::max)),
    );
    f.insert(
        "lean.left_peak_deg".into(),
        json!(lean.iter().copied().fold(f64::MAX, f64::min)),
    );
    f.insert(
        "lean.peak_side".into(),
        json!(if peak >= 0.0 { "right" } else { "left" }),
    );
    f.insert("lean.peak_video_s".into(), round(t[pi] - t0, 3));
    f.insert(
        "lean.zero_tracked".into(),
        json!(csv.column("lean_zero_deg").iter().any(Option::is_some)),
    );

    // Load.
    let lat: Vec<f64> = csv.column("accel_lat_g").into_iter().flatten().collect();
    let lat_peak = lat
        .iter()
        .copied()
        .max_by(|a, b| a.abs().total_cmp(&b.abs()));
    f.insert(
        "load.lateral_peak_g".into(),
        lat_peak.map(|v| json!(v)).unwrap_or(Value::Null),
    );
    let total = csv
        .column("accel_total_g")
        .into_iter()
        .flatten()
        .fold(f64::MIN, f64::max);
    f.insert("load.total_peak_g".into(), json!(total));

    // Moments.
    let analysis = parse_json(&gmax_ok(argv!["--quiet", "analyse", &video, "--json"]).stdout);
    f.insert(
        "gps.distance_m".into(),
        round(analysis["ride"]["distanceMetres"].as_f64().unwrap(), 1),
    );
    f.insert(
        "moments.curves_detected".into(),
        analysis["quality"]["curvesDetected"].clone(),
    );
    top_moments(&mut f, &analysis);

    // Clip planning, one moment per metric, as `clips` would cut them.
    let plan = gmax_ok(argv![
        "clips",
        &video,
        "--top",
        "1",
        "--dry-run",
        "-o",
        dir.join("plan")
    ]);
    let windows: Vec<(f64, f64)> = plan
        .stdout
        .lines()
        .filter_map(|l| {
            let first = l.split_whitespace().next()?;
            let (a, b) = first.split_once("..")?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .collect();
    f.insert("clips.count".into(), json!(windows.len()));
    for (i, (a, b)) in windows.iter().enumerate() {
        f.insert(format!("clips.{i}.start_s"), json!(a));
        f.insert(format!("clips.{i}.end_s"), json!(b));
    }
    Some(f)
}

fn differs(key: &str, want: &Value, got: &Value) -> Option<String> {
    match (want, got, tolerance(key)) {
        (Value::Number(w), Value::Number(g), tol) => {
            let (w, g) = (w.as_f64().unwrap(), g.as_f64().unwrap());
            let ok = match tol {
                Tol::Exact => w == g,
                Tol::Abs(d) => (w - g).abs() <= d,
                Tol::Rel(r, floor) => (w - g).abs() <= (r * w.abs()).max(floor),
            };
            (!ok).then(|| format!("{key}: golden {w}, now {g} (tolerance {tol:?})"))
        }
        (w, g, _) => (w != g).then(|| format!("{key}: golden {w}, now {g}")),
    }
}

fn check(name: &str) {
    let Some(fields) = summarise(name) else {
        return;
    };
    let path = golden_path(name);
    let s = spec(name);
    if blessing() {
        let doc = json!({
            "section": name,
            "start_s": s.start_s,
            "duration_s": s.duration_s,
            "why": s.why.split_whitespace().collect::<Vec<_>>().join(" "),
            "fields": fields,
        });
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap() + "\n").unwrap();
        eprintln!("blessed {}", path.display());
        return;
    }
    let golden = match std::fs::read_to_string(&path) {
        Ok(text) => parse_json(&text),
        Err(_) => panic!(
            "no golden at {}; create it with GMAX_BLESS=1 cargo test --test e2e golden",
            path.display()
        ),
    };
    assert_eq!(
        golden["start_s"].as_f64(),
        Some(s.start_s),
        "{name} was blessed from a different window; re-bless"
    );
    let want = golden["fields"].as_object().expect("golden has no fields");
    let mut problems = Vec::new();
    for key in want
        .keys()
        .chain(fields.keys().filter(|k| !want.contains_key(*k)))
    {
        match (want.get(key), fields.get(key)) {
            (Some(w), Some(g)) => problems.extend(differs(key, w, g)),
            (Some(_), None) => problems.push(format!("{key}: in the golden, no longer produced")),
            (None, Some(g)) => problems.push(format!("{key}: new field ({g}), not in the golden")),
            (None, None) => unreachable!(),
        }
    }
    assert!(
        problems.is_empty(),
        "{name} no longer matches {}:\n  {}\nIf the change is intended, re-bless with \
         GMAX_BLESS=1 cargo test --test e2e golden",
        path.display(),
        problems.join("\n  ")
    );
}

macro_rules! golden {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                check(stringify!($name));
            }
        )*
    };
}

mod golden_section {
    use super::check;
    golden!(
        gps_acquisition,
        hard_corner,
        corner_sequence,
        fast_straight,
        gps_dropout,
        mount_step,
        tail
    );
}

#[test]
fn every_catalogued_section_has_a_golden_test() {
    // The macro list above and the catalogue must not drift apart.
    let listed = [
        "gps_acquisition",
        "hard_corner",
        "corner_sequence",
        "fast_straight",
        "gps_dropout",
        "mount_step",
        "tail",
    ];
    for s in crate::common::sections::SECTIONS {
        assert!(
            listed.contains(&s.name),
            "section {} has no golden test",
            s.name
        );
    }
}
