//! Rendering the analysis as tables and as JSON.

use super::{Analysis, MetricId};
use crate::math::fmt_num;

/// Format an offset from the start of the ride as `+HH:MM:SS.mmm`.
pub fn format_offset(seconds: f64) -> String {
    let sign = if seconds < 0.0 { '-' } else { '+' };
    let s = seconds.abs();
    let mut whole = s.floor() as i64;
    let mut ms = ((s - s.floor()) * 1000.0).round() as i64;
    // Rounding up from .9996 must carry into the seconds, not print ".1000".
    if ms >= 1000 {
        ms -= 1000;
        whole += 1;
    }
    format!(
        "{sign}{:02}:{:02}:{:02}.{:03}",
        whole / 3600,
        (whole / 60) % 60,
        whole % 60,
        ms
    )
}

/// Lay out a table, right-aligning any column that is entirely numeric.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return "  (no data)\n".to_string();
    }
    let cols = headers.len();
    let mut width: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            width[i] = width[i].max(cell.chars().count());
        }
    }
    // A column of numbers reads far better right-aligned; a column of words
    // does not.
    let numeric: Vec<bool> = (0..cols)
        .map(|i| {
            rows.iter().all(|r| {
                let c = r.get(i).map(|s| s.trim()).unwrap_or("");
                c.is_empty()
                    || c == "—"
                    || c.chars()
                        .all(|ch| ch.is_ascii_digit() || matches!(ch, '-' | '+' | '.' | ','))
            })
        })
        .collect();

    let pad = |s: &str, w: usize, right: bool| {
        let n = s.chars().count();
        let fill = " ".repeat(w.saturating_sub(n));
        if right {
            format!("{fill}{s}")
        } else {
            format!("{s}{fill}")
        }
    };

    let mut out = String::new();
    let header: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| pad(h, width[i], numeric[i]))
        .collect();
    out.push_str(&format!("  {}\n", header.join("  ").trim_end()));
    let rule: Vec<String> = width.iter().map(|w| "-".repeat(*w)).collect();
    out.push_str(&format!("  {}\n", rule.join("  ")));
    for row in rows {
        let cells: Vec<String> = (0..cols)
            .map(|i| pad(row.get(i).map(|s| s.as_str()).unwrap_or(""), width[i], numeric[i]))
            .collect();
        out.push_str(&format!("  {}\n", cells.join("  ").trim_end()));
    }
    out
}

fn headers_for(id: MetricId) -> Vec<&'static str> {
    match id {
        MetricId::Lean => vec!["#", "lean°", "dir", "from start", "km/h", "lat, lon"],
        MetricId::Accel | MetricId::Decel => {
            vec!["#", "m/s²", "g", "from start", "km/h", "lat, lon"]
        }
        MetricId::Speed => vec!["#", "km/h", "m/s", "from start", "lat, lon"],
        MetricId::Curves => vec!["#", "curves", "window", "peak lean°", "lat, lon"],
        MetricId::Lateral => vec!["#", "g", "dir", "from start", "km/h", "lat, lon"],
        MetricId::Total => vec!["#", "g", "from start", "km/h", "lat, lon"],
    }
}

fn row_for(id: MetricId, e: &super::Entry) -> Vec<String> {
    let coord = match e.pos {
        Some((lat, lon)) => format!("{lat:.5}, {lon:.5}"),
        // The IMU knew what happened; the receiver did not know where.
        None => "—".to_string(),
    };
    let kmh = e
        .speed_kmh()
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "—".into());
    let offset = format_offset(e.offset_s);
    match id {
        MetricId::Lean => vec![
            e.rank.to_string(),
            format!(
                "{:.1}{}",
                e.value.abs(),
                if e.at_sensor_limit { " !" } else { "" }
            ),
            e.direction.unwrap_or("").to_string(),
            offset,
            kmh,
            coord,
        ],
        MetricId::Accel | MetricId::Decel => vec![
            e.rank.to_string(),
            format!("{:.2}", e.value),
            format!("{:.2}", e.g.unwrap_or(0.0)),
            offset,
            kmh,
            coord,
        ],
        MetricId::Speed => vec![
            e.rank.to_string(),
            format!("{:.1}", e.value),
            format!("{:.2}", e.speed_ms.unwrap_or(0.0)),
            offset,
            coord,
        ],
        MetricId::Curves => vec![
            e.rank.to_string(),
            e.curve_count.unwrap_or(0).to_string(),
            format!(
                "{} → {}",
                format_offset(e.offset_s),
                format_offset(e.offset_s + e.span_s())
            ),
            format!("{:.1}", e.peak_lean_deg.unwrap_or(0.0).abs()),
            coord,
        ],
        MetricId::Lateral => vec![
            e.rank.to_string(),
            format!("{:.2}", e.value.abs()),
            e.direction.unwrap_or("").to_string(),
            offset,
            kmh,
            coord,
        ],
        MetricId::Total => vec![
            e.rank.to_string(),
            format!("{:.2}", e.value),
            offset,
            kmh,
            coord,
        ],
    }
}

const RULE: &str = "==============================================================================";

pub fn render_text(a: &Analysis) -> String {
    let mut s = String::new();
    s.push_str(&format!("{RULE}\n"));
    s.push_str(&format!(
        "  {}\n",
        if a.source.is_empty() {
            "Ride analysis".to_string()
        } else {
            a.source.clone()
        }
    ));
    s.push_str(&format!("{RULE}\n\n"));

    // --- ride -------------------------------------------------------------
    s.push_str("RIDE\n");
    let t0 = chrono::DateTime::from_timestamp(
        a.t0.trunc() as i64,
        ((a.t0 - a.t0.trunc()) * 1e9) as u32,
    );
    match t0 {
        Some(t0) if a.has_absolute_clock() => s.push_str(&format!(
            "  Start     {}\n",
            t0.format("%Y-%m-%dT%H:%M:%S%.3fZ")
        )),
        // Times from the start of the recording would print as 1970.
        _ => s.push_str(
            "  Start     unknown: no GPS clock, so times run from the recording's start\n",
        ),
    }
    s.push_str(&format!(
        "  Duration  {}\n",
        format_offset(a.cadence.duration_s).trim_start_matches('+')
    ));
    s.push_str(&format!("  Distance  {:.2} km\n", a.distance_m / 1000.0));
    s.push_str(&format!("  Samples   {}\n", a.cadence.sample_count));

    // --- cadence ----------------------------------------------------------
    s.push_str("\nSAMPLE CADENCE (measured from the data; metadata is never used to compute)\n");
    match a.cadence.hz {
        Some(hz) => s.push_str(&format!(
            "  Measured    {hz:.3} Hz (median Δt {:.3} s, p5 {:.3} s, p95 {:.3} s)\n",
            a.cadence.median_dt, a.cadence.p5_dt, a.cadence.p95_dt
        )),
        None => s.push_str("  Measured    unavailable\n"),
    }
    s.push_str(&format!(
        "  Regularity  {}\n",
        if a.cadence.regular { "regular" } else { "irregular" }
    ));
    if !a.declared_hz.is_empty() {
        let parts: Vec<String> = a
            .declared_hz
            .iter()
            .map(|(k, v)| format!("{k}={v:.2}"))
            .collect();
        s.push_str(&format!(
            "  Declared    {} (informational only)\n",
            parts.join(", ")
        ));
    }
    if let Some(hz) = a.cadence.hz {
        s.push_str(&format!(
            "  Fidelity    reported maxima are maxima of the samples present; at {hz:.1} Hz they \
             may understate the true instantaneous peak\n"
        ));
    }

    if !a.warnings.is_empty() {
        s.push_str("\nWARNINGS\n");
        for w in &a.warnings {
            s.push_str(&format!("  ! {w}\n"));
        }
    }

    // --- quality ----------------------------------------------------------
    s.push_str("\nDATA QUALITY\n");
    let q = &a.quality;
    for (label, n) in [
        ("lean angle", q.missing_lean),
        ("speed", q.missing_speed),
        ("acceleration", q.missing_accel),
    ] {
        if n > 0 {
            s.push_str(&format!(
                "  {n} sample(s) missing {label}, skipped for that metric only\n"
            ));
        }
    }
    if let Some(l) = q.lean_limit {
        s.push_str(&format!(
            "  lean appears clipped at ±{:.1}° on {} sample(s), flagged with ! below\n",
            l.value, l.samples
        ));
    }
    s.push_str(&format!(
        "  {} curve(s) detected beyond ±{:.0}°\n",
        q.curves_detected, a.options.curve_threshold_deg
    ));

    s.push_str("\nUNITS\n");
    s.push_str("  lean and course in degrees, speed in km/h (m/s where noted),\n");
    s.push_str("  acceleration in m/s² and g, position in decimal degrees\n");

    // --- metrics ----------------------------------------------------------
    for m in &a.metrics {
        s.push_str(&format!(
            "\n{} — top {}\n",
            m.id.title().to_uppercase(),
            a.options.top
        ));
        if let Some(reason) = m.unavailable {
            s.push_str(&format!("  unavailable: {reason}\n"));
            continue;
        }
        if matches!(m.id, MetricId::Accel | MetricId::Decel) {
            let note = match a.options.accel_source {
                super::AccelSource::Gps => {
                    "(differentiated from GPS speed, not from the raw accelerometer)"
                }
                super::AccelSource::Imu => "(from the vehicle-frame IMU)",
            };
            s.push_str(&format!("  {note}\n"));
        }
        let rows: Vec<Vec<String>> = m.entries.iter().map(|e| row_for(m.id, e)).collect();
        s.push_str(&table(&headers_for(m.id), &rows));
    }
    s
}

/// Render the analysis as JSON.
///
/// Keys match what the prototype's `extract-highlights.py` already consumes, so
/// that it keeps working against `gmax` output.
pub fn render_json(a: &Analysis) -> serde_json::Value {
    use serde_json::{json, Map, Value};

    let mut metrics = Map::new();
    for m in &a.metrics {
        let entries: Vec<Value> = m
            .entries
            .iter()
            .map(|e| {
                let mut o = Map::new();
                o.insert("rank".into(), json!(e.rank));
                o.insert(
                    "timestamp".into(),
                    match chrono::DateTime::from_timestamp(
                        e.t.trunc() as i64,
                        ((e.t - e.t.trunc()) * 1e9) as u32,
                    ) {
                        Some(t) if a.has_absolute_clock() => {
                            json!(t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
                        }
                        _ => Value::Null,
                    },
                );
                o.insert(
                    "epochSeconds".into(),
                    if a.has_absolute_clock() { json!(round(e.t, 3)) } else { Value::Null },
                );
                o.insert("offsetSeconds".into(), json!(round(e.offset_s, 3)));
                o.insert("offset".into(), json!(format_offset(e.offset_s)));
                if let Some(v) = e.video_time_s {
                    o.insert("videoTimeSeconds".into(), json!(round(v, 3)));
                }
                match e.pos {
                    Some((lat, lon)) => {
                        o.insert("lat".into(), json!(round(lat, 7)));
                        o.insert("lon".into(), json!(round(lon, 7)));
                    }
                    None => {
                        o.insert("lat".into(), Value::Null);
                        o.insert("lon".into(), Value::Null);
                    }
                }
                o.insert("elevationM".into(), opt(e.ele_m, 1));
                o.insert("speedMps".into(), opt(e.speed_ms, 3));
                o.insert("speedKmh".into(), opt(e.speed_kmh(), 2));
                match m.id {
                    MetricId::Lean => {
                        o.insert("leanDeg".into(), json!(round(e.value, 2)));
                        o.insert("direction".into(), json!(e.direction));
                        o.insert("atSensorLimit".into(), json!(e.at_sensor_limit));
                    }
                    MetricId::Accel | MetricId::Decel => {
                        o.insert("accelMps2".into(), json!(round(e.value, 3)));
                        o.insert("accelG".into(), opt(e.g, 3));
                    }
                    MetricId::Speed => {
                        o.insert("speedKmh".into(), json!(round(e.value, 2)));
                    }
                    MetricId::Curves => {
                        o.insert("curveCount".into(), json!(e.curve_count));
                        if let Some((start, end)) = e.window_s {
                            o.insert(
                                "windowStartOffsetSeconds".into(),
                                json!(round(start - a.t0, 3)),
                            );
                            o.insert(
                                "windowEndOffsetSeconds".into(),
                                json!(round(end - a.t0, 3)),
                            );
                            o.insert("windowSeconds".into(), json!(round(end - start, 3)));
                        }
                        o.insert("peakLeanDeg".into(), opt(e.peak_lean_deg, 2));
                    }
                    MetricId::Lateral | MetricId::Total => {
                        o.insert("accelG".into(), json!(round(e.value, 3)));
                        if let Some(d) = e.direction {
                            o.insert("direction".into(), json!(d));
                        }
                    }
                }
                Value::Object(o)
            })
            .collect();

        metrics.insert(
            m.id.slug().to_string(),
            json!({
                "title": m.id.title(),
                "unit": m.id.unit(),
                "unavailable": m.unavailable,
                "skipped": m.skipped,
                "entries": entries,
            }),
        );
    }

    json!({
        "ride": {
            "source": a.source,
            "start": chrono::DateTime::from_timestamp(a.t0.trunc() as i64, 0)
                .filter(|_| a.has_absolute_clock())
                .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            "durationSeconds": round(a.cadence.duration_s, 3),
            "distanceMetres": round(a.distance_m, 1),
        },
        "units": {
            "lean": "degrees, signed, positive to the right",
            "speed": "m/s in speedMps, km/h in speedKmh",
            "acceleration": "m/s^2 in accelMps2, g in accelG",
            "position": "decimal degrees",
        },
        "cadence": {
            "measuredHz": opt(a.cadence.hz, 4),
            "medianDtSeconds": round(a.cadence.median_dt, 4),
            "p5DtSeconds": round(a.cadence.p5_dt, 4),
            "p95DtSeconds": round(a.cadence.p95_dt, 4),
            "minDtSeconds": round(a.cadence.min_dt, 4),
            "maxDtSeconds": round(a.cadence.max_dt, 4),
            "regular": a.cadence.regular,
            "gapCount": a.cadence.gaps.len(),
            "gapThresholdSeconds": round(a.cadence.gap_threshold, 4),
            "declaredInMetadata": a.declared_hz.iter()
                .map(|(k, v)| (k.clone(), json!(round(*v, 4))))
                .collect::<Map<String, Value>>(),
            "note": "Cadence is measured from sample timestamps at runtime. Declared rates are \
                     informational only and never used for computation.",
        },
        "quality": {
            "total": a.quality.total,
            "missingLean": a.quality.missing_lean,
            "missingSpeed": a.quality.missing_speed,
            "missingAccel": a.quality.missing_accel,
            "leanSensorLimit": a.quality.lean_limit.map(|l| json!({
                "value": round(l.value, 2), "samples": l.samples
            })),
            "curvesDetected": a.quality.curves_detected,
            "warnings": a.warnings,
        },
        "options": {
            "top": a.options.top,
            "minGapSeconds": a.options.min_gap_s,
            "curveThresholdDeg": a.options.curve_threshold_deg,
            "windowSeconds": a.options.window_s,
            "maxGapFactor": a.options.max_gap_factor,
            "accelCeilingMps2": a.options.accel_ceiling_ms2,
            "accelSource": match a.options.accel_source {
                super::AccelSource::Gps => "gps",
                super::AccelSource::Imu => "imu",
            },
        },
        "metrics": metrics,
    })
}

fn round(v: f64, dp: usize) -> f64 {
    if !v.is_finite() {
        return 0.0;
    }
    let f = 10f64.powi(dp as i32);
    (v * f).round() / f
}

fn opt(v: Option<f64>, dp: usize) -> serde_json::Value {
    match v {
        Some(x) if x.is_finite() => serde_json::json!(round(x, dp)),
        _ => serde_json::Value::Null,
    }
}

/// Used by the CSV cue sheet and anywhere a bare number is wanted.
pub fn num(v: f64, dp: usize) -> String {
    fmt_num(v, dp)
}
