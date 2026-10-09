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

use std::collections::{BTreeMap, BTreeSet};

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
    #[error(
        "payload {payload} at byte {offset}: data follows the zero padding that should end it"
    )]
    DataAfterPadding { payload: usize, offset: usize },
    #[error("payload {payload}: expected a DEVC container, found {found}")]
    ExpectedDevc { payload: usize, found: FourCc },
    #[error("no GPMF payloads found")]
    Empty,
}

impl GpmfError {
    /// What went wrong, without saying where: the part two occurrences of
    /// the same fault in different payloads have in common.
    fn kind(&self) -> &'static str {
        match self {
            GpmfError::TruncatedHeader { .. } => "truncated KLV header",
            GpmfError::TruncatedPayload { .. } => "truncated KLV data",
            GpmfError::DataAfterPadding { .. } => "data after padding",
            GpmfError::ExpectedDevc { .. } => "not a DEVC",
            GpmfError::Empty => "empty",
        }
    }
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
        // `off` can sit past the end: the last element's padding is implied,
        // not always written, so rounding up from an unpadded final element
        // overshoots. `get` treats that as the clean end it is.
        let rest = self.buf.get(self.off..).unwrap_or(&[]);
        // A trailing run of padding zeros is normal, not an error.
        if rest.len() < 8 {
            if rest.iter().any(|&b| b != 0) {
                self.error = Some(GpmfError::TruncatedHeader {
                    payload: self.payload,
                    offset: self.off,
                });
            }
            return None;
        }
        let key = FourCc::from_slice(&rest[..4]);
        if key == FourCc::ZERO {
            // Padding, which runs to the end. Anything non-zero after it is a
            // corrupt key rather than padding, and would otherwise vanish
            // without a word along with everything behind it.
            if let Some(i) = rest.iter().position(|&b| b != 0) {
                self.error = Some(GpmfError::DataAfterPadding {
                    payload: self.payload,
                    offset: self.off + i,
                });
            }
            return None;
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
        self.off = start + dlen.div_ceil(4) * 4;
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
        self.values.len().checked_div(self.components).unwrap_or(0)
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
        let mut context = StreamContext::default();

        for (index, &(start, len, time, duration)) in payload_spans.iter().enumerate() {
            let end = start.saturating_add(len).min(data.len());
            if start >= end {
                continue;
            }
            let buf = &data[start..end];
            let mut devices = Vec::new();
            let mut iter = KlvIter::new(buf, index);
            for klv in iter.by_ref() {
                if klv.key.as_bytes() != b"DEVC" {
                    // Not fatal: note it and keep reading this payload.
                    context.fault(
                        "GPMF",
                        GpmfError::ExpectedDevc {
                            payload: index,
                            found: klv.key,
                        },
                        &mut warnings,
                    );
                    continue;
                }
                devices.push(parse_devc(klv.data, index, &mut context, &mut warnings));
            }
            if let Some(err) = iter.error {
                context.fault("GPMF", err, &mut warnings);
            }
            payloads.push(Payload {
                index,
                time,
                duration,
                devices,
            });
        }

        context.finish(&mut warnings);

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
        self.collect_series(|_, _| true)
    }

    /// One `(device id, key)`'s series alone.
    ///
    /// For a caller that needs one stream: [`series`](Self::series) builds
    /// every one, and on a camera whose gyro runs at 800 Hz that is most of
    /// the work and most of the memory.
    pub fn series_of(&self, device: u32, key: FourCc) -> Option<Series> {
        self.collect_series(|d, k| d == device && k == key)
            .into_values()
            .next()
    }

    /// How many samples each `(device id, key)` carries, without building any
    /// series.
    pub fn sample_counts(&self) -> BTreeMap<(u32, FourCc), usize> {
        let mut out: BTreeMap<(u32, FourCc), usize> = BTreeMap::new();
        for device in self.payloads.iter().flat_map(|p| &p.devices) {
            for chunk in &device.streams {
                *out.entry((device.id, chunk.key)).or_default() += chunk.len();
            }
        }
        out
    }

    /// The name a device gives itself in `DVNM`, from the first payload that
    /// carries one.
    pub fn device_name(&self, device: u32) -> Option<String> {
        self.payloads
            .iter()
            .flat_map(|p| &p.devices)
            .filter(|d| d.id == device)
            .find_map(|d| d.name.clone())
    }

    fn collect_series(
        &self,
        wanted: impl Fn(u32, FourCc) -> bool,
    ) -> BTreeMap<(u32, FourCc), Series> {
        let mut out: BTreeMap<(u32, FourCc), Series> = BTreeMap::new();
        let mut spans: BTreeMap<(u32, FourCc), Vec<ChunkSpan>> = BTreeMap::new();
        for payload in &self.payloads {
            for device in &payload.devices {
                for chunk in device.streams.iter().filter(|c| wanted(device.id, c.key)) {
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
                    // Values are flattened, so one chunk of a different width
                    // would shift every later sample onto the wrong fields.
                    // `parse` has already warned about it.
                    if chunk.components != entry.components {
                        continue;
                    }

                    spans
                        .entry((device.id, chunk.key))
                        .or_default()
                        .push(ChunkSpan {
                            stmp: chunk.stmp.map(|us| us as f64 / 1e6),
                            payload_time: payload.time,
                            payload_duration: payload.duration,
                            len: n,
                        });
                    entry.values.extend_from_slice(&chunk.values);
                    if let Some(t) = chunk.tmpc {
                        entry.tmpc.push(t);
                    }
                }
            }
        }

        for (key, series) in out.iter_mut() {
            if let Some(spans) = spans.get(key) {
                series.times = lay_out_times(spans);
            }
        }
        out
    }
}

/// Where one chunk's samples sit, before they are given times.
struct ChunkSpan {
    /// `STMP` in seconds, when the chunk carried one.
    stmp: Option<f64>,
    payload_time: f64,
    payload_duration: f64,
    len: usize,
}

/// Give every sample of a series a time.
///
/// Each chunk starts at its own `STMP` anchor, and a chunk without one is put
/// on the same clock by carrying over the offset between `STMP` and container
/// time from the nearest chunk that had one — mixing the two clocks raw would
/// jump by however far apart they are. Within a chunk, samples are spread at
/// the nominal payload duration over the sample count, and that estimate can
/// overshoot the next chunk's anchor: a 200 Hz sensor does not deliver exactly
/// 200.2 samples every 1.001 s. Where it would, the chunk is rescaled to end
/// one sample short of the next anchor, so the series is monotonic and evenly
/// spaced rather than bunched against the boundary.
///
/// `STMP` is taken as an absolute position on the timeline, not re-based to
/// the first payload. That is deliberate: a clip stream-copied out of a
/// recording keeps its parent's `STMP` values while its container clock
/// restarts at zero, and matching the two sample for sample is what lets a
/// clip borrow its parent's calibration (see `telemetry::timeline`). The cost
/// is that a file whose camera does not reset `STMP` per file — a continuation
/// chapter, `GS02…`, is expected to be one — has a timeline that does not
/// start near zero. Everything downstream measures from `times[0]`, which is
/// what keeps that harmless.
fn lay_out_times(spans: &[ChunkSpan]) -> Vec<f64> {
    // The STMP-minus-container-time offset to put each chunk on the STMP
    // clock: its own where it has one, else the last seen, else the first
    // one to come. With no STMP anywhere this is zero and container time is
    // the clock.
    let first_offset = spans
        .iter()
        .find_map(|s| s.stmp.map(|t| t - s.payload_time))
        .unwrap_or(0.0);
    let mut offset = first_offset;
    let anchors: Vec<f64> = spans
        .iter()
        .map(|s| match s.stmp {
            Some(t) => {
                offset = t - s.payload_time;
                t
            }
            None => s.payload_time + offset,
        })
        .collect();

    let mut times = Vec::with_capacity(spans.iter().map(|s| s.len).sum());
    for (k, span) in spans.iter().enumerate() {
        let n = span.len as f64;
        let t0 = anchors[k];
        let mut step = if span.payload_duration > 0.0 {
            span.payload_duration / n
        } else {
            0.0
        };
        if let Some(&next) = anchors.get(k + 1) {
            // The tolerance keeps float noise in exactly-fitting chunks, such
            // as the frame-rate streams, from rescaling them by a hair.
            if next > t0 && (step == 0.0 || t0 + n * step > next + 1e-9) {
                step = (next - t0) / n;
            }
        }
        for i in 0..span.len {
            times.push(t0 + i as f64 * step);
        }
    }

    // An anchor that goes backwards cannot be rescaled around. Nudge past
    // the previous sample by a hair rather than reordering; a duplicated
    // instant breaks interpolation.
    for i in 1..times.len() {
        if times[i] <= times[i - 1] {
            times[i] = times[i - 1] + 1e-9;
        }
    }
    times
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
}

/// What one payload's streams leave for the next.
///
/// `SCAL` and `TYPE` describe a stream, not a payload, and a camera is free to
/// send them only when they change. Each payload is decoded against the last
/// ones seen for its `(device, key)`, or a later payload that omits them would
/// come out unscaled, or — for a struct with no layout — not at all.
#[derive(Default)]
struct StreamContext {
    streams: BTreeMap<(u32, FourCc), StreamState>,
    /// Warnings already given, so a fault repeated in every payload of a long
    /// recording is reported once rather than fifteen hundred times.
    warned: BTreeSet<String>,
    /// Malformed KLV seen so far, by kind and place: where its warning sits in
    /// the list, and how many times it has happened.
    faults: BTreeMap<String, (usize, usize)>,
}

#[derive(Default)]
struct StreamState {
    scal: Vec<f64>,
    type_str: Option<String>,
    /// Components per sample, fixed by the first chunk that had any.
    components: usize,
}

impl StreamContext {
    fn warn_once(&mut self, warning: String, warnings: &mut Vec<String>) {
        self.warn_once_as(warning.clone(), warning, warnings);
    }

    /// Warn unless a warning with the same `gist` was already given, for
    /// messages that name the payload they were first seen in.
    fn warn_once_as(&mut self, gist: String, warning: String, warnings: &mut Vec<String>) {
        if self.warned.insert(gist) {
            warnings.push(warning);
        }
    }

    /// Report malformed KLV found in `scope`, once per kind of fault there.
    ///
    /// A camera that writes one stream wrongly writes it wrongly in every
    /// payload, and each occurrence names its own payload and offset, so
    /// without this a damaged stream buried every other warning under fifteen
    /// hundred near-identical ones. The first occurrence is kept as the
    /// example, since its payload and offset are where to look; the rest are
    /// counted, and [`finish`](Self::finish) adds the count.
    fn fault(&mut self, scope: &str, err: GpmfError, warnings: &mut Vec<String>) {
        let gist = format!("{scope}: {}", err.kind());
        match self.faults.get_mut(&gist) {
            Some((_, count)) => *count += 1,
            None => {
                self.faults.insert(gist, (warnings.len(), 1));
                warnings.push(format!("{scope}: {err}"));
            }
        }
    }

    /// Say how often each fault recurred, on the warning that first reported it.
    fn finish(&self, warnings: &mut [String]) {
        for &(at, count) in self.faults.values() {
            if count > 1 {
                if let Some(w) = warnings.get_mut(at) {
                    w.push_str(&format!(" ({count} payloads in all)"));
                }
            }
        }
    }
}

fn parse_devc(
    buf: &[u8],
    payload: usize,
    context: &mut StreamContext,
    warnings: &mut Vec<String>,
) -> Device {
    let mut id = 0u32;
    let mut name = None;
    let mut streams = Vec::new();
    let mut iter = KlvIter::new(buf, payload);
    for klv in iter.by_ref() {
        match klv.key.as_bytes() {
            b"DVID" => {
                if klv.data.len() >= 4 {
                    id = u32::from_be_bytes([klv.data[0], klv.data[1], klv.data[2], klv.data[3]]);
                }
            }
            b"DVNM" => name = Some(decode_text(klv.data)),
            b"STRM" => {
                if let Some(chunk) = parse_strm(klv.data, payload, id, context, warnings) {
                    streams.push(chunk);
                }
            }
            _ => {}
        }
    }
    if let Some(err) = iter.error {
        context.fault(&format!("device {id}"), err, warnings);
    }
    Device { id, name, streams }
}

fn parse_strm(
    buf: &[u8],
    payload: usize,
    device: u32,
    context: &mut StreamContext,
    warnings: &mut Vec<String>,
) -> Option<StreamChunk> {
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
    for klv in iter.by_ref() {
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
                // SCAL is a list of divisors however it is shaped: N repeats of
                // one value, or one sample of N components. Both flatten to the
                // same N. Keeping only each sample's first component made the
                // second shape a single divisor applied to every field.
                let (_, vals) = decode_numeric(klv.kind, klv.sample_size, klv.repeat, klv.data);
                scal = vals;
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
        // Named by its data key when the damage came after it, which is what
        // tells one broken stream from another on the same device.
        let scope = match &data {
            Some((key, ..)) => format!("device {device} {key}"),
            None => format!("device {device} stream"),
        };
        context.fault(&scope, err, warnings);
    }

    let (key, kind, sample_size, repeat, raw) = data?;

    // Fill in what this payload left out from what the stream said before,
    // and remember what it did say for the payloads after.
    let state = context.streams.entry((device, key)).or_default();
    if scal.is_empty() {
        scal = state.scal.clone();
    } else {
        state.scal = scal.clone();
    }
    match &type_str {
        None => type_str = state.type_str.clone(),
        Some(t) => state.type_str = Some(t.clone()),
    }

    let (components, mut values, text) = if kind == b'?' {
        match type_str
            .as_deref()
            .map(|layout| decode_struct(layout, sample_size, repeat, &raw))
        {
            Some(Ok((c, v))) => (c, v, Vec::new()),
            Some(Err(why)) => {
                context.warn_once(format!("{key}: {why}; its samples are skipped"), warnings);
                (0, Vec::new(), Vec::new())
            }
            None => {
                context.warn_once(
                    format!("{key}: a struct with no TYPE layout; its samples are skipped"),
                    warnings,
                );
                (0, Vec::new(), Vec::new())
            }
        }
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

    // Series are flattened, so a change of width mid-recording cannot be
    // followed. The first width seen wins and `Gpmf::series` drops the rest.
    let state = context.streams.entry((device, key)).or_default();
    if state.components == 0 {
        state.components = components;
    } else if components != 0 && components != state.components {
        let established = state.components;
        context.warn_once_as(
            format!("{key} width {components} after {established}"),
            format!(
                "{key}: from payload {payload}, some chunks have {components} values per sample \
                 where the first had {established}; they are skipped"
            ),
            warnings,
        );
    }

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
        b'l' | b'L' | b'f' | b'q' => 4,
        b'd' | b'j' | b'J' | b'Q' => 8,
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
        // Signed fixed point: Q15.16 and Q31.32.
        b'q' => i32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64 / 65_536.0,
        b'Q' => {
            i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f64
                / 4_294_967_296.0
        }
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
                None => {
                    // Keep whole samples only, or the partial one would shift
                    // every sample appended after it.
                    out.truncate(out.len() / components * components);
                    return (components, out);
                }
            }
        }
    }
    (components, out)
}

/// Decode a `?`-typed struct against its `TYPE` layout string.
///
/// `GPS9` is the reason this exists: its layout is `lllllllSS`, seven `i32`
/// then two `u16`, 32 bytes per sample. Only numeric fields become
/// components, but every field takes up room, so text fields (`c`, `F`, `G`,
/// `U`) are stepped over rather than ignored, and `f[4]` counts as four `f`.
/// Ignoring either would read every later field from the wrong offset.
fn decode_struct(
    layout: &str,
    sample_size: usize,
    repeat: usize,
    data: &[u8],
) -> std::result::Result<(usize, Vec<f64>), String> {
    let fields = parse_layout(layout, sample_size)?;
    let components = fields.iter().filter(|f| f.numeric).count();
    if components == 0 {
        return Ok((0, Vec::new()));
    }
    let mut out = Vec::with_capacity(components * repeat);
    for r in 0..repeat {
        let mut off = r * sample_size;
        let before = out.len();
        for f in &fields {
            if f.numeric {
                match data.get(off..off + f.width) {
                    Some(b) => out.push(read_one(f.kind, b)),
                    None => {
                        out.truncate(before);
                        return Ok((components, out));
                    }
                }
            }
            off += f.width;
        }
    }
    Ok((components, out))
}

/// One field of a `TYPE` layout, after array notation has been expanded.
struct Field {
    kind: u8,
    width: usize,
    numeric: bool,
}

/// Expand a layout into its fields, refusing one wider than `sample_size`.
///
/// The width check is made as the layout is read, not after: it is what keeps
/// a corrupt `f[65535]…` from expanding into billions of fields, and a layout
/// that does not fit would take each sample's tail from the next sample.
fn parse_layout(layout: &str, sample_size: usize) -> std::result::Result<Vec<Field>, String> {
    let bytes = layout.as_bytes();
    let mut fields = Vec::new();
    let mut total = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        let kind = bytes[i];
        i += 1;
        let (width, numeric) = match (type_width(kind), kind) {
            (Some(w), _) => (w, true),
            (None, b'c') => (1, false),
            (None, b'F') => (4, false),
            (None, b'G' | b'U') => (16, false),
            _ => {
                return Err(format!(
                    "TYPE {layout:?} has a field of unknown type {:?}",
                    kind as char
                ))
            }
        };
        let mut count = 1usize;
        if bytes.get(i) == Some(&b'[') {
            let close = bytes[i..]
                .iter()
                .position(|&b| b == b']')
                .ok_or_else(|| format!("TYPE {layout:?} has an unclosed ["))?;
            count = std::str::from_utf8(&bytes[i + 1..i + close])
                .ok()
                .and_then(|n| n.parse::<usize>().ok())
                .filter(|&n| n <= u16::MAX as usize)
                .ok_or_else(|| format!("TYPE {layout:?} has a bad repeat count"))?;
            i += close + 1;
        }
        total = total.saturating_add(width.saturating_mul(count));
        if total > sample_size {
            return Err(format!(
                "TYPE {layout:?} describes samples wider than their {sample_size} bytes"
            ));
        }
        for _ in 0..count {
            fields.push(Field {
                kind,
                width,
                numeric,
            });
        }
    }
    Ok(fields)
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
    let v: [f64; 3] = std::array::from_fn(|i| sample.get(i).copied().unwrap_or(0.0));
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

    let Some(track) = file.gpmd_track() else {
        // A track skipped as unreadable is the likely reason, so say so.
        let mut msg = "file has no GPMF (gpmd) telemetry track".to_string();
        for w in &file.warnings {
            msg.push_str("; ");
            msg.push_str(w);
        }
        anyhow::bail!(msg);
    };
    let mut warnings = file.warnings.clone();
    let edits = (!track.edit_list_is_trivial()).then(|| track.edits.clone());
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
        cursor = cursor.saturating_add(s.size as usize);
    }

    let data = file
        .read_track_data(track_id)
        .context("reading the telemetry track")?;
    let mut gpmf = Gpmf::parse(&data, &spans)?;
    // Payload times come straight from the sample table, which ignores edits.
    // They only time samples that lack an STMP, so an edit list matters only
    // then — and a stream-copied clip, which always has one, has STMP
    // throughout. It is reported rather than applied, because applying one
    // wrongly would shift timestamps that are right.
    let unstamped = gpmf
        .payloads
        .iter()
        .flat_map(|p| &p.devices)
        .flat_map(|d| &d.streams)
        .any(|c| c.stmp.is_none());
    if let (Some(edits), true) = (edits, unstamped) {
        warnings.push(format!(
            "the telemetry track has an edit list ({edits:?}) that is not applied, and some samples \
             carry no STMP, so their times ignore it"
        ));
    }
    warnings.append(&mut gpmf.warnings);
    gpmf.warnings = warnings;
    Ok(gpmf)
}
