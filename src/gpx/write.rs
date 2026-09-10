//! Write GPX 1.1.
//!
//! Built by string concatenation rather than through an XML writer, because the
//! output has to satisfy constraints an XML writer does not know about: GPX 1.1
//! mandates element order (`<ele>` then `<time>` then `<extensions>`), and the
//! numeric formatting has to be stable so that two runs of `gmax` on the same
//! input produce byte-identical files.

use std::io::{BufWriter, Write};
use std::path::Path;

use crate::math::fmt_num;
use crate::telemetry::Telemetry;

use super::{escape, format_time, ns, Gpx, TrackPoint};

#[derive(Debug, Clone)]
pub struct WriteOptions {
    /// `<trk><type>`.
    pub activity: String,
    /// `<gpx creator=...>`.
    pub creator: String,
    /// Track and metadata name.
    pub name: Option<String>,
    /// Emit the rider telemetry channels in the `gt:` namespace.
    pub extensions: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        WriteOptions {
            activity: "motorcycling".to_string(),
            creator: concat!("gmax ", env!("CARGO_PKG_VERSION")).to_string(),
            name: None,
            extensions: true,
        }
    }
}

pub fn write_file(gpx: &Gpx, path: &Path, opts: &WriteOptions) -> anyhow::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    write_to(gpx, &mut w, opts)
}

pub fn write_to<W: Write>(gpx: &Gpx, w: &mut W, opts: &WriteOptions) -> anyhow::Result<()> {
    let name = opts
        .name
        .clone()
        .or_else(|| gpx.name.clone())
        .unwrap_or_else(|| "GoPro ride".to_string());

    writeln!(w, r#"<?xml version="1.0" encoding="UTF-8"?>"#)?;
    writeln!(w, r#"<gpx version="1.1" creator="{}""#, escape(&opts.creator))?;
    writeln!(w, r#"     xmlns="{}""#, ns::GPX)?;
    writeln!(w, r#"     xmlns:xsi="{}""#, ns::XSI)?;
    writeln!(w, r#"     xmlns:gpxtpx="{}""#, ns::GPXTPX)?;
    writeln!(w, r#"     xmlns:gt="{}""#, ns::GT)?;
    writeln!(w, r#"     xsi:schemaLocation="{}">"#, ns::SCHEMA_LOCATION)?;

    // --- metadata ---------------------------------------------------------
    writeln!(w, "  <metadata>")?;
    writeln!(w, "    <name>{}</name>", escape(&name))?;
    if let Some(t) = gpx.time.or_else(|| gpx.points.first().and_then(|p| p.time)) {
        writeln!(w, "    <time>{}</time>", format_time(t))?;
    }
    if let Some((minlat, minlon, maxlat, maxlon)) = gpx.bounds() {
        writeln!(
            w,
            r#"    <bounds minlat="{}" minlon="{}" maxlat="{}" maxlon="{}"/>"#,
            fmt_num(minlat, 7),
            fmt_num(minlon, 7),
            fmt_num(maxlat, 7),
            fmt_num(maxlon, 7)
        )?;
    }
    if !gpx.metadata.is_empty() {
        writeln!(w, "    <extensions>")?;
        for (k, v) in &gpx.metadata {
            writeln!(
                w,
                "      <gt:{0}>{1}</gt:{0}>",
                sanitize_element_name(k),
                escape(v)
            )?;
        }
        writeln!(w, "    </extensions>")?;
    }
    writeln!(w, "  </metadata>")?;

    // --- track ------------------------------------------------------------
    writeln!(w, "  <trk>")?;
    writeln!(w, "    <name>{}</name>", escape(&name))?;
    let activity = gpx.activity.as_deref().unwrap_or(&opts.activity);
    writeln!(w, "    <type>{}</type>", escape(activity))?;
    writeln!(w, "    <trkseg>")?;
    for p in &gpx.points {
        write_point(w, p, opts.extensions)?;
    }
    writeln!(w, "    </trkseg>")?;
    writeln!(w, "  </trk>")?;
    writeln!(w, "</gpx>")?;
    w.flush()?;
    Ok(())
}

fn write_point<W: Write>(w: &mut W, p: &TrackPoint, extensions: bool) -> anyhow::Result<()> {
    writeln!(
        w,
        r#"      <trkpt lat="{}" lon="{}">"#,
        fmt_num(p.lat, 7),
        fmt_num(p.lon, 7)
    )?;
    // GPX 1.1 fixes this order; a validator rejects any other.
    if let Some(e) = p.ele_m {
        writeln!(w, "        <ele>{}</ele>", fmt_num(e, 1))?;
    }
    if let Some(t) = p.time {
        writeln!(w, "        <time>{}</time>", format_time(t))?;
    }
    if let Some(h) = p.hdop {
        writeln!(w, "        <hdop>{}</hdop>", fmt_num(h, 2))?;
    }

    // Build the extension body first: an empty <extensions> element is legal
    // but pointless, and omitting it keeps a no-telemetry track compact.
    let mut ext = String::new();
    if extensions {
        let mut tpx = String::new();
        if let Some(s) = p.speed_ms {
            tpx.push_str(&format!(
                "            <gpxtpx:speed>{}</gpxtpx:speed>\n",
                fmt_num(s, 3)
            ));
        }
        if let Some(c) = p.course_deg {
            tpx.push_str(&format!(
                "            <gpxtpx:course>{}</gpxtpx:course>\n",
                fmt_num(c, 1)
            ));
        }
        if !tpx.is_empty() {
            ext.push_str("          <gpxtpx:TrackPointExtension>\n");
            ext.push_str(&tpx);
            ext.push_str("          </gpxtpx:TrackPointExtension>\n");
        }
        // Lean angle has no standard home in GPX — Garmin's extension covers
        // speed, course, heart rate and cadence, but nothing for a motorcycle
        // — so it goes in our own namespace.
        if let Some(l) = p.lean_deg {
            ext.push_str(&format!(
                "          <gt:lean_deg>{}</gt:lean_deg>\n",
                fmt_num(l, 2)
            ));
        }
        if p.at_sensor_limit {
            ext.push_str("          <gt:at_sensor_limit>true</gt:at_sensor_limit>\n");
        }
        if let Some(a) = p.accel_ms2 {
            ext.push_str(&format!(
                "          <gt:accel_ms2>{}</gt:accel_ms2>\n",
                fmt_num(a, 3)
            ));
        }
        if let Some(v) = p.video_time_s {
            ext.push_str(&format!(
                "          <gt:video_time_s>{}</gt:video_time_s>\n",
                fmt_num(v, 3)
            ));
        }
        // After a merge this says which receiver each position came from, so a
        // repaired track can be audited point by point.
        if let Some(s) = &p.source {
            ext.push_str(&format!(
                "          <gt:gps_source>{}</gt:gps_source>\n",
                escape(s)
            ));
        }
    }
    if !ext.is_empty() {
        writeln!(w, "        <extensions>")?;
        w.write_all(ext.as_bytes())?;
        writeln!(w, "        </extensions>")?;
    }
    writeln!(w, "      </trkpt>")?;
    Ok(())
}

/// Keep a metadata key usable as an XML element name.
fn sanitize_element_name(k: &str) -> String {
    let mut out = String::with_capacity(k.len());
    for (i, c) in k.chars().enumerate() {
        let ok = c.is_ascii_alphabetic() || c == '_' || (i > 0 && (c.is_ascii_digit() || c == '-' || c == '.'));
        out.push(if ok { c } else { '_' });
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Build a [`Gpx`] from extracted telemetry.
///
/// One point per GPS fix rather than per master sample: the master timeline runs
/// at 29.97 Hz, and writing 46 080 points to describe a 10 Hz receiver would
/// triple the file size to say the same thing three times. Rider channels are
/// sampled from the master timeline at each fix's instant.
pub fn from_telemetry(t: &Telemetry, name: Option<String>) -> Gpx {
    let mut points = Vec::with_capacity(t.valid_fix_count());

    // Walk the master timeline alongside the fixes so each fix picks up the
    // rider channels in effect at its own instant.
    let mut i = 0usize;
    for fix in &t.gps {
        if !fix.valid {
            continue; // never invent a position
        }
        while i + 1 < t.n && t.times[i + 1] <= fix.time {
            i += 1;
        }
        points.push(TrackPoint {
            lat: fix.lat,
            lon: fix.lon,
            ele_m: Some(fix.alt_m),
            time: fix.utc.or_else(|| {
                t.t0.map(|t0| {
                    t0 + chrono::Duration::microseconds((fix.time * 1e6).round() as i64)
                })
            }),
            hdop: fix.dop,
            speed_ms: Some(fix.speed_2d),
            course_deg: fix.course_deg,
            lean_deg: Some(t.lean_deg[i]),
            accel_ms2: t.accel_lon_g.as_ref().map(|v| v[i] * crate::math::G),
            at_sensor_limit: false,
            source: Some(fix.source.as_str().to_string()),
            video_time_s: Some(fix.time),
        });
    }

    Gpx {
        name,
        creator: None,
        activity: None,
        time: points.first().and_then(|p| p.time),
        points,
        metadata: Vec::new(),
    }
}
