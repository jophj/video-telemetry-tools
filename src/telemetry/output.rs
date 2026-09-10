//! Writing telemetry out as CSV and as the viewer's JSON.

use std::io::{BufWriter, Write};
use std::path::Path;

use super::{Source, Telemetry};

/// The CSV column order, unchanged from the prototype so that anything already
/// reading `*.telemetry.csv` keeps working.
///
/// `gps_fix` and `gps_source` are appended rather than inserted, for the same
/// reason: a reader indexing by position is unaffected, and one indexing by
/// name gains two columns it can ignore.
pub const COLUMNS: &[&str] = &[
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
];

/// Render a value to a fixed number of decimals, or the empty string for a
/// missing one.
///
/// A blank cell means "not measured". Writing `0` there instead would be a
/// measurement that was never taken, and would drag every average towards zero.
fn cell(v: Option<f64>, dp: usize) -> String {
    match v {
        Some(x) if x.is_finite() => format!("{x:.dp$}"),
        _ => String::new(),
    }
}

fn channel(c: &Option<Vec<f64>>, i: usize, dp: usize) -> String {
    cell(c.as_ref().map(|v| v[i]), dp)
}

pub fn write_csv(t: &Telemetry, path: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    write_csv_to(t, &mut w)
}

pub fn write_csv_to<W: Write>(t: &Telemetry, w: &mut W) -> anyhow::Result<()> {
    writeln!(w, "{}", COLUMNS.join(","))?;

    let mut row = String::with_capacity(256);
    for i in 0..t.n {
        row.clear();
        let fix = t.fix_at(i);
        let valid = fix.map(|f| f.valid).unwrap_or(false);
        // GPS columns are blanked unless the fix behind them is usable.
        let g = fix.filter(|f| f.valid);

        row.push_str(&format!("{:.3}", t.times[i]));
        row.push(',');
        row.push_str(
            &t.utc_at(i)
                .map(|u| u.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
                .unwrap_or_default(),
        );
        for c in [
            cell(g.map(|f| f.lat), 7),
            cell(g.map(|f| f.lon), 7),
            cell(g.map(|f| f.alt_m), 2),
            cell(g.map(|f| f.speed_2d * 3.6), 2),
            cell(g.map(|f| f.speed_3d * 3.6), 2),
            cell(g.and_then(|f| f.course_deg), 1),
            format!("{:.2}", t.lean_deg[i]),
            channel(&t.lean_rate_dps, i, 1),
            format!("{:.2}", t.pitch_deg[i]),
            channel(&t.pitch_rate_dps, i, 1),
            format!("{:.1}", t.yaw_rate_dps[i]),
            channel(&t.accel_lon_g, i, 3),
            channel(&t.accel_lat_g, i, 3),
            channel(&t.accel_vert_g, i, 3),
            channel(&t.accel_total_g, i, 3),
            cell(g.and_then(|f| f.dop), 2),
            channel(&t.imu_temp_c, i, 1),
            (valid as u8).to_string(),
            fix.and_then(|f| f.fix).map(|v| v.to_string()).unwrap_or_default(),
            fix.map(|f| f.source.as_str()).unwrap_or("").to_string(),
        ] {
            row.push(',');
            row.push_str(&c);
        }
        row.push('\n');
        w.write_all(row.as_bytes())?;
    }
    w.flush()?;
    Ok(())
}

/// A channel in the viewer's JSON: its key, its values and its precision.
struct JsonChannel<'a> {
    key: &'static str,
    dp: usize,
    values: Box<dyn Fn(usize) -> Option<f64> + 'a>,
}

/// Write the JSON the browser viewer consumes.
///
/// Streamed rather than built as a `serde_json::Value`: this is sixteen
/// channels over 46 080 samples, about 4 MB of output, and materialising it as
/// a tree of `Value` first costs far more memory than the whole telemetry set.
pub fn write_json(
    t: &Telemetry,
    path: &Path,
    video: Option<&str>,
    video_duration_s: Option<f64>,
    clip: Option<&str>,
) -> anyhow::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    write_json_to(t, &mut w, video, video_duration_s, clip)
}

pub fn write_json_to<W: Write>(
    t: &Telemetry,
    w: &mut W,
    video: Option<&str>,
    video_duration_s: Option<f64>,
    clip: Option<&str>,
) -> anyhow::Result<()> {
    let channels: Vec<JsonChannel> = vec![
        JsonChannel { key: "lean", dp: 2, values: Box::new(|i| Some(t.lean_deg[i])) },
        JsonChannel { key: "leanrate", dp: 1, values: Box::new(|i| t.lean_rate_dps.as_ref().map(|v| v[i])) },
        JsonChannel { key: "pitch", dp: 2, values: Box::new(|i| Some(t.pitch_deg[i])) },
        JsonChannel { key: "pitchrate", dp: 1, values: Box::new(|i| t.pitch_rate_dps.as_ref().map(|v| v[i])) },
        JsonChannel { key: "yaw", dp: 1, values: Box::new(|i| Some(t.yaw_rate_dps[i])) },
        JsonChannel { key: "speed", dp: 1, values: Box::new(|i| t.valid_fix_at(i).map(|f| f.speed_2d * 3.6)) },
        JsonChannel { key: "alon", dp: 3, values: Box::new(|i| t.accel_lon_g.as_ref().map(|v| v[i])) },
        JsonChannel { key: "alat", dp: 3, values: Box::new(|i| t.accel_lat_g.as_ref().map(|v| v[i])) },
        JsonChannel { key: "avert", dp: 3, values: Box::new(|i| t.accel_vert_g.as_ref().map(|v| v[i])) },
        JsonChannel { key: "atot", dp: 3, values: Box::new(|i| t.accel_total_g.as_ref().map(|v| v[i])) },
        JsonChannel { key: "alt", dp: 1, values: Box::new(|i| t.valid_fix_at(i).map(|f| f.alt_m)) },
        JsonChannel { key: "course", dp: 1, values: Box::new(|i| t.valid_fix_at(i).and_then(|f| f.course_deg)) },
        JsonChannel { key: "lat", dp: 6, values: Box::new(|i| t.valid_fix_at(i).map(|f| f.lat)) },
        JsonChannel { key: "lon", dp: 6, values: Box::new(|i| t.valid_fix_at(i).map(|f| f.lon)) },
        JsonChannel { key: "temp", dp: 1, values: Box::new(|i| t.imu_temp_c.as_ref().map(|v| v[i])) },
        JsonChannel { key: "dop", dp: 2, values: Box::new(|i| t.valid_fix_at(i).and_then(|f| f.dop)) },
    ];

    write!(w, "{{\"n\":{}", t.n)?;
    write!(w, ",\"hz\":{:.5}", t.hz)?;
    write!(w, ",\"hz_nominal\":{}", t.hz_nominal)?;
    write!(
        w,
        ",\"hz_note\":{}",
        json_string(&format!(
            "{} samples per {:.6} s GPMF payload, measured from the container sample table",
            t.hz_nominal, 1.0 / (t.hz / t.hz_nominal)
        ))
    )?;
    write!(w, ",\"dur_s\":{:.3}", t.duration_s)?;
    match t.t0 {
        Some(t0) => write!(
            w,
            ",\"start_utc\":{}",
            json_string(&t0.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        )?,
        None => write!(w, ",\"start_utc\":null")?,
    }
    write!(w, ",\"start_utc_is_fallback\":{}", t.t0_is_fallback)?;
    write!(w, ",\"gps_fix_pct\":{:.1}", t.fix_percentage())?;
    write!(
        w,
        ",\"lean_sign\":{{\"flipped\":{},\"correlation\":{:.4},\"gps_correlation\":{},\"samples\":{},\"confidence\":{}}}",
        t.lean_sign.flipped,
        t.lean_sign.correlation,
        t.lean_sign
            .gps_correlation
            .map(|r| format!("{r:.4}"))
            .unwrap_or_else(|| "null".into()),
        t.lean_sign.samples,
        json_string(t.lean_sign.confidence.as_str())
    )?;

    // Parallel arrays keyed by channel, which is what viewer.html expects.
    write!(w, ",\"gps_valid\":[")?;
    for i in 0..t.n {
        if i > 0 {
            w.write_all(b",")?;
        }
        w.write_all(if t.valid_fix_at(i).is_some() { b"1" } else { b"0" })?;
    }
    write!(w, "]")?;

    let mut missing: Vec<&str> = Vec::new();
    write!(w, ",\"ch\":{{")?;
    for (ci, ch) in channels.iter().enumerate() {
        if ci > 0 {
            w.write_all(b",")?;
        }
        write!(w, "{}:[", json_string(ch.key))?;
        let mut any = false;
        for i in 0..t.n {
            if i > 0 {
                w.write_all(b",")?;
            }
            match (ch.values)(i) {
                Some(v) if v.is_finite() => {
                    any = true;
                    write!(w, "{v:.*}", ch.dp)?;
                }
                _ => w.write_all(b"null")?,
            }
        }
        write!(w, "]")?;
        if !any {
            missing.push(ch.key);
        }
    }
    write!(w, "}}")?;

    write!(w, ",\"missing\":[")?;
    for (i, m) in missing.iter().enumerate() {
        if i > 0 {
            w.write_all(b",")?;
        }
        write!(w, "{}", json_string(m))?;
    }
    write!(w, "]")?;

    write!(
        w,
        ",\"video\":{}",
        video.map(json_string).unwrap_or_else(|| "null".into())
    )?;
    write!(
        w,
        ",\"clip\":{}",
        clip.map(json_string).unwrap_or_else(|| "null".into())
    )?;
    match video_duration_s {
        Some(d) => write!(w, ",\"video_dur_s\":{d:.3}")?,
        None => write!(w, ",\"video_dur_s\":null")?,
    }
    write!(w, "}}")?;
    w.flush()?;
    Ok(())
}

/// Minimal JSON string escaping.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A short human-readable summary, used by the CLI and the analyse report.
pub fn summary(t: &Telemetry) -> String {
    let mut s = String::new();
    let dist = t.distance_m() / 1000.0;
    s.push_str(&format!(
        "{} samples at {:.5} Hz ({} per payload), {:.1} s\n",
        t.n, t.hz, t.hz_nominal, t.duration_s
    ));
    if let Some(t0) = t.t0 {
        s.push_str(&format!(
            "start {}{}\n",
            t0.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
            if t.t0_is_fallback {
                " (from the container clock, not GPS)"
            } else {
                ""
            }
        ));
    }
    s.push_str(&format!(
        "GPS {} of {} fixes usable ({:.1}%), {:.2} km over {} run(s) of coverage, {:.0} m climb\n",
        t.valid_fix_count(),
        t.gps.len(),
        t.fix_percentage(),
        dist,
        t.coverage_runs().len(),
        t.elevation_gain_m()
    ));
    s.push_str(&format!(
        "lean sign: {} (confidence {}, |r| = {:.3} vs turn rate{})\n",
        if t.lean_sign.flipped {
            "inverted from raw"
        } else {
            "as measured"
        },
        t.lean_sign.confidence.as_str(),
        t.lean_sign.correlation,
        t.lean_sign
            .gps_correlation
            .map(|r| format!(", r = {r:+.3} vs GPS course rate"))
            .unwrap_or_default(),
    ));
    s
}

impl Source {
    /// Parse the `gps_source` column back, for tools reading our own CSV.
    pub fn parse(s: &str) -> Option<Source> {
        match s {
            "gopro" => Some(Source::Gopro),
            "external" => Some(Source::External),
            "interpolated" => Some(Source::Interpolated),
            _ => None,
        }
    }
}

/// Build the viewer's JSON from a telemetry CSV.
///
/// The viewer is usually pointed at a stitched or proxy rendering, which has no
/// GPMF track of its own, so the channels have to come from the CSV written
/// earlier. It also means a CSV that has been edited or merged can be inspected
/// without re-deriving anything.
pub fn json_from_csv(
    text: &str,
    video: Option<&str>,
    video_duration_s: Option<f64>,
) -> anyhow::Result<String> {
    let mut lines = text.lines();
    let header: Vec<&str> = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("the CSV is empty"))?
        .split(',')
        .map(|h| h.trim())
        .collect();
    let rows: Vec<Vec<&str>> = lines
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split(',').collect())
        .collect();
    if rows.is_empty() {
        anyhow::bail!("the CSV has a header but no rows");
    }

    let index = |name: &str| header.iter().position(|h| *h == name);
    // Spelled out rather than a closure: an inferred closure lifetime here ties
    // the returned &str to the borrow of `row` instead of to the input text.
    fn cell<'a>(row: &[&'a str], i: Option<usize>) -> Option<&'a str> {
        i.and_then(|i| row.get(i).copied())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
    }
    fn number(row: &[&str], i: Option<usize>) -> Option<f64> {
        cell(row, i).and_then(|s| s.parse().ok())
    }

    let i_time = index("time_s");
    let i_utc = index("utc");
    let i_valid = index("gps_valid");

    // The viewer's channel keys, and the CSV column each is read from.
    let channels: [(&str, Option<usize>, usize); 16] = [
        ("lean", index("lean_deg"), 2),
        ("leanrate", index("lean_rate_dps"), 1),
        ("pitch", index("pitch_deg"), 2),
        ("pitchrate", index("pitch_rate_dps"), 1),
        ("yaw", index("yaw_rate_dps"), 1),
        ("speed", index("speed_kmh"), 1),
        ("alon", index("accel_lon_g"), 3),
        ("alat", index("accel_lat_g"), 3),
        ("avert", index("accel_vert_g"), 3),
        ("atot", index("accel_total_g"), 3),
        ("alt", index("alt_m"), 1),
        ("course", index("course_deg"), 1),
        ("lat", index("lat"), 6),
        ("lon", index("lon"), 6),
        ("temp", index("imu_temp_c"), 1),
        ("dop", index("gps_dop"), 2),
    ];

    // Measure the rate from the timestamps rather than assuming it, so a CSV
    // at any cadence renders correctly.
    let times: Vec<f64> = rows.iter().filter_map(|r| number(r, i_time)).collect();
    let hz = if times.len() > 1 {
        let span = times[times.len() - 1] - times[0];
        if span > 0.0 {
            (times.len() - 1) as f64 / span
        } else {
            30.0
        }
    } else {
        30.0
    };
    let duration = times.last().copied().unwrap_or(0.0);
    let valid_count = rows
        .iter()
        .filter(|r| cell(r, i_valid) == Some("1"))
        .count();

    let mut out = String::new();
    out.push_str(&format!("{{\"n\":{}", rows.len()));
    out.push_str(&format!(",\"hz\":{hz:.5}"));
    out.push_str(&format!(",\"hz_nominal\":{}", hz.round()));
    out.push_str(&format!(
        ",\"hz_note\":{}",
        json_string(&format!(
            "{hz:.5} Hz, measured from the CSV's own time_s column"
        ))
    ));
    out.push_str(&format!(",\"dur_s\":{duration:.3}"));
    match rows.first().and_then(|r| cell(r, i_utc)) {
        Some(t) => out.push_str(&format!(",\"start_utc\":{}", json_string(t))),
        None => out.push_str(",\"start_utc\":null"),
    }
    out.push_str(",\"start_utc_is_fallback\":false");
    out.push_str(&format!(
        ",\"gps_fix_pct\":{:.1}",
        100.0 * valid_count as f64 / rows.len() as f64
    ));
    // A CSV carries no record of how its lean sign was decided.
    out.push_str(",\"lean_sign\":null");

    out.push_str(",\"gps_valid\":[");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push(if cell(row, i_valid) == Some("1") { '1' } else { '0' });
    }
    out.push(']');

    let mut missing: Vec<&str> = Vec::new();
    out.push_str(",\"ch\":{");
    for (ci, (key, column, dp)) in channels.iter().enumerate() {
        if ci > 0 {
            out.push(',');
        }
        out.push_str(&format!("{}:[", json_string(key)));
        let mut any = false;
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            match number(row, *column) {
                Some(v) if v.is_finite() => {
                    any = true;
                    out.push_str(&format!("{v:.*}", dp));
                }
                _ => out.push_str("null"),
            }
        }
        out.push(']');
        if !any {
            missing.push(key);
        }
    }
    out.push('}');

    out.push_str(",\"missing\":[");
    for (i, m) in missing.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&json_string(m));
    }
    out.push(']');

    out.push_str(&format!(
        ",\"video\":{}",
        video.map(json_string).unwrap_or_else(|| "null".into())
    ));
    out.push_str(",\"clip\":null");
    match video_duration_s {
        Some(d) => out.push_str(&format!(",\"video_dur_s\":{d:.3}")),
        None => out.push_str(",\"video_dur_s\":null"),
    }
    out.push('}');
    Ok(out)
}
