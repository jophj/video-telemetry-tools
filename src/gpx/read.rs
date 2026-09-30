//! Pull-parse a GPX file into [`Gpx`].

use std::path::Path;

use chrono::{DateTime, NaiveDateTime, Utc};
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

/// How a timestamp was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeKind {
    /// Carried a `Z` or a numeric offset, so its instant is unambiguous.
    Zoned,
    /// Carried no zone at all, and was read as UTC.
    Naive,
}

/// Parse a timestamp.
///
/// RFC 3339 is what GPX mandates, but real exporters also write times with no
/// zone (`2024-05-01T10:00:00`) or no seconds (`2024-05-01T10:00Z`). Dropping
/// those would silently leave a whole track without a clock, so they are
/// accepted: a zoneless time is read as UTC, which is what GPX specifies, and
/// the caller counts them so the user can be told. Any clock offset that
/// assumption gets wrong is a constant, and a merge's alignment absorbs it.
fn parse_time(s: &str) -> Option<(DateTime<Utc>, TimeKind)> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some((t.with_timezone(&Utc), TimeKind::Zoned));
    }

    // Split off whatever zone designator there is.
    let (body, offset_s, kind) = if let Some(b) = s.strip_suffix(['Z', 'z']) {
        (b, 0, TimeKind::Zoned)
    } else if let Some((b, off)) = split_numeric_offset(s) {
        (b, off, TimeKind::Zoned)
    } else {
        (s, 0, TimeKind::Naive)
    };

    const FORMATS: [&str; 4] = [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
    ];
    let naive = FORMATS
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(body, f).ok())?;
    let utc = naive.and_utc() - chrono::Duration::seconds(offset_s);
    Some((utc, kind))
}

/// Split a trailing `+hh:mm` or `+hhmm` offset off a timestamp, returning the
/// body and the offset in seconds east of UTC.
fn split_numeric_offset(s: &str) -> Option<(&str, i64)> {
    let parse = |sign: u8, hh: &str, mm: &str| -> Option<i64> {
        let (h, m): (i64, i64) = (hh.parse().ok()?, mm.parse().ok()?);
        let sign = if sign == b'-' { -1 } else { 1 };
        Some(sign * (h * 3600 + m * 60))
    };
    let b = s.as_bytes();
    // Only look for an offset after the time part has started, so the dashes
    // of the date are never mistaken for one.
    let t_at = s.find(['T', ' '])?;
    for len in [6usize, 5] {
        if b.len() < t_at + len {
            continue;
        }
        let at = b.len() - len;
        let (sign, rest) = (b[at], &s[at + 1..]);
        if at <= t_at || !(sign == b'+' || sign == b'-') {
            continue;
        }
        let off = match (len, rest.as_bytes().get(2)) {
            (6, Some(b':')) => parse(sign, &rest[..2], &rest[3..]),
            (5, _) if rest.bytes().all(|c| c.is_ascii_digit()) => {
                parse(sign, &rest[..2], &rest[2..])
            }
            _ => None,
        };
        if let Some(off) = off {
            return Some((&s[..at], off));
        }
    }
    None
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
    /// `<trkseg>` elements opened so far, across every track.
    segments_opened: usize,
    /// Point timestamps read without a zone, and ones that could not be read.
    naive_times: usize,
    bad_times: usize,
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
            // CDATA is text by another spelling, and some exporters wrap names
            // and descriptions in it.
            ev @ (Event::Text(_) | Event::CData(_)) => {
                let text = match ev {
                    Event::Text(t) => t.unescape()?.to_string(),
                    Event::CData(c) => c.decode()?.trim().to_string(),
                    _ => unreachable!(),
                };
                if text.is_empty() {
                    continue;
                }
                let Some(element) = open.last() else { continue };
                let parent = open.len().checked_sub(2).map(|i| open[i].as_slice());
                on_text(element, parent, &text, &mut gpx, &mut ctx, &mut point);
            }
            _ => {}
        }
    }

    if !saw_gpx_root {
        anyhow::bail!("not a GPX document: no <gpx> root element");
    }

    // A track whose points are out of order breaks every later assumption —
    // bracketing, interpolation, cadence — so sort defensively rather than
    // trusting the producer. Only the timed points are sorted, among the
    // positions they already occupy: a comparator that called an untimed
    // point equal to everything would not be a total order, and could leave
    // timed points stranded out of order on either side of it.
    let slots: Vec<usize> = (0..gpx.points.len())
        .filter(|&i| gpx.points[i].time.is_some())
        .collect();
    let mut timed: Vec<TrackPoint> = slots.iter().map(|&i| gpx.points[i].clone()).collect();
    timed.sort_by_key(|p| p.time);
    for (i, p) in slots.into_iter().zip(timed) {
        gpx.points[i] = p;
    }

    if ctx.naive_times > 0 {
        gpx.warnings.push(format!(
            "{} point timestamp(s) carried no time zone and were read as UTC",
            ctx.naive_times
        ));
    }
    if ctx.bad_times > 0 {
        gpx.warnings.push(format!(
            "{} point timestamp(s) could not be parsed; those points have no time and cannot \
             be merged or ranked",
            ctx.bad_times
        ));
    }

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
        b"trkseg" => ctx.segments_opened += 1,
        b"trkpt" if !ctx.in_wpt => {
            let mut p = TrackPoint {
                lat: f64::NAN,
                lon: f64::NAN,
                segment: ctx.segments_opened.saturating_sub(1),
                ..Default::default()
            };
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
    parent: Option<&[u8]>,
    text: &str,
    gpx: &mut Gpx,
    ctx: &mut Context,
    point: &mut Option<TrackPoint>,
) {
    let num = || text.trim().parse::<f64>().ok();

    if let Some(p) = point.as_mut() {
        match element {
            b"ele" => p.ele_m = num(),
            b"time" => {
                p.time = match parse_time(text) {
                    Some((t, kind)) => {
                        if kind == TimeKind::Naive {
                            ctx.naive_times += 1;
                        }
                        Some(t)
                    }
                    None => {
                        ctx.bad_times += 1;
                        None
                    }
                }
            }
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
    // Matching on the parent as well as the element matters: `<name>` also
    // appears inside `<author>`, `<copyright>` and `<link>`, and an author's
    // name is not the ride's.
    match (element, parent) {
        (b"name", Some(b"metadata" | b"trk")) => {
            if gpx.name.is_none() {
                gpx.name = Some(text.to_string());
            }
        }
        (b"type", Some(b"trk")) => gpx.activity = Some(text.to_string()),
        (b"time", Some(b"metadata")) => gpx.time = parse_time(text).map(|(t, _)| t),
        // A direct child of `<metadata><extensions>` is one key/value pair,
        // which is how a merge records its audit trail.
        (key, Some(b"extensions")) if ctx.in_metadata && !ctx.in_trk => {
            gpx.metadata
                .push((String::from_utf8_lossy(key).into_owned(), text.to_string()));
        }
        _ => {}
    }
}
