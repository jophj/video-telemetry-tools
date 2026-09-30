//! The encoder registry, and choosing one for this machine.
//!
//! A `--quality` preset is a *candidate list*, not a fixed encoder: the first
//! candidate that actually works here wins. That is what lets the same command
//! line do the right thing on an Apple laptop, an NVIDIA workstation, an Intel
//! box and a bare VM, which is the whole point of shipping one binary for three
//! platforms.

use std::collections::HashMap;
use std::path::PathBuf;

use super::Ffmpeg;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Gpu,
    Cpu,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Gpu => "GPU",
            Kind::Cpu => "CPU",
        }
    }
}

/// One encoder and how to drive it.
#[derive(Debug, Clone, Copy)]
pub struct Encoder {
    pub name: &'static str,
    pub kind: Kind,
    /// The codec it produces: `hevc`, `h264`, `av1`, `vp9`, `prores`.
    pub codec: &'static str,
    /// Pixel format to feed it.
    pub pix_fmt: &'static str,
    /// Default rate-control value, on this encoder's own scale.
    pub default_q: Option<u32>,
    pub notes: &'static str,
}

/// Every encoder `gmax` knows how to drive, hardware first within each codec.
///
/// Values are carried over from the shell prototype, which tuned them against
/// real 360 footage; the notes say which scale each number is on, because they
/// are not comparable between encoders.
pub const REGISTRY: &[Encoder] = &[
    // --- Apple, macOS -----------------------------------------------------
    Encoder { name: "hevc_videotoolbox", kind: Kind::Gpu, codec: "hevc", pix_fmt: "p010le", default_q: Some(65), notes: "Apple VideoToolbox, 10-bit (quality 0-100)" },
    Encoder { name: "h264_videotoolbox", kind: Kind::Gpu, codec: "h264", pix_fmt: "nv12", default_q: Some(60), notes: "Apple VideoToolbox, 8-bit (quality 0-100)" },
    Encoder { name: "prores_videotoolbox", kind: Kind::Gpu, codec: "prores", pix_fmt: "p210le", default_q: None, notes: "Apple VideoToolbox ProRes 422 HQ" },
    // --- NVIDIA, Windows and Linux ---------------------------------------
    Encoder { name: "hevc_nvenc", kind: Kind::Gpu, codec: "hevc", pix_fmt: "p010le", default_q: Some(22), notes: "NVIDIA NVENC, 10-bit (CQ)" },
    Encoder { name: "h264_nvenc", kind: Kind::Gpu, codec: "h264", pix_fmt: "yuv420p", default_q: Some(22), notes: "NVIDIA NVENC, 8-bit (CQ)" },
    Encoder { name: "av1_nvenc", kind: Kind::Gpu, codec: "av1", pix_fmt: "p010le", default_q: Some(28), notes: "NVIDIA NVENC AV1, Ada or newer (CQ)" },
    // --- Intel Quick Sync, Windows and Linux ------------------------------
    Encoder { name: "hevc_qsv", kind: Kind::Gpu, codec: "hevc", pix_fmt: "p010le", default_q: Some(23), notes: "Intel Quick Sync, 10-bit (global_quality)" },
    Encoder { name: "h264_qsv", kind: Kind::Gpu, codec: "h264", pix_fmt: "nv12", default_q: Some(23), notes: "Intel Quick Sync, 8-bit (global_quality)" },
    Encoder { name: "av1_qsv", kind: Kind::Gpu, codec: "av1", pix_fmt: "p010le", default_q: Some(30), notes: "Intel Quick Sync AV1, Arc or newer" },
    // --- AMD, Windows and Linux -------------------------------------------
    Encoder { name: "hevc_amf", kind: Kind::Gpu, codec: "hevc", pix_fmt: "p010le", default_q: Some(22), notes: "AMD AMF, 10-bit (QP)" },
    Encoder { name: "h264_amf", kind: Kind::Gpu, codec: "h264", pix_fmt: "nv12", default_q: Some(22), notes: "AMD AMF, 8-bit (QP)" },
    Encoder { name: "av1_amf", kind: Kind::Gpu, codec: "av1", pix_fmt: "p010le", default_q: Some(28), notes: "AMD AMF AV1, RDNA 3 or newer" },
    // --- VAAPI, Linux -----------------------------------------------------
    Encoder { name: "hevc_vaapi", kind: Kind::Gpu, codec: "hevc", pix_fmt: "p010", default_q: Some(22), notes: "VAAPI, 10-bit (QP); needs a render node" },
    Encoder { name: "h264_vaapi", kind: Kind::Gpu, codec: "h264", pix_fmt: "nv12", default_q: Some(22), notes: "VAAPI, 8-bit (QP); needs a render node" },
    Encoder { name: "av1_vaapi", kind: Kind::Gpu, codec: "av1", pix_fmt: "p010", default_q: Some(28), notes: "VAAPI AV1 (QP); needs a render node" },
    // --- Media Foundation, Windows ----------------------------------------
    // The floor on a Windows box whose ffmpeg was built without NVENC, QSV or
    // AMF: Media Foundation is part of the OS, so it is almost always there.
    Encoder { name: "hevc_mf", kind: Kind::Gpu, codec: "hevc", pix_fmt: "nv12", default_q: Some(24), notes: "Windows Media Foundation HEVC (quality)" },
    Encoder { name: "h264_mf", kind: Kind::Gpu, codec: "h264", pix_fmt: "nv12", default_q: Some(24), notes: "Windows Media Foundation H.264 (quality)" },
    // --- software, everywhere ---------------------------------------------
    Encoder { name: "libx265", kind: Kind::Cpu, codec: "hevc", pix_fmt: "yuv420p10le", default_q: Some(16), notes: "x265 10-bit, preset slow (CRF)" },
    Encoder { name: "libx264", kind: Kind::Cpu, codec: "h264", pix_fmt: "yuv420p", default_q: Some(18), notes: "x264 8-bit, preset slow (CRF)" },
    Encoder { name: "libsvtav1", kind: Kind::Cpu, codec: "av1", pix_fmt: "yuv420p10le", default_q: Some(28), notes: "SVT-AV1 10-bit, preset 6 (CRF)" },
    Encoder { name: "libaom-av1", kind: Kind::Cpu, codec: "av1", pix_fmt: "yuv420p10le", default_q: Some(30), notes: "libaom AV1, slow (CRF)" },
    Encoder { name: "libvpx-vp9", kind: Kind::Cpu, codec: "vp9", pix_fmt: "yuv420p10le", default_q: Some(31), notes: "libvpx VP9 (CRF)" },
    Encoder { name: "prores_ks", kind: Kind::Cpu, codec: "prores", pix_fmt: "yuv422p10le", default_q: None, notes: "ProRes 422 HQ, for NLE round-tripping" },
];

/// Whether an encoder is fed more than 8 bits per sample.
pub fn is_high_bit_depth(enc: &Encoder) -> bool {
    // p010, p010le, p210le, yuv420p10le, yuv422p10le: every high-depth format
    // in the registry names its depth.
    enc.pix_fmt.contains("10")
}

pub fn lookup(name: &str) -> Option<&'static Encoder> {
    REGISTRY.iter().find(|e| e.name == name)
}

/// A named quality target, resolved per machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// Best available HEVC, hardware first. The default: `max` would take hours
    /// on a full clip.
    High,
    /// Software HEVC first, for the smallest transparent file.
    Max,
    /// Best available AV1. About half the size of `high` on this footage, but
    /// player support in a 360 viewer is far less of a sure thing.
    Av1,
    /// ProRes, for handing to an editor.
    Prores,
    /// H.264, for maximum compatibility.
    H264,
}

impl Preset {
    pub fn as_str(self) -> &'static str {
        match self {
            Preset::High => "high",
            Preset::Max => "max",
            Preset::Av1 => "av1",
            Preset::Prores => "prores",
            Preset::H264 => "h264",
        }
    }

    /// Candidates in preference order. The first usable one wins.
    pub fn candidates(self) -> Vec<&'static str> {
        match self {
            Preset::High => vec![
                "hevc_videotoolbox",
                "hevc_nvenc",
                "hevc_qsv",
                "hevc_amf",
                "hevc_mf",
                "hevc_vaapi",
                "libx265",
            ],
            Preset::Max => vec![
                "libx265",
                "hevc_videotoolbox",
                "hevc_nvenc",
                "hevc_qsv",
                "hevc_amf",
                "hevc_vaapi",
            ],
            Preset::Av1 => vec![
                "libsvtav1",
                "av1_nvenc",
                "av1_qsv",
                "av1_amf",
                "av1_vaapi",
                "libaom-av1",
            ],
            Preset::Prores => vec!["prores_ks", "prores_videotoolbox"],
            Preset::H264 => vec![
                "libx264",
                "h264_videotoolbox",
                "h264_nvenc",
                "h264_qsv",
                "h264_amf",
                "h264_mf",
                "h264_vaapi",
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Verified: it encoded a frame here.
    Usable,
    /// In the build, but it could not encode a frame. Almost always missing
    /// hardware.
    Unusable,
    /// Not compiled into this ffmpeg.
    NotBuilt,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Usable => "usable",
            Status::Unusable => "not usable here",
            Status::NotBuilt => "not built",
        }
    }
}

/// What this machine can do, with one entry per registry encoder.
#[derive(Debug, Clone)]
pub struct Survey {
    pub entries: Vec<(&'static Encoder, Status)>,
    /// The ffmpeg version this survey describes.
    pub version: String,
    pub from_cache: bool,
}

impl Survey {
    pub fn status(&self, name: &str) -> Option<Status> {
        self.entries
            .iter()
            .find(|(e, _)| e.name == name)
            .map(|(_, s)| *s)
    }

    /// Overrule one verdict, e.g. for an encoder that passed the survey but
    /// failed at the real output size.
    pub fn set_status(&mut self, name: &str, status: Status) {
        if let Some(entry) = self.entries.iter_mut().find(|(e, _)| e.name == name) {
            entry.1 = status;
        }
    }

    pub fn is_usable(&self, name: &str) -> bool {
        self.status(name) == Some(Status::Usable)
    }

    /// Resolve a preset to an encoder that works here.
    pub fn resolve(&self, preset: Preset, accel: Option<Kind>) -> Option<&'static Encoder> {
        preset
            .candidates()
            .into_iter()
            .filter_map(lookup)
            .filter(|e| accel.map(|k| e.kind == k).unwrap_or(true))
            .find(|e| self.is_usable(e.name))
    }

    /// A table of everything, for `gmax encoders`.
    pub fn render(&self, accel: Option<Kind>) -> String {
        let mut out = String::new();
        out.push_str(&format!("ffmpeg: {}\n", self.version));
        if self.from_cache {
            out.push_str("(cached; pass --reprobe to test again)\n");
        }
        out.push_str("\n  status           encoder              type  codec   notes\n");
        out.push_str("  ---------------  -------------------  ----  ------  -----------------------------------\n");
        for (e, s) in &self.entries {
            if let Some(k) = accel {
                if e.kind != k {
                    continue;
                }
            }
            out.push_str(&format!(
                "  {:<15}  {:<19}  {:<4}  {:<6}  {}\n",
                s.as_str(),
                e.name,
                e.kind.as_str(),
                e.codec,
                e.notes
            ));
        }
        out.push_str("\n  presets on this machine:\n");
        for p in [
            Preset::High,
            Preset::Max,
            Preset::Av1,
            Preset::Prores,
            Preset::H264,
        ] {
            match self.resolve(p, accel) {
                Some(e) => out.push_str(&format!(
                    "    --quality {:<7} {} ({})\n",
                    p.as_str(),
                    e.name,
                    e.kind.as_str()
                )),
                None => out.push_str(&format!(
                    "    --quality {:<7} nothing usable here\n",
                    p.as_str()
                )),
            }
        }
        out
    }
}

/// How long a hardware encoder's failure is believed.
///
/// A success is a fact about the machine, but a failure is often a moment: a
/// driver not yet loaded, a GPU busy with another encode, an eGPU unplugged.
/// Caching that forever would hide the encoder until someone thought to pass
/// `--reprobe`, so a failure is retried once it is this old.
const FAILURE_TTL_S: u64 = 60 * 60;

/// Survey the machine, using the cache unless told to re-probe.
pub fn survey(ff: &Ffmpeg, reprobe: bool) -> Survey {
    let key = cache_key(ff);
    let now = unix_now();
    let cached = if reprobe { None } else { read_cache(&key) };

    // Everything cached and fresh: nothing to test.
    if let Some(c) = &cached {
        if c.iter().all(|(_, _, at)| !expired(*at, now)) {
            return Survey {
                entries: c.iter().map(|(e, s, _)| (*e, *s)).collect(),
                version: ff.version.clone(),
                from_cache: true,
            };
        }
    }

    // Otherwise test only what the cache cannot vouch for: everything on a
    // miss, just the stale failures on a hit.
    let listed: Option<Vec<String>> = cached.is_none().then(|| ff.listed_encoders());
    let mut rows: Vec<(&'static Encoder, Status, Option<u64>)> = Vec::with_capacity(REGISTRY.len());
    for (i, enc) in REGISTRY.iter().enumerate() {
        let previous = cached.as_ref().map(|c| c[i]);
        let row = match (previous, &listed) {
            (Some((_, status, at)), _) if !expired(at, now) => (enc, status, at),
            (Some(_), _) => probe_row(ff, enc, now),
            (None, Some(listed)) if !listed.iter().any(|l| l == enc.name) => {
                (enc, Status::NotBuilt, None)
            }
            (None, _) if enc.kind == Kind::Gpu => probe_row(ff, enc, now),
            // Software is trusted from the build list: if it compiled in, it
            // runs.
            (None, _) => (enc, Status::Usable, None),
        };
        rows.push(row);
    }

    write_cache(&key, &rows);
    Survey {
        entries: rows.iter().map(|(e, s, _)| (*e, *s)).collect(),
        version: ff.version.clone(),
        from_cache: false,
    }
}

/// Test one hardware encoder, stamping a failure with when it was seen.
fn probe_row(ff: &Ffmpeg, enc: &'static Encoder, now: u64) -> (&'static Encoder, Status, Option<u64>) {
    if ff.test_encoder(enc) {
        (enc, Status::Usable, None)
    } else {
        (enc, Status::Unusable, Some(now))
    }
}

/// Whether a cached failure has outlived its [`FAILURE_TTL_S`].
fn expired(failed_at: Option<u64>, now: u64) -> bool {
    failed_at.is_some_and(|at| now.saturating_sub(at) >= FAILURE_TTL_S)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What the cached verdicts are about: this exact ffmpeg binary.
///
/// The version banner alone is not enough. Two installs can print the same
/// banner and be built with different encoders — a Homebrew ffmpeg and a
/// static build of the same release, say — and `--ffmpeg` switches between
/// them per invocation. So the key is the banner plus the binary's canonical
/// path, size and modification time, and replacing or pointing elsewhere
/// re-probes automatically.
fn cache_key(ff: &Ffmpeg) -> String {
    let path = std::fs::canonicalize(&ff.ffmpeg).unwrap_or_else(|_| ff.ffmpeg.clone());
    let (size, mtime) = std::fs::metadata(&path)
        .map(|m| {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (m.len(), mtime)
        })
        .unwrap_or((0, 0));
    // Tabs and newlines would break the line-oriented file; neither belongs
    // in a sane path, and replacing them only risks a spurious re-probe.
    let clean = |s: String| s.replace(['\t', '\n', '\r'], " ");
    format!(
        "{}\t{}\t{size}\t{mtime}",
        clean(ff.version.clone()),
        clean(path.display().to_string())
    )
}

fn cache_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "gmax")?;
    Some(dirs.cache_dir().join("encoders.txt"))
}

/// The cache is a plain text file whose first line is the [`cache_key`].
///
/// Probing a dozen hardware encoders costs a second or two, which is nothing
/// once but irritating on every invocation. Each following line is an encoder,
/// its verdict and, for a failure, when it was seen.
///
/// Returned in registry order, one row per encoder, or `None` when the cache
/// is missing, for another binary, or written by an older registry that does
/// not mention every encoder — re-probing beats reporting one wrongly.
fn read_cache(key: &str) -> Option<Vec<(&'static Encoder, Status, Option<u64>)>> {
    let text = std::fs::read_to_string(cache_path()?).ok()?;
    parse_cache(&text, key)
}

fn parse_cache(text: &str, key: &str) -> Option<Vec<(&'static Encoder, Status, Option<u64>)>> {
    let mut lines = text.lines();
    if lines.next()? != key {
        return None;
    }
    let mut found: HashMap<&str, (Status, Option<u64>)> = HashMap::new();
    for line in lines {
        let mut parts = line.split('\t');
        let (Some(name), Some(status)) = (parts.next(), parts.next()) else {
            continue;
        };
        let status = match status {
            "usable" => Status::Usable,
            "unusable" => Status::Unusable,
            "notbuilt" => Status::NotBuilt,
            _ => continue,
        };
        // A failure without a time is from a cache that predates the TTL;
        // treating it as ancient gets it retried.
        let at = (status == Status::Unusable)
            .then(|| parts.next().and_then(|t| t.parse().ok()).unwrap_or(0));
        if let Some(e) = lookup(name) {
            found.insert(e.name, (status, at));
        }
    }
    REGISTRY
        .iter()
        .map(|e| found.get(e.name).map(|(s, at)| (e, *s, *at)))
        .collect()
}

fn render_cache(key: &str, rows: &[(&'static Encoder, Status, Option<u64>)]) -> String {
    let mut text = String::new();
    text.push_str(key);
    text.push('\n');
    for (e, s, at) in rows {
        let word = match s {
            Status::Usable => "usable",
            Status::Unusable => "unusable",
            Status::NotBuilt => "notbuilt",
        };
        match at {
            Some(at) => text.push_str(&format!("{}\t{word}\t{at}\n", e.name)),
            None => text.push_str(&format!("{}\t{word}\n", e.name)),
        }
    }
    text
}

fn write_cache(key: &str, rows: &[(&'static Encoder, Status, Option<u64>)]) {
    let Some(path) = cache_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // A failure to cache is not a failure to work.
    let _ = std::fs::write(path, render_cache(key, rows));
}

/// Rate-control and pixel-format arguments for an encoder.
///
/// Each family takes its quality on its own scale and by its own flag name,
/// which is why a single `--enc-quality` cannot be passed through blindly.
pub fn video_args(enc: &Encoder, quality: Option<u32>, vaapi_filter: bool) -> Vec<String> {
    let q = quality.or(enc.default_q);
    let mut a: Vec<String> = vec!["-c:v".into(), enc.name.into()];
    let name = enc.name;

    if name == "prores_videotoolbox" {
        a.extend(["-profile:v".into(), "hq".into()]);
    } else if name == "prores_ks" {
        // Profile 3 is 422 HQ; the vendor tag keeps Apple tools happy.
        a.extend([
            "-profile:v".into(),
            "3".into(),
            "-vendor".into(),
            "apl0".into(),
        ]);
    } else if name.ends_with("_videotoolbox") {
        if let Some(q) = q {
            a.extend(["-q:v".into(), q.to_string()]);
        }
        if name == "hevc_videotoolbox" {
            a.extend(["-profile:v".into(), "main10".into()]);
        }
    } else if name.ends_with("_nvenc") {
        a.extend([
            "-preset".into(),
            "p5".into(),
            "-tune".into(),
            "hq".into(),
            "-rc".into(),
            "vbr".into(),
        ]);
        if let Some(q) = q {
            a.extend(["-cq".into(), q.to_string()]);
        }
        // Constant quality means no target bitrate.
        a.extend(["-b:v".into(), "0".into()]);
    } else if name.ends_with("_qsv") {
        a.extend(["-preset".into(), "medium".into()]);
        if let Some(q) = q {
            a.extend(["-global_quality".into(), q.to_string()]);
        }
    } else if name.ends_with("_amf") {
        a.extend([
            "-quality".into(),
            "quality".into(),
            "-rc".into(),
            "cqp".into(),
        ]);
        if let Some(q) = q {
            a.extend([
                "-qp_i".into(),
                q.to_string(),
                "-qp_p".into(),
                q.to_string(),
            ]);
        }
    } else if name.ends_with("_vaapi") {
        a.extend(["-rc_mode".into(), "CQP".into()]);
        if let Some(q) = q {
            a.extend(["-qp".into(), q.to_string()]);
        }
    } else if name.ends_with("_mf") {
        // Media Foundation takes a 0-100 quality, like VideoToolbox.
        a.extend(["-rate_control".into(), "quality".into()]);
        if let Some(q) = q {
            a.extend(["-quality".into(), q.to_string()]);
        }
    } else if name == "libx265" {
        if let Some(q) = q {
            a.extend(["-crf".into(), q.to_string()]);
        }
        a.extend([
            "-preset".into(),
            "slow".into(),
            "-x265-params".into(),
            "log-level=error".into(),
        ]);
    } else if name == "libx264" {
        if let Some(q) = q {
            a.extend(["-crf".into(), q.to_string()]);
        }
        a.extend(["-preset".into(), "slow".into()]);
    } else if name == "libsvtav1" {
        if let Some(q) = q {
            a.extend(["-crf".into(), q.to_string()]);
        }
        a.extend(["-preset".into(), "6".into()]);
    } else if name == "libaom-av1" {
        if let Some(q) = q {
            a.extend(["-crf".into(), q.to_string()]);
        }
        a.extend([
            "-b:v".into(),
            "0".into(),
            "-cpu-used".into(),
            "4".into(),
            "-row-mt".into(),
            "1".into(),
        ]);
    } else if name == "libvpx-vp9" {
        if let Some(q) = q {
            a.extend(["-crf".into(), q.to_string()]);
        }
        a.extend(["-b:v".into(), "0".into(), "-row-mt".into(), "1".into()]);
    } else if let Some(q) = q {
        a.extend(["-crf".into(), q.to_string()]);
    }

    // VAAPI frames already live on the GPU in the right format, so setting a
    // pixel format on the output would try to convert them back.
    if !vaapi_filter {
        a.extend(["-pix_fmt".into(), enc.pix_fmt.into()]);
    }
    // Without this tag, QuickTime and most 360 players refuse HEVC in MP4.
    if enc.codec == "hevc" {
        a.extend(["-tag:v".into(), "hvc1".into()]);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ff_at(path: &str) -> Ffmpeg {
        Ffmpeg {
            ffmpeg: PathBuf::from(path),
            ffprobe: PathBuf::from("ffprobe"),
            version: "ffmpeg version 9.0.2".into(),
        }
    }

    #[test]
    fn two_binaries_with_one_banner_do_not_share_a_cache() {
        // Same banner, different installs: a static build and a package-manager
        // build of one release can expose entirely different encoders.
        assert_ne!(cache_key(&ff_at("/opt/a/ffmpeg")), cache_key(&ff_at("/opt/b/ffmpeg")));
    }

    #[test]
    fn a_cache_for_another_binary_is_ignored() {
        let rows: Vec<_> = REGISTRY.iter().map(|e| (e, Status::Usable, None)).collect();
        let text = render_cache("key-a", &rows);
        assert!(parse_cache(&text, "key-a").is_some());
        assert!(parse_cache(&text, "key-b").is_none());
    }

    #[test]
    fn a_hardware_failure_is_remembered_only_for_a_while() {
        let rows: Vec<_> = REGISTRY
            .iter()
            .map(|e| match e.kind {
                Kind::Gpu => (e, Status::Unusable, Some(1_000)),
                Kind::Cpu => (e, Status::Usable, None),
            })
            .collect();
        let parsed = parse_cache(&render_cache("k", &rows), "k").unwrap();
        let (_, status, at) = parsed.iter().find(|(e, _, _)| e.kind == Kind::Gpu).unwrap();
        assert_eq!(*status, Status::Unusable);
        assert_eq!(*at, Some(1_000), "the failure keeps when it was seen");
        // Fresh within the TTL, retried after it; a success never expires.
        assert!(!expired(*at, 1_000 + FAILURE_TTL_S - 1));
        assert!(expired(*at, 1_000 + FAILURE_TTL_S));
        assert!(!expired(None, u64::MAX));
    }

    #[test]
    fn an_undated_failure_from_an_old_cache_is_retried() {
        let mut text = String::from("k\n");
        for e in REGISTRY {
            let word = if e.kind == Kind::Gpu { "unusable" } else { "usable" };
            text.push_str(&format!("{}\t{word}\n", e.name));
        }
        let parsed = parse_cache(&text, "k").unwrap();
        assert!(parsed
            .iter()
            .filter(|(_, s, _)| *s == Status::Unusable)
            .all(|(_, _, at)| expired(*at, unix_now())));
    }
}
