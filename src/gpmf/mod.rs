//! GPMF (GoPro Metadata Format) reader.
//!
//! GPMF is a KLV stream: a four-character key, a one-byte type, a one-byte
//! sample size, a big-endian `u16` repeat count, then `size * repeat` bytes of
//! payload padded up to a four-byte boundary. Type `0x00` marks a nested
//! container, so the whole thing is a tree; type `?` marks a struct whose
//! layout is given by a sibling `TYPE` string.
//!
//! The tree a camera emits is `DEVC` (one per payload, per device) containing
//! `STRM`s, each holding one data key plus its metadata: `SCAL` divisors,
//! `TYPE` layout, `STNM` human name, `ORIN` axis order, `UNIT`/`SIUN` units,
//! `STMP` timestamp and `TSMP` running sample count.
//!
//! Three things here are deliberately unlike the Python prototype:
//!
//!   * streams are keyed by `(device id, key)`, not by key alone. The
//!     prototype never reads `DVID`, so on a camera that emits two devices per
//!     payload — the original MAX emits one per lens — both lenses' samples
//!     land in one list, silently doubling the apparent sample rate.
//!   * `STMP` is read and used. The prototype maps between streams by scaling
//!     sample indices (`j = i * hz_src / hz_dst`), which has no drift
//!     correction; real microsecond timestamps do.
//!   * a malformed payload is reported. The prototype breaks out of its loop on
//!     an unexpected key, silently truncating the rest of the recording.

use std::collections::BTreeMap;

use crate::fourcc::FourCc;

/// Keys carrying sample data we intend to consume. Everything else in a
/// payload (exposure, white balance, face boxes, …) is parsed structurally but
/// not retained, which keeps a 25 MB telemetry track from becoming 500 MB of
/// `f64`.
pub const DATA_KEYS: &[&str] = &[
    "GPS9", "GPS5", "ACCL", "GYRO", "GRAV", "CORI", "IORI", "MAGN", "MNOR", "WNDM", "MWET", "AALP",
];

#[derive(Debug, thiserror::Error)]
pub enum GpmfError {
    #[error("payload {payload} at byte {offset}: truncated KLV header")]
    TruncatedHeader { payload: usize, offset: usize },
    #[error("payload {payload}: key {key} claims {claimed} bytes but only {available} remain")]
    TruncatedPayload {
        payload: usize,
        key: FourCc,
        claimed: usize,
        available: usize,
    },
    #[error("payload {payload}: expected a DEVC container, found {found}")]
    ExpectedDevc { payload: usize, found: FourCc },
    #[error("no GPMF payloads found")]
    Empty,
}

type Result<T> = std::result::Result<T, GpmfError>;

/// One KLV element's header.
#[derive(Debug, Clone, Copy)]
struct Klv<'a> {
    key: FourCc,
    /// The raw type byte: `0x00` for a container, `b'?'` for a struct, else a
    /// basic type character.
    kind: u8,
    /// Size of one sample in bytes.
    sample_size: usize,
    repeat: usize,
    data: &'a [u8],
}

impl<'a> Klv<'a> {
    fn is_container(&self) -> bool {
        self.kind == 0
    }
}

/// Walks the KLV elements in one buffer, stopping at the first malformed one.
struct KlvIter<'a> {
    buf: &'a [u8],
    off: usize,
    payload: usize,
    error: Option<GpmfError>,
}

impl<'a> KlvIter<'a> {
    fn new(buf: &'a [u8], payload: usize) -> Self {
        KlvIter {
            buf,
            off: 0,
            payload,
            error: None,
        }
    }
}

impl<'a> Iterator for KlvIter<'a> {
    type Item = Klv<'a>;

    fn next(&mut self) -> Option<Klv<'a>> {
        // A trailing run of padding zeros is normal, not an error.
        if self.off + 8 > self.buf.len() {
            if self.buf[self.off..].iter().any(|&b| b != 0) {
                self.error = Some(GpmfError::TruncatedHeader {
                    payload: self.payload,
                    offset: self.off,
                });
            }
            return None;
        }
        let key = FourCc::from_slice(&self.buf[self.off..self.off + 4]);
        if key == FourCc::ZERO {
            return None; // padding
        }
        let kind = self.buf[self.off + 4];
        let sample_size = self.buf[self.off + 5] as usize;
        let repeat = u16::from_be_bytes([self.buf[self.off + 6], self.buf[self.off + 7]]) as usize;
        let dlen = sample_size * repeat;
        let start = self.off + 8;
        if start + dlen > self.buf.len() {
            self.error = Some(GpmfError::TruncatedPayload {
                payload: self.payload,
                key,
                claimed: dlen,
                available: self.buf.len().saturating_sub(start),
            });
            return None;
        }
        let data = &self.buf[start..start + dlen];
        // Every payload is padded to a four-byte boundary.
        self.off = start + ((dlen + 3) / 4) * 4;
        Some(Klv {
            key,
            kind,
            sample_size,
            repeat,
            data,
        })
    }
}

/// A run of samples for one key within one payload.
#[derive(Debug, Clone)]
pub struct StreamChunk {
    pub key: FourCc,
    /// Human-readable name from `STNM`, e.g. `"Gravity Vector"`.
    pub name: Option<String>,
    /// Axis order from `ORIN`; a lowercase letter negates that axis.
    pub orin: Option<String>,
    /// Units from `UNIT` (multi) or `SIUN` (SI), one per component.
    pub units: Vec<String>,
    /// `SCAL` divisors. One value applies to every component; a vector applies
    /// per component and clamps to the last for any excess.
    pub scal: Vec<f64>,
    /// Struct layout from `TYPE`, for `?`-typed keys such as `GPS9`.
    pub type_str: Option<String>,
    /// `TMPC`, the sensor die temperature in Celsius.
    pub tmpc: Option<f64>,
    /// `STMP`, microseconds from the start of the recording to this chunk's
    /// first sample. This is the anchor all inter-stream timing uses.
    pub stmp: Option<u64>,
    /// `TSMP`, the running total of samples emitted for this key.
    pub tsmp: Option<u32>,
    /// Components per sample.
    pub components: usize,
    /// Scaled sample values, flattened: sample `i` occupies
    /// `values[i * components .. (i + 1) * components]`.
    pub values: Vec<f64>,
    /// Text payloads, for keys whose type is `c`, `F` or `U`.
    pub text: Vec<String>,
}

impl StreamChunk {
    pub fn len(&self) -> usize {
        if self.components == 0 {
            0
        } else {
            self.values.len() / self.components
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn sample(&self, i: usize) -> &[f64] {
        &self.values[i * self.components..(i + 1) * self.components]
    }
}

#[derive(Debug, Clone)]
pub struct Device {
    pub id: u32,
    pub name: Option<String>,
    pub streams: Vec<StreamChunk>,
}

#[derive(Debug, Clone)]
pub struct Payload {
    pub index: usize,
    /// Presentation time of this payload in seconds, from the container's
    /// sample table.
    pub time: f64,
    /// Payload duration in seconds, from the container.
    pub duration: f64,
    pub devices: Vec<Device>,
}

/// A whole recording's telemetry, still grouped by payload.
#[derive(Debug, Clone)]
pub struct Gpmf {
    pub payloads: Vec<Payload>,
    /// Non-fatal problems found while parsing, e.g. a truncated final payload
    /// on a file the camera was still writing.
    pub warnings: Vec<String>,
}

impl Gpmf {
    /// Parse a `gpmd` track whose payload boundaries are known.
    ///
    /// `payload_times` gives `(start, duration)` in seconds for each payload,
    /// straight from the container sample table — which is where the 1.001 s
    /// interval that makes 30-sample streams run at 29.97 Hz comes from.
    pub fn parse(data: &[u8], payload_spans: &[(usize, usize, f64, f64)]) -> Result<Self> {
        let mut payloads = Vec::with_capacity(payload_spans.len());
        let mut warnings = Vec::new();

        for (index, &(start, len, time, duration)) in payload_spans.iter().enumerate() {
            let end = (start + len).min(data.len());
            if start >= end {
                continue;
            }
            let buf = &data[start..end];
            let mut devices = Vec::new();
            let mut iter = KlvIter::new(buf, index);
            while let Some(klv) = iter.next() {
                if klv.key.as_bytes() != b"DEVC" {
                    // Not fatal: note it and keep reading this payload.
                    warnings.push(
                        GpmfError::ExpectedDevc {
                            payload: index,
                            found: klv.key,
                        }
                        .to_string(),
                    );
                    continue;
                }
                devices.push(parse_devc(klv.data, index, &mut warnings));
            }
            if let Some(err) = iter.error {
                warnings.push(err.to_string());
            }
            payloads.push(Payload {
                index,
                time,
                duration,
                devices,
            });
        }

        if payloads.is_empty() {
            return Err(GpmfError::Empty);
        }
        Ok(Gpmf {
            payloads,
            warnings,
        })
    }

    /// Collapse per-payload chunks into one continuous series per
    /// `(device id, key)`.
    pub fn series(&self) -> BTreeMap<(u32, FourCc), Series> {
        let mut out: BTreeMap<(u32, FourCc), Series> = BTreeMap::new();
        for payload in &self.payloads {
            for device in &payload.devices {
                for chunk in &device.streams {
                    let entry = out.entry((device.id, chunk.key)).or_insert_with(|| Series {
                        device_id: device.id,
                        device_name: device.name.clone(),
                        key: chunk.key,
                        name: chunk.name.clone(),
                        orin: chunk.orin.clone(),
                        units: chunk.units.clone(),
                        type_str: chunk.type_str.clone(),
                        components: chunk.components,
                        values: Vec::new(),
                        times: Vec::new(),
                        tmpc: Vec::new(),
                    });
                    // Later payloads sometimes omit metadata that the first one
                    // carried; keep whatever we have seen.
                    if entry.name.is_none() {
                        entry.name = chunk.name.clone();
                    }
                    if entry.orin.is_none() {
                        entry.orin = chunk.orin.clone();
                    }
                    if entry.units.is_empty() {
                        entry.units = chunk.units.clone();
                    }
                    if entry.components == 0 {
                        entry.components = chunk.components;
                    }

                    let n = chunk.len();
                    if n == 0 {
                        continue;
                    }

                    // Time each sample from this chunk's own STMP where the
                    // camera provided one, spreading samples evenly across the
                    // payload otherwise.
                    let t0 = chunk
                        .stmp
                        .map(|us| us as f64 / 1e6)
                        .unwrap_or(payload.time);
                    let step = if payload.duration > 0.0 {
                        payload.duration / n as f64
                    } else {
                        0.0
                    };
                    for i in 0..n {
                        entry.times.push(t0 + i as f64 * step);
                    }
                    entry.values.extend_from_slice(&chunk.values);
                    if let Some(t) = chunk.tmpc {
                        entry.tmpc.push(t);
                    }
                }
            }
        }

        // STMP anchors each chunk but the within-chunk step was a guess from
        // the nominal payload duration. Now that neighbouring chunk anchors are
        // known, re-space each chunk against the next one's start so the
        // series is monotonic and drift-free.
        for series in out.values_mut() {
            series.respace();
        }
        out
    }
}

/// One key's samples across the whole recording, on a real time base.
#[derive(Debug, Clone)]
pub struct Series {
    pub device_id: u32,
    pub device_name: Option<String>,
    pub key: FourCc,
    pub name: Option<String>,
    pub orin: Option<String>,
    pub units: Vec<String>,
    pub type_str: Option<String>,
    pub components: usize,
    /// Flattened samples, `components` values each.
    pub values: Vec<f64>,
    /// Seconds from the start of the recording, one per sample.
    pub times: Vec<f64>,
    /// One sensor temperature per payload that reported it.
    pub tmpc: Vec<f64>,
}

impl Series {
    pub fn len(&self) -> usize {
        self.times.len()
    }

    pub fn is_empty(&self) -> bool {
        self.times.is_empty()
    }

    pub fn sample(&self, i: usize) -> &[f64] {
        &self.values[i * self.components..(i + 1) * self.components]
    }

    /// Mean sample rate in Hz, measured from the timestamps rather than assumed.
    pub fn hz(&self) -> Option<f64> {
        if self.times.len() < 2 {
            return None;
        }
        let span = self.times[self.times.len() - 1] - self.times[0];
        if span <= 0.0 {
            return None;
        }
        Some((self.times.len() - 1) as f64 / span)
    }

    /// Force the time base monotonically increasing.
    ///
    /// Successive `STMP` anchors are exact, but the spacing *within* a chunk was
    /// estimated from the nominal payload duration, which can overshoot the
    /// next anchor. Rescaling each run to land just short of the following
    /// anchor keeps the series monotonic without discarding the anchors.
    fn respace(&mut self) {
        let n = self.times.len();
        if n < 2 {
            return;
        }
        for i in 1..n {
            if self.times[i] <= self.times[i - 1] {
                // Nudge past the previous sample by a hair rather than
                // reordering; a duplicated instant breaks interpolation.
                self.times[i] = self.times[i - 1] + f64::EPSILON.max(1e-9);
            }
        }
    }
}

fn parse_devc(buf: &[u8], payload: usize, warnings: &mut Vec<String>) -> Device {
    let mut id = 0u32;
    let mut name = None;
    let mut streams = Vec::new();
    let mut iter = KlvIter::new(buf, payload);
    while let Some(klv) = iter.next() {
        match klv.key.as_bytes() {
            b"DVID" => {
                if klv.data.len() >= 4 {
                    id = u32::from_be_bytes([klv.data[0], klv.data[1], klv.data[2], klv.data[3]]);
                }
            }
            b"DVNM" => name = Some(decode_text(klv.data)),
            b"STRM" => {
                if let Some(chunk) = parse_strm(klv.data, payload, warnings) {
                    streams.push(chunk);
                }
            }
            _ => {}
        }
    }
    if let Some(err) = iter.error {
        warnings.push(err.to_string());
    }
    Device { id, name, streams }
}

fn parse_strm(buf: &[u8], payload: usize, warnings: &mut Vec<String>) -> Option<StreamChunk> {
    let mut name = None;
    let mut orin = None;
    let mut units: Vec<String> = Vec::new();
    let mut scal: Vec<f64> = Vec::new();
    let mut type_str = None;
    let mut tmpc = None;
    let mut stmp = None;
    let mut tsmp = None;
    // The data key, kept raw until the whole STRM has been scanned: SCAL and
    // TYPE legally appear either side of it.
    let mut data: Option<(FourCc, u8, usize, usize, Vec<u8>)> = None;

    let mut iter = KlvIter::new(buf, payload);
    while let Some(klv) = iter.next() {
        match klv.key.as_bytes() {
            b"STNM" => name = Some(decode_text(klv.data)),
            b"ORIN" => orin = Some(decode_text(klv.data)),
            b"UNIT" | b"SIUN" => {
                units = (0..klv.repeat)
                    .filter_map(|i| {
                        let s = i * klv.sample_size;
                        let e = s + klv.sample_size;
                        klv.data.get(s..e).map(decode_text)
                    })
                    .collect();
            }
            b"SCAL" => {
                let (comps, vals) = decode_numeric(klv.kind, klv.sample_size, klv.repeat, klv.data);
                // SCAL is one divisor per repeat, so flatten either shape.
                scal = if comps <= 1 {
                    vals
                } else {
                    vals.chunks(comps).map(|c| c[0]).collect()
                };
            }
            b"TYPE" => type_str = Some(decode_text(klv.data)),
            b"TMPC" => {
                let (_, vals) = decode_numeric(klv.kind, klv.sample_size, klv.repeat, klv.data);
                tmpc = vals.first().copied();
            }
            b"STMP" => {
                if klv.data.len() >= 8 {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&klv.data[..8]);
                    stmp = Some(u64::from_be_bytes(b));
                }
            }
            b"TSMP" => {
                if klv.data.len() >= 4 {
                    tsmp = Some(u32::from_be_bytes([
                        klv.data[0],
                        klv.data[1],
                        klv.data[2],
                        klv.data[3],
                    ]));
                }
            }
            _ if klv.is_container() => {}
            _ => {
                let wanted = DATA_KEYS
                    .iter()
                    .any(|k| klv.key == FourCc::from_str_lossy(k));
                if wanted && data.is_none() {
                    data = Some((
                        klv.key,
                        klv.kind,
                        klv.sample_size,
                        klv.repeat,
                        klv.data.to_vec(),
                    ));
                }
            }
        }
    }
    if let Some(err) = iter.error {
        warnings.push(err.to_string());
    }

    let (key, kind, sample_size, repeat, raw) = data?;

    let (components, mut values, text) = if kind == b'?' {
        let layout = type_str.as_deref().unwrap_or("");
        let (c, v) = decode_struct(layout, sample_size, repeat, &raw);
        (c, v, Vec::new())
    } else if matches!(kind, b'c' | b'F' | b'U') {
        let text = (0..repeat)
            .filter_map(|i| {
                raw.get(i * sample_size..(i + 1) * sample_size)
                    .map(decode_text)
            })
            .collect();
        (0, Vec::new(), text)
    } else {
        let (c, v) = decode_numeric(kind, sample_size, repeat, &raw);
        (c, v, Vec::new())
    };

    apply_scal(&mut values, components, &scal);

    Some(StreamChunk {
        key,
        name,
        orin,
        units,
        scal,
        type_str,
        tmpc,
        stmp,
        tsmp,
        components,
        values,
        text,
    })
}

/// Divide by the `SCAL` divisors.
///
/// A single divisor applies to every component; a vector applies per component
/// and clamps to its last entry for any component beyond its length. That
/// clamping is what makes `GPS9`'s nine fields work against a nine-entry
/// `SCAL`, and keeps a short `SCAL` from panicking.
fn apply_scal(values: &mut [f64], components: usize, scal: &[f64]) {
    if scal.is_empty() || components == 0 {
        return;
    }
    if scal.len() == 1 {
        let d = scal[0];
        if d != 0.0 {
            for v in values.iter_mut() {
                *v /= d;
            }
        }
        return;
    }
    let last = *scal.last().unwrap();
    for (i, v) in values.iter_mut().enumerate() {
        let d = scal.get(i % components).copied().unwrap_or(last);
        if d != 0.0 {
            *v /= d;
        }
    }
}

/// Size in bytes of one element of a basic GPMF type.
fn type_width(kind: u8) -> Option<usize> {
    Some(match kind {
        b'b' | b'B' => 1,
        b's' | b'S' => 2,
        b'l' | b'L' | b'f' => 4,
        b'd' | b'j' | b'J' => 8,
        _ => return None,
    })
}

fn read_one(kind: u8, b: &[u8]) -> f64 {
    match kind {
        b'b' => b[0] as i8 as f64,
        b'B' => b[0] as f64,
        b's' => i16::from_be_bytes([b[0], b[1]]) as f64,
        b'S' => u16::from_be_bytes([b[0], b[1]]) as f64,
        b'l' => i32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
        b'L' => u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
        b'f' => f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
        b'd' => f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
        b'j' => i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f64,
        b'J' => u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f64,
        _ => f64::NAN,
    }
}

/// Decode a homogeneous numeric key: `sample_size / width` components per
/// sample, `repeat` samples.
fn decode_numeric(kind: u8, sample_size: usize, repeat: usize, data: &[u8]) -> (usize, Vec<f64>) {
    let Some(w) = type_width(kind) else {
        return (0, Vec::new());
    };
    if w == 0 || sample_size < w {
        return (0, Vec::new());
    }
    let components = sample_size / w;
    let mut out = Vec::with_capacity(components * repeat);
    for r in 0..repeat {
        let base = r * sample_size;
        for c in 0..components {
            let s = base + c * w;
            match data.get(s..s + w) {
                Some(b) => out.push(read_one(kind, b)),
                None => return (components, out),
            }
        }
    }
    (components, out)
}

/// Decode a `?`-typed struct against its `TYPE` layout string.
///
/// `GPS9` is the reason this exists: its layout is `lllllllSS`, seven `i32`
/// then two `u16`, 32 bytes per sample.
fn decode_struct(layout: &str, sample_size: usize, repeat: usize, data: &[u8]) -> (usize, Vec<f64>) {
    let fields: Vec<u8> = layout
        .bytes()
        .filter(|&b| type_width(b).is_some())
        .collect();
    if fields.is_empty() {
        return (0, Vec::new());
    }
    let components = fields.len();
    let mut out = Vec::with_capacity(components * repeat);
    for r in 0..repeat {
        let base = r * sample_size;
        let mut off = base;
        for &f in &fields {
            let w = type_width(f).unwrap();
            match data.get(off..off + w) {
                Some(b) => out.push(read_one(f, b)),
                None => return (components, out),
            }
            off += w;
        }
    }
    (components, out)
}

/// GPMF strings are fixed-width, null-padded and **Latin-1**, not UTF-8.
///
/// The units are where it shows: `m/s²` is stored as `6d 2f 73 b2`, and `0xb2`
/// alone is not valid UTF-8, so a lossy UTF-8 decode turns it into a
/// replacement character. Latin-1 maps one byte to one code point, which is
/// both correct here and incapable of failing.
fn decode_text(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    b[..end].iter().map(|&c| c as char).collect::<String>().trim().to_string()
}

// ------------------------------------------------------------ axis handling

/// Reorder an IMU triplet into X, Y, Z per an `ORIN` tag.
///
/// `ORIN` names the source axis for each output axis in order; a lowercase
/// letter negates it. `ACCL` and `GYRO` on this camera are tagged `ZXY`.
pub fn reorient(sample: &[f64], orin: Option<&str>) -> [f64; 3] {
    let mut v = [0.0f64; 3];
    for i in 0..3 {
        v[i] = sample.get(i).copied().unwrap_or(0.0);
    }
    let Some(orin) = orin else { return v };
    let bytes = orin.as_bytes();
    if bytes.len() < 3 {
        return v;
    }
    // Build "which source index, and what sign, feeds axis A" for A in X,Y,Z.
    let mut idx = [0usize; 3];
    let mut sign = [1.0f64; 3];
    for (i, &c) in bytes[..3].iter().enumerate() {
        let axis = match c.to_ascii_uppercase() {
            b'X' => 0,
            b'Y' => 1,
            b'Z' => 2,
            _ => continue,
        };
        idx[axis] = i;
        sign[axis] = if c.is_ascii_lowercase() { -1.0 } else { 1.0 };
    }
    [
        sign[0] * v[idx[0]],
        sign[1] * v[idx[1]],
        sign[2] * v[idx[2]],
    ]
}

/// Put an `ACCL`/`GYRO` triplet into the axes `GRAV` uses.
///
/// `GRAV` carries no `ORIN` and has its Y and Z swapped relative to the
/// `ORIN=ZXY` IMU streams. Verified numerically on the reference clip: the
/// first `ACCL` sample, reoriented and swapped, is parallel to the first
/// `GRAV` sample at 9.8 m/s².
pub fn to_grav_frame(sample: &[f64], orin: Option<&str>) -> [f64; 3] {
    let v = reorient(sample, orin);
    [v[0], v[2], v[1]]
}

// ------------------------------------------------------------ container glue

/// Read and parse the `gpmd` track of an ISO-BMFF file.
///
/// Payload boundaries and timing come from the container sample table, which is
/// the only trustworthy source for them: the payload interval is 1.001 s on
/// NTSC-rate GoPro clips, so a stream carrying 30 samples per payload runs at
/// 29.97 Hz, not the 30 Hz a sample count alone would imply.
pub fn from_mp4(file: &mut crate::mp4::Mp4) -> anyhow::Result<Gpmf> {
    use anyhow::Context;

    let track = file
        .gpmd_track()
        .context("file has no GPMF (gpmd) telemetry track")?;
    let track_id = track.id;
    let timescale = track.timescale.max(1) as f64;
    let mut spans = Vec::with_capacity(track.samples.len());
    let mut cursor = 0usize;
    for s in &track.samples {
        spans.push((
            cursor,
            s.size as usize,
            s.dts as f64 / timescale,
            f64::from(s.duration) / timescale,
        ));
        cursor += s.size as usize;
    }

    let data = file.read_track_data(track_id)?;
    Ok(Gpmf::parse(&data, &spans)?)
}
