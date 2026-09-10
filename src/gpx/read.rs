//! Pull-parse a GPX file into [`Gpx`].

use std::path::Path;

use chrono::{DateTime, Utc};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use super::{Gpx, TrackPoint};

/// Strip any namespace prefix, leaving the local name.
///
/// Every element match in this parser goes through here. Tools disagree wildly
/// about prefixes for the same namespace — `gpxtpx:`, `gpxx:`, `ns3:`, none at
/// all — and Garmin's TrackPointExtension exists at two different URIs, so
/// matching qualified names would reject perfectly good files.
fn local(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// Parse a timestamp, accepting a trailing `Z` or a numeric offset.
fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Where in the document the parser currently is.
#[derive(Default)]
struct Context {
    in_metadata: bool,
    in_trk: bool,
    /// Waypoints are skipped wholesale: `gmax` writes moments as chapters and
    /// an EDL, not as GPX waypoints, and a waypoint's `<name>`/`<time>` would
    /// otherwise be mistaken for the track's.
    in_wpt: bool,
}

pub fn read_file(path: &Path) -> anyhow::Result<Gpx> {
    let text = std::fs::read_to_string(path)?;
    read_str(&text)
}

pub fn read_str(text: &str) -> anyhow::Result<Gpx> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    let mut gpx = Gpx::default();
    let mut ctx = Context::default();
    let mut point: Option<TrackPoint> = None;
    // Stack of open element local names; its top is the element whose text a
    // `Text` event belongs to.
    let mut open: Vec<Vec<u8>> = Vec::new();
    let mut saw_gpx_root = false;

    loop {
        match reader.read_event()? {
            Event::Eof => break,
            Event::Start(e) => {
                let name = local(e.name().as_ref()).to_vec();
                on_open(&name, &e, &mut gpx, &mut ctx, &mut point, &mut saw_gpx_root)?;
                open.push(name);
            }
            // An empty element opens and closes at once, so it must not be
            // pushed onto the stack.
            Event::Empty(e) => {
                let name = local(e.name().as_ref()).to_vec();
                on_open(&name, &e, &mut gpx, &mut ctx, &mut point, &mut saw_gpx_root)?;
                on_close(&name, &mut gpx, &mut ctx, &mut point);
            }
            Event::End(e) => {
                let name = local(e.name().as_ref()).to_vec();
                on_close(&name, &mut gpx, &mut ctx, &mut point);
                open.pop();
            }
            Event::Text(t) => {
                let text = t.unescape()?.to_string();
                if text.is_empty() {
                    continue;
                }
                let Some(element) = open.last() else { continue };
                on_text(element, &text, &mut gpx, &ctx, &mut point);
            }
            _ => {}
        }
    }

    if !saw_gpx_root {
        anyhow::bail!("not a GPX document: no <gpx> root element");
    }

    // A track whose points are out of order breaks every later assumption —
    // bracketing, interpolation, cadence — so sort defensively rather than
    // trusting the producer.
    gpx.points.sort_by(|a, b| match (a.time, b.time) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => std::cmp::Ordering::Equal,
    });

    Ok(gpx)
}

fn on_open(
    name: &[u8],
    e: &BytesStart,
    gpx: &mut Gpx,
    ctx: &mut Context,
    point: &mut Option<TrackPoint>,
    saw_gpx_root: &mut bool,
) -> anyhow::Result<()> {
    match name {
        b"gpx" => {
            *saw_gpx_root = true;
            for a in e.attributes().flatten() {
                if local(a.key.as_ref()) == b"creator" {
                    gpx.creator = Some(a.unescape_value()?.to_string());
                }
            }
        }
        b"metadata" => ctx.in_metadata = true,
        b"trk" => ctx.in_trk = true,
        b"wpt" => ctx.in_wpt = true,
        b"trkpt" if !ctx.in_wpt => {
            let mut p = TrackPoint::default();
            p.lat = f64::NAN;
            p.lon = f64::NAN;
            for a in e.attributes().flatten() {
                let v = a.unescape_value()?;
                match local(a.key.as_ref()) {
                    b"lat" => p.lat = v.trim().parse().unwrap_or(f64::NAN),
                    b"lon" => p.lon = v.trim().parse().unwrap_or(f64::NAN),
                    _ => {}
                }
            }
            *point = Some(p);
        }
        // This flag appears both as an empty element, where its presence is
        // the value, and as text; handle both spellings of both forms. Note
        // this only fires for `Empty` events and `Start` events whose text then
        // overwrites it, so a `>false<` still reads as false.
        b"atSensorLimit" | b"at_sensor_limit" => {
            if let Some(p) = point.as_mut() {
                p.at_sensor_limit = true;
            }
        }
        _ => {}
    }
    Ok(())
}

fn on_close(name: &[u8], gpx: &mut Gpx, ctx: &mut Context, point: &mut Option<TrackPoint>) {
    match name {
        b"metadata" => ctx.in_metadata = false,
        b"trk" => ctx.in_trk = false,
        b"wpt" => ctx.in_wpt = false,
        b"trkpt" => {
            if let Some(p) = point.take() {
                // A point without coordinates is not a point.
                if p.lat.is_finite() && p.lon.is_finite() {
                    gpx.points.push(p);
                }
            }
        }
        _ => {}
    }
}

fn on_text(
    element: &[u8],
    text: &str,
    gpx: &mut Gpx,
    ctx: &Context,
    point: &mut Option<TrackPoint>,
) {
    let num = || text.trim().parse::<f64>().ok();

    if let Some(p) = point.as_mut() {
        match element {
            b"ele" => p.ele_m = num(),
            b"time" => p.time = parse_time(text),
            b"hdop" => p.hdop = num(),
            b"speed" => p.speed_ms = num(),
            b"course" => p.course_deg = num(),
            // Element names differ by producer, so accept every spelling we
            // know of. `lean`/`accel`/`atSensorLimit` are what
            // motion-record-analysis writes in its `mra:` namespace; the
            // `_deg`/`_ms2`/snake-case forms are what `gmax` writes in `gt:`.
            b"lean" | b"lean_deg" => p.lean_deg = num(),
            b"accel" | b"accel_ms2" => p.accel_ms2 = num(),
            b"gps_source" => p.source = Some(text.to_string()),
            b"video_time_s" => p.video_time_s = num(),
            b"atSensorLimit" | b"at_sensor_limit" => {
                p.at_sensor_limit = matches!(text.trim(), "true" | "1")
            }
            _ => {}
        }
        return;
    }

    // Outside a track point. Waypoints are ignored entirely so their names and
    // times cannot be mistaken for the track's.
    if ctx.in_wpt {
        return;
    }
    match element {
        b"name" if ctx.in_metadata || ctx.in_trk => {
            if gpx.name.is_none() {
                gpx.name = Some(text.to_string());
            }
        }
        b"type" if ctx.in_trk => gpx.activity = Some(text.to_string()),
        b"time" if ctx.in_metadata => gpx.time = parse_time(text),
        _ => {}
    }
}
