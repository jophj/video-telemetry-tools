//! A deliberately small ISO base media file reader.
//!
//! `gmax` needs exactly one thing from the container that ffmpeg cannot give it
//! cheaply: the raw bytes of every sample in the `gpmd` telemetry track, plus
//! the media timescale that turns a sample index into a real instant. Doing
//! that here rather than shelling out means `telemetry`, `gpx`, `merge` and
//! `analyse` work on a machine with no ffmpeg installed at all.
//!
//! Three properties of a GoPro Max 2 `.360` drive the design:
//!
//!   * they are routinely over 4 GiB (the reference clip is 8.55 GB), so `co64`
//!     chunk offsets are mandatory, not optional;
//!   * `.360` is not a registered brand (`major_brand=mp41`) and carries
//!     private tracks (`gpmd`, `tmcd`) and an ambisonic PCM track, so a strict
//!     parser is a liability;
//!   * only the sample table is needed up front. Sample *data* is read lazily,
//!     because the video tracks alone are 8 GB.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::fourcc::FourCc;

/// Difference between the ISO-BMFF epoch (1904-01-01) and the Unix epoch.
const EPOCH_OFFSET_1904: i64 = 2_082_844_800;

/// A `moov` subtree large enough to be pathological is refused rather than
/// allocated: a real one is a few MB even for a 46 000-frame clip.
const MAX_MOOV_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Mp4Error {
    #[error("{path}: not an ISO base media file (no ftyp/moov at the top level)")]
    NotIsoBmff { path: PathBuf },
    #[error("{path}: no moov box; the file is truncated or still being written")]
    NoMoov { path: PathBuf },
    #[error("{path}: moov box is {size} bytes, refusing to read it")]
    MoovTooLarge { path: PathBuf, size: u64 },
    #[error("{path}: box at byte {offset} declares a size of {size}, smaller than its own header")]
    BadBoxSize {
        path: PathBuf,
        offset: u64,
        size: u32,
    },
    #[error("corrupt {what} box: {reason}")]
    Corrupt { what: &'static str, reason: String },
    #[error("{path}: a sample at byte {offset} of {size} bytes runs past the end of the {file_len}-byte file")]
    SampleOutOfRange {
        path: PathBuf,
        offset: u64,
        size: u32,
        file_len: u64,
    },
    #[error("truncated {what} box: wanted {wanted} bytes, {have} available")]
    TruncatedBox {
        what: &'static str,
        wanted: usize,
        have: usize,
    },
    #[error("track {track}: sample {sample} references chunk {chunk}, which the offset table does not have")]
    ChunkOutOfRange {
        track: u32,
        sample: u32,
        chunk: u32,
    },
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

type Result<T> = std::result::Result<T, Mp4Error>;

/// One sample's location in the file and its position on the media timeline.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
    /// Decode time in the track's own timescale.
    pub dts: u64,
    /// Duration in the track's own timescale.
    pub duration: u32,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub id: u32,
    /// `mdia/hdlr` handler type: `vide`, `soun`, `meta`, `data`, …
    pub handler: FourCc,
    /// First `stsd` entry's format: `hvc1`, `mp4a`, `gpmd`, `tmcd`, `in24`, …
    pub format: FourCc,
    pub timescale: u32,
    pub duration: u64,
    /// Display dimensions from `tkhd`, zero for non-visual tracks.
    pub width: u32,
    pub height: u32,
    pub samples: Vec<Sample>,
    /// The `edts/elst` edit list, empty when the track has none.
    pub edits: Vec<Edit>,
}

/// One `elst` entry: a span of the movie timeline and where in the media it
/// comes from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Edit {
    /// Length of this span, in the *movie* timescale.
    pub segment_duration: u64,
    /// Start of the span in the track's media timescale, or -1 for an empty
    /// edit: a gap in which the track presents nothing.
    pub media_time: i64,
    pub media_rate: f64,
}

impl Track {
    /// Track duration in seconds, from the media header.
    pub fn duration_secs(&self) -> f64 {
        if self.timescale == 0 {
            return 0.0;
        }
        self.duration as f64 / self.timescale as f64
    }

    /// Mean sample duration in seconds.
    ///
    /// For the `gpmd` track this is the GPMF payload interval, and it is the
    /// number every derived rate in `gmax` hangs off: on the reference clip it
    /// is 1.001 s, which is what makes the 30-samples-per-payload streams run
    /// at 29.97 Hz rather than the 30 Hz the prototypes assumed.
    pub fn mean_sample_duration(&self) -> Option<f64> {
        if self.samples.is_empty() || self.timescale == 0 {
            return None;
        }
        let total: u64 = self.samples.iter().map(|s| u64::from(s.duration)).sum();
        if total == 0 {
            return None;
        }
        Some(total as f64 / self.timescale as f64 / self.samples.len() as f64)
    }

    pub fn total_bytes(&self) -> u64 {
        self.samples.iter().map(|s| u64::from(s.size)).sum()
    }

    /// True when the edit list, if any, presents the media exactly as its
    /// sample table lays it out.
    ///
    /// Sample times in [`Sample::dts`] ignore edits, so any other list means
    /// those times are not presentation times: an empty edit delays the whole
    /// track, and a non-zero `media_time` skips its start.
    pub fn edit_list_is_trivial(&self) -> bool {
        match self.edits.as_slice() {
            [] => true,
            [only] => only.media_time == 0 && only.media_rate == 1.0,
            _ => false,
        }
    }
}

pub struct Mp4 {
    path: PathBuf,
    file: File,
    file_len: u64,
    pub tracks: Vec<Track>,
    /// `mvhd` creation time, as a Unix timestamp. GoPro writes local wall clock
    /// here on some models, so it is a fallback clock only.
    pub creation_time: Option<i64>,
    pub timescale: u32,
    pub duration: u64,
    /// Top-level `ftyp` major brand, if present.
    pub major_brand: Option<FourCc>,
    /// Flat `udta` string values, keyed by box type (`FIRM`, `MINF`, …).
    pub udta: HashMap<FourCc, Vec<u8>>,
    /// Non-fatal problems met while reading the container, e.g. a track that
    /// was skipped because its sample table did not make sense.
    pub warnings: Vec<String>,
}

impl Mp4 {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path).map_err(|source| Mp4Error::Io {
            path: path.clone(),
            source,
        })?;
        let mut reader = BufReader::with_capacity(64 * 1024, &file);

        let mut major_brand = None;
        let mut moov: Option<Vec<u8>> = None;
        let mut saw_top_level_box = false;
        let mut cursor = 0u64;
        let file_len = file
            .metadata()
            .map_err(|source| Mp4Error::Io {
                path: path.clone(),
                source,
            })?
            .len();

        let mut warnings = Vec::new();

        // Walk only the top level; `moov` may sit after an 8 GB `mdat`.
        while cursor < file_len {
            let header = match read_box_header(&mut reader, cursor, file_len, &path) {
                Ok(Some(header)) => header,
                Ok(None) => break,
                // Garbage after a moov already in hand costs nothing we need,
                // so it is reported rather than fatal. Before one, it is the
                // real reason the moov is missing and must say so, not leave
                // the caller with a misleading "no moov box".
                Err(e @ Mp4Error::BadBoxSize { .. }) if moov.is_some() => {
                    warnings.push(format!("{e}; ignoring the rest of the file"));
                    break;
                }
                Err(e) => return Err(e),
            };
            saw_top_level_box = true;
            match header.kind.as_bytes() {
                b"ftyp" => {
                    // Only the major brand is wanted. A real ftyp is a few
                    // dozen bytes, and reading no more than the brand keeps a
                    // corrupt size from becoming a huge allocation.
                    let body = read_exact_vec(&mut reader, header.body_len().min(4), &path)?;
                    if body.len() >= 4 {
                        major_brand = Some(FourCc::from_slice(&body[..4]));
                    }
                }
                b"moov" => {
                    if header.body_len() > MAX_MOOV_BYTES {
                        return Err(Mp4Error::MoovTooLarge {
                            path,
                            size: header.body_len(),
                        });
                    }
                    moov = Some(read_exact_vec(&mut reader, header.body_len(), &path)?);
                }
                _ => {
                    reader
                        .seek(SeekFrom::Start(header.end))
                        .map_err(|source| Mp4Error::Io {
                            path: path.clone(),
                            source,
                        })?;
                }
            }
            if header.end <= cursor {
                break; // A zero-length box would spin forever.
            }
            cursor = header.end;
        }

        if !saw_top_level_box {
            return Err(Mp4Error::NotIsoBmff { path });
        }
        let moov = moov.ok_or_else(|| Mp4Error::NoMoov { path: path.clone() })?;

        let mut mp4 = Mp4 {
            path,
            file,
            file_len,
            tracks: Vec::new(),
            creation_time: None,
            timescale: 0,
            duration: 0,
            major_brand,
            udta: HashMap::new(),
            warnings,
        };
        mp4.parse_moov(&moov)?;
        Ok(mp4)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Duration in seconds from the movie header.
    pub fn duration_secs(&self) -> f64 {
        if self.timescale == 0 {
            return 0.0;
        }
        self.duration as f64 / self.timescale as f64
    }

    pub fn track_by_format(&self, format: &str) -> Option<&Track> {
        let want = FourCc::from_str_lossy(format);
        self.tracks.iter().find(|t| t.format == want)
    }

    pub fn tracks_by_handler(&self, handler: &str) -> Vec<&Track> {
        let want = FourCc::from_str_lossy(handler);
        self.tracks.iter().filter(|t| t.handler == want).collect()
    }

    /// The GPMF telemetry track, identified by its `stsd` format `gpmd`.
    pub fn gpmd_track(&self) -> Option<&Track> {
        self.track_by_format("gpmd")
    }

    /// Concatenate every sample of a track into one buffer.
    ///
    /// GPMF payloads are self-delimiting, so the telemetry parser is happy to
    /// see the whole track as one stream. The `gpmd` track is ~25 MB on a
    /// 25-minute clip; callers wanting to stay streaming should use
    /// [`Mp4::read_sample`] instead.
    pub fn read_track_data(&mut self, track_id: u32) -> Result<Vec<u8>> {
        let samples = match self.tracks.iter().find(|t| t.id == track_id) {
            Some(t) => t.samples.clone(),
            None => return Ok(Vec::new()),
        };
        // Every size here came from the file, so check them all against it
        // before allocating anything: a corrupt table must not be able to ask
        // for more memory than the file has bytes.
        for s in &samples {
            self.check_in_file(s)?;
        }
        let total: u64 = samples.iter().map(|s| u64::from(s.size)).sum();
        if total > self.file_len {
            return Err(Mp4Error::Corrupt {
                what: "stsz",
                reason: format!(
                    "track {track_id}'s samples add up to {total} bytes, more than the {}-byte file",
                    self.file_len
                ),
            });
        }
        let mut out = Vec::with_capacity(total as usize);
        let mut buf = Vec::new();
        for s in &samples {
            self.file
                .seek(SeekFrom::Start(s.offset))
                .map_err(|source| Mp4Error::Io {
                    path: self.path.clone(),
                    source,
                })?;
            buf.resize(s.size as usize, 0);
            self.file
                .read_exact(&mut buf)
                .map_err(|source| Mp4Error::Io {
                    path: self.path.clone(),
                    source,
                })?;
            out.extend_from_slice(&buf);
        }
        Ok(out)
    }

    fn check_in_file(&self, sample: &Sample) -> Result<()> {
        match sample.offset.checked_add(u64::from(sample.size)) {
            Some(end) if end <= self.file_len => Ok(()),
            _ => Err(Mp4Error::SampleOutOfRange {
                path: self.path.clone(),
                offset: sample.offset,
                size: sample.size,
                file_len: self.file_len,
            }),
        }
    }

    pub fn read_sample(&mut self, sample: &Sample) -> Result<Vec<u8>> {
        self.check_in_file(sample)?;
        self.file
            .seek(SeekFrom::Start(sample.offset))
            .map_err(|source| Mp4Error::Io {
                path: self.path.clone(),
                source,
            })?;
        let mut buf = vec![0u8; sample.size as usize];
        self.file
            .read_exact(&mut buf)
            .map_err(|source| Mp4Error::Io {
                path: self.path.clone(),
                source,
            })?;
        Ok(buf)
    }

    fn parse_moov(&mut self, moov: &[u8]) -> Result<()> {
        let mut warnings = Vec::new();
        for (kind, body) in children(moov, "moov", &mut warnings) {
            match kind.as_bytes() {
                b"mvhd" => {
                    let (created, timescale, duration) = parse_mvhd(body)?;
                    self.creation_time = created;
                    self.timescale = timescale;
                    self.duration = duration;
                }
                b"trak" => match parse_trak(body, self.file_len, &mut warnings) {
                    Ok(Some(track)) => self.tracks.push(track),
                    Ok(None) => {}
                    // One unreadable track must not cost the others: the
                    // telemetry track is all most commands want, and it is
                    // rarely the one that is broken.
                    Err(e) => warnings.push(format!("skipping a track: {e}")),
                },
                b"udta" => {
                    for (k, v) in children(body, "udta", &mut warnings) {
                        self.udta.insert(k, v.to_vec());
                    }
                }
                _ => {}
            }
        }
        self.warnings.extend(warnings);
        Ok(())
    }
}

fn parse_trak(trak: &[u8], file_len: u64, warnings: &mut Vec<String>) -> Result<Option<Track>> {
    let mut id = 0u32;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut handler = FourCc::ZERO;
    let mut format = FourCc::ZERO;
    let mut timescale = 0u32;
    let mut duration = 0u64;
    let mut stbl: Option<&[u8]> = None;
    let mut edits = Vec::new();

    for (kind, body) in children(trak, "trak", warnings) {
        match kind.as_bytes() {
            b"tkhd" => {
                let (tid, w, h) = parse_tkhd(body)?;
                id = tid;
                width = w;
                height = h;
            }
            b"edts" => {
                for (k2, b2) in children(body, "edts", warnings) {
                    if k2.as_bytes() == b"elst" {
                        edits = parse_elst(b2)?;
                    }
                }
            }
            b"mdia" => {
                for (k2, b2) in children(body, "mdia", warnings) {
                    match k2.as_bytes() {
                        b"mdhd" => {
                            let (ts, dur) = parse_mdhd(b2)?;
                            timescale = ts;
                            duration = dur;
                        }
                        b"hdlr" => {
                            if b2.len() >= 12 {
                                handler = FourCc::from_slice(&b2[8..12]);
                            }
                        }
                        b"minf" => {
                            for (k3, b3) in children(b2, "minf", warnings) {
                                if k3.as_bytes() == b"stbl" {
                                    stbl = Some(b3);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    let Some(stbl) = stbl else {
        return Ok(None);
    };
    let tables = SampleTables::parse(stbl, &mut format, file_len, warnings)?;
    let samples = tables.build(id)?;

    Ok(Some(Track {
        id,
        handler,
        format,
        timescale,
        duration,
        width,
        height,
        samples,
        edits,
    }))
}

/// The four `stbl` tables needed to locate a sample, kept separate so the
/// interleaving logic reads as the spec describes it.
#[derive(Default)]
struct SampleTables {
    /// `(sample_count, sample_delta)` runs from `stts`.
    time_to_sample: Vec<(u32, u32)>,
    /// Per-sample sizes; a constant-size track expands to `sample_count`.
    sizes: Vec<u32>,
    /// `(first_chunk, samples_per_chunk)` runs from `stsc`.
    sample_to_chunk: Vec<(u32, u32)>,
    /// Chunk offsets from either `stco` or `co64`.
    chunk_offsets: Vec<u64>,
}

impl SampleTables {
    fn parse(
        stbl: &[u8],
        format: &mut FourCc,
        file_len: u64,
        warnings: &mut Vec<String>,
    ) -> Result<Self> {
        let mut t = SampleTables::default();
        for (kind, body) in children(stbl, "stbl", warnings) {
            match kind.as_bytes() {
                b"stsd" => {
                    // version/flags(4) + entry_count(4), then size(4) + format(4).
                    if body.len() >= 16 {
                        *format = FourCc::from_slice(&body[12..16]);
                    }
                }
                b"stts" => {
                    let mut r = Reader::new(body, "stts");
                    r.skip(4)?; // version/flags
                    let n = r.count(8)?;
                    t.time_to_sample.reserve(n);
                    for _ in 0..n {
                        let count = r.u32()?;
                        let delta = r.u32()?;
                        t.time_to_sample.push((count, delta));
                    }
                }
                b"stsz" => {
                    let mut r = Reader::new(body, "stsz");
                    r.skip(4)?;
                    let uniform = r.u32()?;
                    if uniform != 0 {
                        // A uniform size has no table to bound the count, so
                        // bound it by the file instead: the samples have to
                        // fit in it.
                        let count = r.u32()?;
                        if u64::from(uniform) * u64::from(count) > file_len {
                            return Err(Mp4Error::Corrupt {
                                what: "stsz",
                                reason: format!(
                                    "{count} samples of {uniform} bytes cannot fit in a {file_len}-byte file"
                                ),
                            });
                        }
                        t.sizes = vec![uniform; count as usize];
                    } else {
                        let count = r.count(4)?;
                        t.sizes.reserve(count);
                        for _ in 0..count {
                            t.sizes.push(r.u32()?);
                        }
                    }
                }
                b"stz2" => {
                    let mut r = Reader::new(body, "stz2");
                    r.skip(4)?;
                    r.skip(3)?; // reserved
                    let field_size = r.u8()?;
                    let count = r.u32()? as usize;
                    let table_bytes = match field_size {
                        4 => count.div_ceil(2),
                        8 => count,
                        16 => count.saturating_mul(2),
                        // Anything else is not a valid stz2, and guessing a
                        // width would misplace every sample after the first.
                        other => {
                            return Err(Mp4Error::Corrupt {
                                what: "stz2",
                                reason: format!("field size {other}, not 4, 8 or 16"),
                            })
                        }
                    };
                    if table_bytes > r.remaining() {
                        return Err(Mp4Error::TruncatedBox {
                            what: "stz2",
                            wanted: table_bytes,
                            have: r.remaining(),
                        });
                    }
                    t.sizes.reserve(count);
                    match field_size {
                        4 => {
                            for i in 0..count {
                                if i % 2 == 0 {
                                    let b = r.u8()?;
                                    t.sizes.push(u32::from(b >> 4));
                                    if i + 1 < count {
                                        t.sizes.push(u32::from(b & 0x0f));
                                    }
                                }
                            }
                        }
                        8 => {
                            for _ in 0..count {
                                t.sizes.push(u32::from(r.u8()?));
                            }
                        }
                        _ => {
                            for _ in 0..count {
                                t.sizes.push(u32::from(r.u16()?));
                            }
                        }
                    }
                }
                b"stsc" => {
                    let mut r = Reader::new(body, "stsc");
                    r.skip(4)?;
                    let n = r.count(12)?;
                    t.sample_to_chunk.reserve(n);
                    for _ in 0..n {
                        let first_chunk = r.u32()?;
                        let per_chunk = r.u32()?;
                        r.skip(4)?; // sample_description_index
                        t.sample_to_chunk.push((first_chunk, per_chunk));
                    }
                }
                b"stco" => {
                    let mut r = Reader::new(body, "stco");
                    r.skip(4)?;
                    let n = r.count(4)?;
                    t.chunk_offsets.reserve(n);
                    for _ in 0..n {
                        t.chunk_offsets.push(u64::from(r.u32()?));
                    }
                }
                b"co64" => {
                    // Mandatory on the reference clip: an 8.55 GB file cannot
                    // express its chunk offsets in 32 bits.
                    let mut r = Reader::new(body, "co64");
                    r.skip(4)?;
                    let n = r.count(8)?;
                    t.chunk_offsets.reserve(n);
                    for _ in 0..n {
                        t.chunk_offsets.push(r.u64()?);
                    }
                }
                _ => {}
            }
        }
        Ok(t)
    }

    /// Resolve the tables into a flat sample list.
    fn build(&self, track_id: u32) -> Result<Vec<Sample>> {
        let n = self.sizes.len();
        let mut samples = Vec::with_capacity(n);
        if n == 0 || self.chunk_offsets.is_empty() {
            return Ok(samples);
        }

        // Walk the stsc runs, laying samples into chunks in order. Each chunk
        // starts at its own offset and samples pack forward from there.
        let mut sample_idx = 0usize;
        let mut run = 0usize;
        let mut chunk = 1u32; // stsc chunk numbers are 1-based
        while sample_idx < n {
            let per_chunk = {
                // Advance to the run covering this chunk.
                while run + 1 < self.sample_to_chunk.len()
                    && self.sample_to_chunk[run + 1].0 <= chunk
                {
                    run += 1;
                }
                match self.sample_to_chunk.get(run) {
                    Some(&(_, per)) if per > 0 => per,
                    // A track with sizes but no usable stsc: assume one sample
                    // per chunk rather than looping forever.
                    _ => 1,
                }
            };
            let base = *self
                .chunk_offsets
                .get(chunk as usize - 1)
                .ok_or(Mp4Error::ChunkOutOfRange {
                    track: track_id,
                    sample: sample_idx as u32,
                    chunk,
                })?;
            let mut within = 0u64;
            for _ in 0..per_chunk {
                if sample_idx >= n {
                    break;
                }
                let size = self.sizes[sample_idx];
                let offset = base.checked_add(within).ok_or_else(|| Mp4Error::Corrupt {
                    what: "stco",
                    reason: format!(
                        "track {track_id}: sample {sample_idx} lies past the end of the 64-bit address space"
                    ),
                })?;
                samples.push(Sample {
                    offset,
                    size,
                    dts: 0,
                    duration: 0,
                });
                within += u64::from(size);
                sample_idx += 1;
            }
            chunk += 1;
        }

        // Overlay the timeline from stts.
        let mut dts = 0u64;
        let mut i = 0usize;
        for &(count, delta) in &self.time_to_sample {
            for _ in 0..count {
                if i >= samples.len() {
                    break;
                }
                samples[i].dts = dts;
                samples[i].duration = delta;
                dts = dts.saturating_add(u64::from(delta));
                i += 1;
            }
        }
        // Any samples past the end of stts inherit the last delta.
        if i < samples.len() {
            let delta = self.time_to_sample.last().map(|&(_, d)| d).unwrap_or(0);
            while i < samples.len() {
                samples[i].dts = dts;
                samples[i].duration = delta;
                dts = dts.saturating_add(u64::from(delta));
                i += 1;
            }
        }

        Ok(samples)
    }
}

// ---------------------------------------------------------------- box walking

struct BoxHeader {
    kind: FourCc,
    /// Absolute offset of the body.
    body: u64,
    /// Absolute offset one past the end of the box.
    end: u64,
}

impl BoxHeader {
    fn body_len(&self) -> u64 {
        self.end.saturating_sub(self.body)
    }
}

fn read_box_header<R: Read + Seek>(
    reader: &mut R,
    at: u64,
    file_len: u64,
    path: &Path,
) -> Result<Option<BoxHeader>> {
    reader
        .seek(SeekFrom::Start(at))
        .map_err(|source| Mp4Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let mut head = [0u8; 8];
    if reader.read_exact(&mut head).is_err() {
        return Ok(None);
    }
    let size32 = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
    let kind = FourCc::from_slice(&head[4..8]);
    let (body, end) = match size32 {
        // `size == 1`: a 64-bit largesize follows the type.
        1 => {
            let mut big = [0u8; 8];
            if reader.read_exact(&mut big).is_err() {
                return Ok(None);
            }
            let size = u64::from_be_bytes(big);
            (at + 16, at.saturating_add(size.max(16)))
        }
        // `size == 0`: the box runs to end of file.
        0 => (at + 8, file_len),
        s if u64::from(s) < 8 => {
            return Err(Mp4Error::BadBoxSize {
                path: path.to_path_buf(),
                offset: at,
                size: s,
            })
        }
        s => (at + 8, at + u64::from(s)),
    };
    Ok(Some(BoxHeader {
        kind,
        body,
        end: end.min(file_len),
    }))
}

/// Read exactly `len` bytes, growing the buffer only as bytes arrive.
///
/// `len` comes from a box header, so it is only as trustworthy as the file:
/// allocating it all up front would let a file that lies about its size, or
/// shrinks while being read, claim memory it never fills.
fn read_exact_vec<R: Read>(reader: &mut R, len: u64, path: &Path) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    reader
        .take(len)
        .read_to_end(&mut buf)
        .map_err(|source| Mp4Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if (buf.len() as u64) < len {
        return Err(Mp4Error::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("wanted {len} bytes, the file had {}", buf.len()),
            ),
        });
    }
    Ok(buf)
}

/// Iterates the child boxes of an in-memory container body.
///
/// Stops at the first child whose header cannot be read, recording why in
/// `malformed` so the caller can report it rather than quietly lose every box
/// after it.
struct BoxIter<'a> {
    buf: &'a [u8],
    off: usize,
    malformed: Option<String>,
}

impl<'a> BoxIter<'a> {
    fn new(buf: &'a [u8]) -> Self {
        BoxIter {
            buf,
            off: 0,
            malformed: None,
        }
    }

    fn stop(&mut self, why: String) -> Option<(FourCc, &'a [u8])> {
        self.malformed = Some(why);
        self.off = self.buf.len();
        None
    }
}

impl<'a> Iterator for BoxIter<'a> {
    type Item = (FourCc, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let at = self.off;
        let rest = self.buf.len().saturating_sub(at);
        if rest < 8 {
            // Up to seven trailing zero bytes are padding some muxers leave;
            // anything else is a header that was cut short.
            if self.buf[at.min(self.buf.len())..].iter().any(|&b| b != 0) {
                return self.stop(format!("{rest} stray bytes at offset {at}"));
            }
            return None;
        }
        let size32 = u32::from_be_bytes([
            self.buf[at],
            self.buf[at + 1],
            self.buf[at + 2],
            self.buf[at + 3],
        ]);
        let kind = FourCc::from_slice(&self.buf[at + 4..at + 8]);
        let (body_start, box_end) = match size32 {
            1 => {
                if rest < 16 {
                    return self.stop(format!("{kind} at offset {at} has a truncated 64-bit size"));
                }
                let mut big = [0u8; 8];
                big.copy_from_slice(&self.buf[at + 8..at + 16]);
                let size = u64::from_be_bytes(big);
                if size < 16 {
                    return self.stop(format!(
                        "{kind} at offset {at} declares a 64-bit size of {size}"
                    ));
                }
                (
                    at + 16,
                    usize::try_from(size).map_or(usize::MAX, |s| at.saturating_add(s)),
                )
            }
            0 => (at + 8, self.buf.len()),
            s if s < 8 => {
                return self.stop(format!("{kind} at offset {at} declares a size of {s}"));
            }
            s => (at + 8, at.saturating_add(s as usize)),
        };
        if box_end > self.buf.len() {
            // Handed out clamped rather than dropped: the tables inside
            // report their own truncation, and a box that merely overhangs
            // its parent is often readable.
            self.malformed = Some(format!(
                "{kind} at offset {at} claims {} bytes but only {rest} remain",
                box_end - at
            ));
        }
        let box_end = box_end.min(self.buf.len());
        self.off = box_end;
        Some((kind, &self.buf[body_start..box_end]))
    }
}

/// The children of a container, with any malformation noted in `warnings`.
fn children<'a>(
    buf: &'a [u8],
    parent: &str,
    warnings: &mut Vec<String>,
) -> Vec<(FourCc, &'a [u8])> {
    let mut iter = BoxIter::new(buf);
    let out: Vec<_> = iter.by_ref().collect();
    if let Some(why) = iter.malformed {
        warnings.push(format!("malformed {parent} box: {why}"));
    }
    out
}

/// Big-endian cursor with bounds errors that name the box they came from.
struct Reader<'a> {
    buf: &'a [u8],
    off: usize,
    what: &'static str,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8], what: &'static str) -> Self {
        Reader { buf, off: 0, what }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.off
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(Mp4Error::TruncatedBox {
                what: self.what,
                wanted: self.off.saturating_add(n),
                have: self.buf.len(),
            });
        }
        let s = &self.buf[self.off..self.off + n];
        self.off += n;
        Ok(s)
    }

    fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }

    /// An entry count for a table of `entry_size`-byte entries, refused when
    /// the box is too short to hold that many. It is what bounds every
    /// allocation made from a table's own claim of its length.
    fn count(&mut self, entry_size: usize) -> Result<usize> {
        let n = self.u32()? as usize;
        let wanted = n.saturating_mul(entry_size);
        if wanted > self.remaining() {
            return Err(Mp4Error::TruncatedBox {
                what: self.what,
                wanted: self.off.saturating_add(wanted),
                have: self.buf.len(),
            });
        }
        Ok(n)
    }

    fn u64(&mut self) -> Result<u64> {
        let s = self.take(8)?;
        Ok(u64::from_be_bytes([
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
        ]))
    }
}

fn parse_mvhd(body: &[u8]) -> Result<(Option<i64>, u32, u64)> {
    let mut r = Reader::new(body, "mvhd");
    let version = r.u8()?;
    r.skip(3)?; // flags
    let (created, timescale, duration) = if version == 1 {
        let created = r.u64()?;
        r.skip(8)?; // modification_time
        let timescale = r.u32()?;
        let duration = r.u64()?;
        (created, timescale, duration)
    } else {
        let created = u64::from(r.u32()?);
        r.skip(4)?;
        let timescale = r.u32()?;
        let duration = u64::from(r.u32()?);
        (created, timescale, duration)
    };
    // A version 1 time is 64 bits unsigned; one past i64::MAX is not a date
    // anyone recorded, so it is treated as absent rather than wrapped.
    let unix = if created == 0 {
        None
    } else {
        i64::try_from(created)
            .ok()
            .and_then(|c| c.checked_sub(EPOCH_OFFSET_1904))
    };
    Ok((unix, timescale, duration))
}

fn parse_elst(body: &[u8]) -> Result<Vec<Edit>> {
    let mut r = Reader::new(body, "elst");
    let version = r.u8()?;
    r.skip(3)?;
    let entry_size = if version == 1 { 20 } else { 12 };
    let n = r.count(entry_size)?;
    let mut edits = Vec::with_capacity(n);
    for _ in 0..n {
        let (segment_duration, media_time) = if version == 1 {
            (r.u64()?, r.u64()? as i64)
        } else {
            (u64::from(r.u32()?), i64::from(r.u32()? as i32))
        };
        let whole = r.u16()? as i16;
        let fraction = r.u16()?;
        edits.push(Edit {
            segment_duration,
            media_time,
            media_rate: f64::from(whole) + f64::from(fraction) / 65536.0,
        });
    }
    Ok(edits)
}

fn parse_tkhd(body: &[u8]) -> Result<(u32, u32, u32)> {
    let mut r = Reader::new(body, "tkhd");
    let version = r.u8()?;
    r.skip(3)?;
    if version == 1 {
        r.skip(16)?; // creation + modification
    } else {
        r.skip(8)?;
    }
    let track_id = r.u32()?;
    r.skip(4)?; // reserved
    if version == 1 {
        r.skip(8)?; // duration
    } else {
        r.skip(4)?;
    }
    r.skip(8)?; // reserved
    r.skip(2)?; // layer
    r.skip(2)?; // alternate_group
    r.skip(2)?; // volume
    r.skip(2)?; // reserved
    r.skip(36)?; // matrix
    // width/height are 16.16 fixed point.
    let width = r.u32()? >> 16;
    let height = r.u32()? >> 16;
    Ok((track_id, width, height))
}

fn parse_mdhd(body: &[u8]) -> Result<(u32, u64)> {
    let mut r = Reader::new(body, "mdhd");
    let version = r.u8()?;
    r.skip(3)?;
    if version == 1 {
        r.skip(16)?;
        let timescale = r.u32()?;
        let duration = r.u64()?;
        Ok((timescale, duration))
    } else {
        r.skip(8)?;
        let timescale = r.u32()?;
        let duration = u64::from(r.u32()?);
        Ok((timescale, duration))
    }
}
