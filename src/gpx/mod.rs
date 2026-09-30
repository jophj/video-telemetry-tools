//! GPX 1.1 reading and writing.
//!
//! The prototypes only ever *wrote* GPX; nothing in them parses it. Reading is
//! what makes the merge feature possible, since an external GPS source arrives
//! as a GPX file.
//!
//! Two things make reading real-world GPX awkward, and both are handled by
//! matching on **local names** rather than qualified ones:
//!
//!   * the same namespace gets different prefixes from different tools
//!     (`gpxtpx:`, `gpxx:`, `ns3:`), and
//!   * Garmin's TrackPointExtension exists in v1 and v2 with different URIs —
//!     the GoPro prototype wrote v1, `motion-record-analysis` writes v2. We
//!     read both and write v2, the only version that defines `speed` and
//!     `course`.
//!
//! Lean angle has no standard home in GPX at all, so it lives in a custom
//! namespace. We read anyone's `lean` element and write our own.

pub mod read;
pub mod write;

use chrono::{DateTime, Utc};

pub use read::read_file;
pub use write::{write_file, WriteOptions};

/// Namespace URIs, as written.
pub mod ns {
    pub const GPX: &str = "http://www.topografix.com/GPX/1/1";
    pub const XSI: &str = "http://www.w3.org/2001/XMLSchema-instance";
    /// Namespace/schema pairs for `xsi:schemaLocation`: GPX itself, and the
    /// Garmin extension our `speed`/`course` live in.
    pub const SCHEMA_LOCATION: &str = "http://www.topografix.com/GPX/1/1 \
         http://www.topografix.com/GPX/1/1/gpx.xsd \
         http://www.garmin.com/xmlschemas/TrackPointExtension/v2 \
         https://www8.garmin.com/xmlschemas/TrackPointExtensionv2.xsd";
    /// Garmin TrackPointExtension v2. Not v1: v1 defines temperature, depth,
    /// heart rate and cadence only, so `speed` and `course` written under it
    /// are elements its schema does not have, and a validating consumer
    /// rejects them.
    pub const GPXTPX: &str = "http://www.garmin.com/xmlschemas/TrackPointExtension/v2";
    /// The v1 URI, which the reader still accepts because older tools —
    /// including the GoPro prototype — wrote it.
    pub const GPXTPX_V1: &str = "http://www.garmin.com/xmlschemas/TrackPointExtension/v1";
    /// `gmax`'s own channels: lean angle and the rest of the rider telemetry.
    pub const GT: &str = "https://github.com/gopro-tools/telemetry/1";
}

/// One point on a track.
#[derive(Debug, Clone, Default)]
pub struct TrackPoint {
    pub lat: f64,
    pub lon: f64,
    pub ele_m: Option<f64>,
    pub time: Option<DateTime<Utc>>,
    pub hdop: Option<f64>,
    /// Metres per second, per the Garmin extension's own units.
    pub speed_ms: Option<f64>,
    /// Degrees true.
    pub course_deg: Option<f64>,
    /// Lean angle in degrees, signed, positive to the right.
    pub lean_deg: Option<f64>,
    /// Longitudinal acceleration in m/s².
    pub accel_ms2: Option<f64>,
    /// Set when the source flagged this lean value as clipped by its sensor.
    pub at_sensor_limit: bool,
    /// Which source this position came from, after a merge.
    pub source: Option<String>,
    /// Seconds from the start of the video this point corresponds to.
    pub video_time_s: Option<f64>,
    /// Which `<trkseg>` this point was read from, counted across every
    /// `<trk>` in the file. A segment boundary is the producer saying the
    /// receiver stopped logging, so interpolation never crosses one and the
    /// writer starts a new segment wherever it changes.
    pub segment: usize,
}

/// A whole track.
#[derive(Debug, Clone, Default)]
pub struct Gpx {
    pub name: Option<String>,
    pub creator: Option<String>,
    /// The GPX `<trk><type>`, e.g. `motorcycling`.
    pub activity: Option<String>,
    pub time: Option<DateTime<Utc>>,
    pub points: Vec<TrackPoint>,
    /// Free-form key/value pairs read from or written to
    /// `<metadata><extensions>`. Reading them back is what lets a merged
    /// track's audit trail survive a round trip.
    pub metadata: Vec<(String, String)>,
    /// Non-fatal problems found while reading, such as timestamps that could
    /// not be parsed. Never written.
    pub warnings: Vec<String>,
}

impl Gpx {
    /// Points carrying a timestamp, which is all a merge can use. In time
    /// order: the reader guarantees it.
    pub fn timed_points(&self) -> Vec<&TrackPoint> {
        self.points.iter().filter(|p| p.time.is_some()).collect()
    }

    /// `(first, last)` timestamps.
    pub fn time_span(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let mut times: Vec<DateTime<Utc>> = self.points.iter().filter_map(|p| p.time).collect();
        if times.is_empty() {
            return None;
        }
        times.sort_unstable();
        Some((times[0], times[times.len() - 1]))
    }

    pub fn has_lean(&self) -> bool {
        self.points.iter().any(|p| p.lean_deg.is_some())
    }

    /// Bounding box as `(min_lat, min_lon, max_lat, max_lon)`.
    pub fn bounds(&self) -> Option<(f64, f64, f64, f64)> {
        let mut it = self.points.iter();
        let first = it.next()?;
        let mut b = (first.lat, first.lon, first.lat, first.lon);
        for p in it {
            b.0 = b.0.min(p.lat);
            b.1 = b.1.min(p.lon);
            b.2 = b.2.max(p.lat);
            b.3 = b.3.max(p.lon);
        }
        Some(b)
    }
}

/// Escape text for use as XML content *or* as an attribute value.
///
/// All five predefined entities, unconditionally. The prototypes escaped three
/// characters in one file and four in another, and neither escaped attribute
/// values at all — which means a ride title containing a quote produced
/// malformed XML.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Format an instant as GPX wants it: ISO-8601 UTC with exactly milliseconds.
pub fn format_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}
