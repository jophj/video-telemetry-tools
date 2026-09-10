//! Stitching a `.360` to equirectangular, and cutting clips from one.
//!
//! The geometry here is measured, not guessed. Each of the two HEVC tracks in a
//! `.360` is a horizontal strip of three faces of an in-camera equi-angular
//! cubemap: track 0 carries left, front and right, track 1 carries down, back
//! and up. The outer face of each strip is stored split in half with a band of
//! duplicated angle at the fisheye seam, which is why the graph below crops
//! four pieces per split face and cross-fades the middle two.

use std::path::{Path, PathBuf};

use crate::ffmpeg::{encoders, Encoder, Ffmpeg, Kind, Preset};
use crate::math::*;

/// Frame layout of one `.360` video track.
#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    /// Cube face size, which is the track height.
    pub face: u32,
    /// Width of the duplicated-angle band at each fisheye seam.
    pub overlap: u32,
    /// Face pixels stored before the overlap band.
    pub segment: u32,
    /// Stored width of one half of a split face.
    pub panel: u32,
}

impl Geometry {
    /// Derive the layout from a track's dimensions.
    ///
    /// `width = 3 * face + 2 * overlap` is the load-bearing identity: it holds
    /// for the Max 2 at 3008x960 (face 960, overlap 64) and for the original
    /// MAX at 4096x1344 (face 1344, overlap 32), so the same code handles both.
    pub fn derive(width: u32, height: u32) -> anyhow::Result<Geometry> {
        let face = height;
        if width < 3 * face {
            anyhow::bail!(
                "a {width}x{height} track is too narrow to hold three {face} px faces; this does \
                 not look like a GoPro .360 strip"
            );
        }
        let overlap = (width - 3 * face) / 2;
        if overlap == 0 {
            anyhow::bail!(
                "a {width}x{height} track leaves no overlap band between faces, so the fisheye \
                 seams cannot be blended"
            );
        }
        if overlap % 2 != 0 {
            anyhow::bail!("the overlap band is {overlap} px, which is odd and cannot be halved");
        }
        if 3 * face + 2 * overlap != width {
            anyhow::bail!(
                "a {width}x{height} track does not decompose as 3 x {face} + 2 x {overlap}"
            );
        }
        Ok(Geometry {
            width,
            height,
            face,
            overlap,
            segment: face / 2 - overlap / 2,
            panel: face / 2 + overlap / 2,
        })
    }

    /// Native equirectangular output size.
    ///
    /// An equi-angular cubemap is uniform at 90 degrees per face, so a face of
    /// `n` pixels carries `n/90` pixels per degree; four faces across is
    /// exactly that resolution in an equirectangular frame. For the Max 2 that
    /// is 3840x1920.
    pub fn native_output(&self) -> (u32, u32) {
        (self.face * 4, self.face * 2)
    }
}

/// The streams a `.360` carries, discovered rather than assumed.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Both HEVC strips, in track order.
    pub video: Vec<usize>,
    /// The stereo AAC track.
    pub audio: Option<usize>,
    /// The four-channel ambisonic PCM track.
    pub ambisonic: Option<usize>,
    /// The GPMF telemetry track.
    pub gpmd: Option<usize>,
    /// Timecode tracks, which are dropped from cut clips.
    pub timecode: Vec<usize>,
    pub geometry: Geometry,
    pub frame_rate: f64,
}

/// Inspect a `.360` and work out what is in it.
///
/// The second video track's index is not stable — 4 on one clip, 5 on another,
/// depending on how many audio tracks the camera wrote — so nothing here may be
/// hardcoded.
pub fn inspect(ff: &Ffmpeg, path: &Path) -> anyhow::Result<Layout> {
    let streams = ff.streams(path)?;
    let mut video = Vec::new();
    let mut audio = None;
    let mut ambisonic = None;
    let mut gpmd = None;
    let mut timecode = Vec::new();
    let mut geometry = None;
    let mut frame_rate = 30000.0 / 1001.0;

    for s in &streams {
        match s.codec_type.as_str() {
            "video" => {
                if let (Some(w), Some(h)) = (s.width, s.height) {
                    if geometry.is_none() {
                        geometry = Some(Geometry::derive(w, h)?);
                        if let Some(r) = s.frame_rate {
                            frame_rate = r;
                        }
                    }
                }
                video.push(s.index);
            }
            "audio" => match s.channels {
                // Four channels or more is the ambisonic track.
                Some(c) if c >= 4 => ambisonic = ambisonic.or(Some(s.index)),
                _ if s.codec_name == "aac" => audio = audio.or(Some(s.index)),
                _ => {}
            },
            _ if s.codec_tag == "gpmd" => gpmd = Some(s.index),
            _ if s.codec_tag == "tmcd" => timecode.push(s.index),
            _ => {}
        }
    }

    let geometry = geometry
        .ok_or_else(|| anyhow::anyhow!("{} has no video track", path.display()))?;
    if video.len() < 2 {
        anyhow::bail!(
            "{} has {} video track(s); a GoPro .360 has two, one strip per lens. A file already \
             stitched to equirectangular cannot be stitched again.",
            path.display(),
            video.len()
        );
    }
    Ok(Layout {
        video,
        audio,
        ambisonic,
        gpmd,
        timecode,
        geometry,
        frame_rate,
    })
}

/// Which audio to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Audio {
    /// The stereo AAC track.
    #[default]
    Stereo,
    /// The four-channel ambisonic track, which forces a MOV container.
    Ambisonic,
    Both,
    None,
}

/// A view rotation in degrees, applied by `v360`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rotation {
    pub yaw: f64,
    pub pitch: f64,
    pub roll: f64,
}

impl Rotation {
    pub fn is_zero(&self) -> bool {
        self.yaw == 0.0 && self.pitch == 0.0 && self.roll == 0.0
    }

    /// Parse a `yaw:pitch:roll` triple.
    pub fn parse(s: &str) -> anyhow::Result<Rotation> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 3 {
            anyhow::bail!("a rotation is three colon-separated numbers, e.g. 0:-71.7:6.7");
        }
        let n = |i: usize| -> anyhow::Result<f64> {
            parts[i]
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("{:?} is not a number", parts[i]))
        };
        Ok(Rotation {
            yaw: n(0)?,
            pitch: n(1)?,
            roll: n(2)?,
        })
    }
}

impl std::fmt::Display for Rotation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.4}:{:.4}:{:.4}", self.yaw, self.pitch, self.roll)
    }
}

/// The rotation order `gmax` uses, and why it is not the default.
///
/// `v360` applies yaw about the vertical, pitch about the lateral axis and roll
/// about the view axis, in the order this string names. Its default is `ypr`,
/// which applies yaw *first*: any yaw then moves the meridian that pitch is
/// about to act on, so choosing a viewing direction destroys the levelling.
///
/// With `pry` the yaw is applied last, about the already-levelled vertical, so
/// levelling and heading are independent. Measured on the reference clip, the
/// sky's latitude after levelling stays at +77.13 degrees for yaw 0, 45 and 90
/// under `pry`, and collapses to +34.65 and -13.06 under `ypr`.
pub const ROTATION_ORDER: &str = "pry";

/// Work out the rotation that levels the horizon, from the mean gravity vector.
///
/// `GRAV` is a unit vector pointing up in the camera's frame, so its average
/// over a clip is which way was up on average, and the rotation that carries
/// that direction to the output's zenith is the one that levels the horizon.
///
/// The convention was established by measurement rather than from the
/// documentation, by rendering probe frames and tracking where the sky went:
///
///   * yaw turns about the world's vertical, `+y` — a yaw of 30 leaves the
///     latitude untouched and shifts longitude by −30;
///   * pitch turns about the lateral axis, `x`, and a positive pitch raises
///     content — +30 lifted the sky from −18.3 to +13.2 degrees;
///   * roll turns about the view axis, `z`;
///   * and `v360`'s world frame coincides with the `GRAV` frame, confirmed by
///     locating the sky in an unrotated frame at (+0.333, −0.315, +0.889)
///     against a mean gravity of (−0.033, −0.468, +0.883).
///
/// So levelling is two angles: yaw the gravity vector's meridian round to face
/// away from the viewer, then pitch it up to the zenith. On the reference clip
/// that gives 2.2 and 117.9 degrees, and the sky's latitude peaks at exactly
/// 118 — +77.1 degrees, against −18.3 unrotated.
///
/// This is where the prototype went wrong, and by more than a sign. It computed
/// `pitch = atan2(gz, -gy)`, which measures gravity's angle from the camera's
/// `-y` axis rather than from the output's zenith, giving 62.1 degrees — the
/// complement of the 27.9 degrees that gravity sits below the equator, and not
/// the 117.9 degrees needed to reach the pole. Rendering at either sign of 62
/// leaves the frame in the tarmac or rolled on its side.
pub fn level_from_gravity(mean: Vec3) -> anyhow::Result<Rotation> {
    let magnitude = magnitude(&mean);
    // A clip whose mean gravity is short was tumbling, and no single rotation
    // levels it.
    if magnitude < 0.5 {
        anyhow::bail!(
            "mean gravity has magnitude {magnitude:.3}, too small to define a level horizon; \
             the camera moved through too many orientations for one rotation to level them"
        );
    }
    let g = normalize(&mean);
    let (gx, gy, gz) = (g[0], g[1], g[2]);

    // Longitude of the up direction, in the same convention the output uses:
    // lon = atan2(x, -z), so lon = 180 degrees is straight along +z.
    let lon = gx.atan2(-gz).to_degrees();
    // Bring that meridian to 180 degrees, where pitch acts on it. Wrapped into
    // (-180, 180] so the reported figure reads as a small correction rather
    // than as -357.84 degrees.
    let yaw = wrap_degrees(lon - 180.0);
    // Remaining angle from the zenith, in the plane pitch turns in.
    let horizontal = (gx * gx + gz * gz).sqrt();
    let pitch = horizontal.atan2(gy).to_degrees();

    Ok(Rotation {
        yaw,
        pitch,
        // Roll turns about the view axis, which after levelling is horizontal,
        // so any roll would tilt the horizon straight back over.
        roll: 0.0,
    })
}

/// Wrap an angle into `(-180, 180]`.
fn wrap_degrees(d: f64) -> f64 {
    let mut d = d % 360.0;
    if d > 180.0 {
        d -= 360.0;
    }
    if d <= -180.0 {
        d += 360.0;
    }
    d
}

/// Mean gravity over a clip, straight from the GPMF track.
pub fn mean_gravity(path: &Path) -> anyhow::Result<(Vec3, usize)> {
    use crate::fourcc::FourCc;
    let mut file = crate::mp4::Mp4::open(path)?;
    let gpmf = crate::gpmf::from_mp4(&mut file)?;
    let series = gpmf.series();
    let grav = series
        .iter()
        .find(|((_, key), _)| *key == FourCc::from_str_lossy("GRAV"))
        .map(|(_, s)| s)
        .ok_or_else(|| anyhow::anyhow!("{} has no GRAV stream", path.display()))?;
    let n = grav.len();
    if n == 0 {
        anyhow::bail!("the GRAV stream is empty");
    }
    let mut sum = [0.0f64; 3];
    for i in 0..n {
        let s = grav.sample(i);
        for k in 0..3 {
            sum[k] += s[k];
        }
    }
    Ok((scale(&sum, 1.0 / n as f64), n))
}

/// Build the stitching filter graph.
///
/// Each split face is reassembled from four crops: the two outer segments, and
/// the two halves of the duplicated-angle band, which are cross-faded so the
/// fisheye seam disappears. `format=yuv444p` comes *before* the geometric
/// transform deliberately — it keeps the resampling's intermediate precision off
/// the 4:2:0 chroma grid, where it would otherwise be visible as colour
/// fringing along the seams.
pub fn filter_graph(
    layout: &Layout,
    out_w: u32,
    out_h: u32,
    rotation: Rotation,
    rorder: &str,
    interp: &str,
    hw_upload: Option<&str>,
) -> String {
    let g = layout.geometry;
    let (face, ovl, seg, panel) = (g.face, g.overlap, g.segment, g.panel);
    let mid = 2 * panel;
    let right = 2 * panel + face;

    let mut parts: Vec<String> = Vec::new();

    // Nine copies of each track: four for each of the two split faces, one for
    // the whole face in the middle.
    for (t, &idx) in layout.video.iter().take(2).enumerate() {
        parts.push(format!(
            "[0:{idx}]format=yuv444p,split=9{}",
            (0..9).map(|s| format!("[t{t}_{s}]")).collect::<String>()
        ));
    }

    // One split face: two segments plus a cross-faded overlap band.
    let split_face = |track: usize, slot: usize, base: u32, label: &str| -> String {
        format!(
            "[t{track}_{s0}]crop={seg}:{face}:{base}:0[{label}a];\
             [t{track}_{s1}]crop={ovl}:{face}:{b1}:0[{label}oA];\
             [t{track}_{s2}]crop={ovl}:{face}:{b2}:0[{label}oB];\
             [{label}oA][{label}oB]blend=all_expr='A*(1-(X+0.5)/{ovl})+B*((X+0.5)/{ovl})'[{label}o];\
             [t{track}_{s3}]crop={seg}:{face}:{b3}:0[{label}b];\
             [{label}a][{label}o][{label}b]hstack=3[{label}]",
            s0 = slot,
            s1 = slot + 1,
            s2 = slot + 2,
            s3 = slot + 3,
            b1 = base + seg,
            b2 = base + panel,
            b3 = base + panel + ovl,
        )
    };

    // Track 0 is left, front, right; track 1 is down, back, up. The order was
    // established by pushing a direction-encoded test sphere through
    // v360=e:eac and reading off where each face landed.
    parts.push(split_face(0, 0, 0, "FL"));
    parts.push(format!("[t0_4]crop={face}:{face}:{mid}:0[FF]"));
    parts.push(split_face(0, 5, right, "FR"));
    parts.push(split_face(1, 0, 0, "FD"));
    parts.push(format!("[t1_4]crop={face}:{face}:{mid}:0[FB]"));
    parts.push(split_face(1, 5, right, "FU"));

    // Reassemble as ffmpeg's 3x2 equi-angular cubemap layout.
    parts.push("[FL][FF][FR]hstack=3[row0]".to_string());
    parts.push("[FD][FB][FU]hstack=3[row1]".to_string());
    parts.push("[row0][row1]vstack=2[eac]".to_string());

    let mut v360 = format!(
        "[eac]v360=input=eac:output=e:w={out_w}:h={out_h}:interp={interp}"
    );
    if !rotation.is_zero() {
        v360.push_str(&format!(
            ":yaw={:.4}:pitch={:.4}:roll={:.4}:rorder={rorder}",
            rotation.yaw, rotation.pitch, rotation.roll
        ));
    }
    if let Some(pix) = hw_upload {
        // VAAPI encodes from GPU memory, so the frames have to be uploaded
        // after the transform rather than before it.
        v360.push_str(&format!(",format={pix},hwupload"));
    }
    v360.push_str("[vout]");
    parts.push(v360);

    parts.join(";")
}

#[derive(Debug, Clone)]
pub struct ConvertOptions {
    pub preset: Preset,
    pub encoder: Option<String>,
    pub quality: Option<u32>,
    pub accel: Option<Kind>,
    pub audio: Audio,
    /// Copy the GPMF telemetry track into the output.
    pub keep_telemetry: bool,
    pub rotation: Option<Rotation>,
    /// Derive the rotation from mean gravity.
    pub level: bool,
    pub rorder: String,
    pub interp: String,
    /// Output size, defaulting to the native angular resolution.
    pub size: Option<(u32, u32)>,
    pub start_s: Option<f64>,
    pub duration_s: Option<f64>,
    /// Write one JPEG instead of a video.
    pub preview: bool,
    pub jobs: Option<usize>,
    /// An ffmetadata file whose chapters are embedded.
    pub chapters: Option<PathBuf>,
    pub overwrite: bool,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        ConvertOptions {
            preset: Preset::High,
            encoder: None,
            quality: None,
            accel: None,
            audio: Audio::Stereo,
            keep_telemetry: true,
            rotation: None,
            level: false,
            rorder: ROTATION_ORDER.into(),
            interp: "lanczos".into(),
            size: None,
            start_s: None,
            duration_s: None,
            preview: false,
            jobs: None,
            chapters: None,
            overwrite: false,
        }
    }
}

/// The container an output needs.
///
/// ProRes and ambisonic audio both require MOV: the MP4 muxer rejects a
/// four-channel `ambisonic 1` layout outright.
pub fn container_for(encoder: &Encoder, audio: Audio) -> &'static str {
    if encoder.codec == "prores" || matches!(audio, Audio::Ambisonic | Audio::Both) {
        "mov"
    } else {
        "mp4"
    }
}

/// Assemble the ffmpeg command line for a conversion.
pub fn convert_args(
    layout: &Layout,
    input: &Path,
    output: &Path,
    encoder: &Encoder,
    vaapi_device: Option<&Path>,
    opts: &ConvertOptions,
) -> Vec<String> {
    let (out_w, out_h) = opts.size.unwrap_or_else(|| layout.geometry.native_output());
    let vaapi = encoder.name.ends_with("_vaapi");
    let graph = filter_graph(
        layout,
        out_w,
        out_h,
        opts.rotation.unwrap_or_default(),
        &opts.rorder,
        &opts.interp,
        vaapi.then_some(encoder.pix_fmt),
    );

    let mut a: Vec<String> = vec!["-hide_banner".into()];
    // The camera writes a rotation matrix that must not be applied twice.
    a.push("-noautorotate".into());
    if opts.overwrite {
        a.push("-y".into());
    } else {
        a.push("-n".into());
    }
    if let Some(j) = opts.jobs {
        a.extend(["-filter_threads".into(), j.to_string()]);
    }
    if let Some(dev) = vaapi_device {
        if vaapi {
            a.extend(["-vaapi_device".into(), dev.display().to_string()]);
        }
    }
    // Seeking before -i is an input seek, which snaps to a keyframe but avoids
    // decoding everything up to that point.
    if let Some(ss) = opts.start_s {
        a.extend(["-ss".into(), format!("{ss:.3}")]);
    }
    if let Some(t) = opts.duration_s {
        a.extend(["-t".into(), format!("{t:.3}")]);
    }
    a.extend(["-i".into(), input.display().to_string()]);

    let chapter_input = opts.chapters.is_some();
    if let Some(ch) = &opts.chapters {
        a.extend(["-i".into(), ch.display().to_string()]);
    }

    a.extend(["-filter_complex".into(), graph]);
    a.extend(["-map".into(), "[vout]".into()]);

    if opts.preview {
        // One frame, as a JPEG; nothing else is mapped.
        a.extend([
            "-frames:v".into(),
            "1".into(),
            "-q:v".into(),
            "2".into(),
            output.display().to_string(),
        ]);
        return a;
    }

    match opts.audio {
        Audio::Stereo | Audio::Both => {
            if let Some(i) = layout.audio {
                a.extend(["-map".into(), format!("0:{i}")]);
            }
        }
        _ => {}
    }
    if matches!(opts.audio, Audio::Ambisonic | Audio::Both) {
        if let Some(i) = layout.ambisonic {
            a.extend(["-map".into(), format!("0:{i}")]);
        }
    }
    if opts.keep_telemetry {
        if let Some(i) = layout.gpmd {
            // Carrying the telemetry forward keeps the output analysable, so
            // `gmax analyse` works on a stitched file too.
            a.extend([
                "-map".into(),
                format!("0:{i}"),
                "-c:d".into(),
                "copy".into(),
                "-copy_unknown".into(),
            ]);
        }
    }

    a.extend(["-map_metadata".into(), "0".into()]);
    if chapter_input {
        a.extend(["-map_chapters".into(), "1".into()]);
    }

    a.extend(encoders::video_args(encoder, opts.quality, vaapi));

    // The source is full-range bt709 and must be tagged as such, or players
    // stretch the levels a second time.
    a.extend([
        "-color_range".into(),
        "pc".into(),
        "-colorspace".into(),
        "bt709".into(),
        "-color_primaries".into(),
        "bt709".into(),
        "-color_trc".into(),
        "bt709".into(),
    ]);

    if matches!(opts.audio, Audio::None) {
        a.push("-an".into());
    } else {
        a.extend(["-c:a".into(), "copy".into()]);
    }
    a.extend(["-movflags".into(), "+faststart".into()]);
    a.push(output.display().to_string());
    a
}

/// Assemble the ffmpeg command line for a lossless cut.
///
/// Every stream is copied, so a clip keeps both lenses, the audio and the
/// telemetry and can be stitched or analysed exactly like the original. The
/// timecode tracks are dropped because a cut clip's timecode would be wrong,
/// and `-write_tmcd 0` stops the MOV muxer from helpfully generating fresh ones
/// from the inherited metadata — without it a clip comes out with two new
/// timecode tracks in place of the one that was removed.
///
/// Note that a stream copy can only begin at a keyframe, so the clip that lands
/// on disk starts at or before the requested instant and may run a second or so
/// longer than asked. Trimming to the exact frame would mean re-encoding, which
/// would defeat the point.
pub fn clip_args(
    layout: &Layout,
    input: &Path,
    output: &Path,
    start_s: f64,
    duration_s: f64,
    overwrite: bool,
) -> Vec<String> {
    let mut a: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
    a.push(if overwrite { "-y".into() } else { "-n".into() });
    a.extend([
        "-ss".into(),
        format!("{start_s:.3}"),
        "-i".into(),
        input.display().to_string(),
        "-t".into(),
        format!("{duration_s:.3}"),
        "-map".into(),
        "0".into(),
    ]);
    for i in &layout.timecode {
        a.extend(["-map".into(), format!("-0:{i}")]);
    }
    a.extend([
        "-c".into(),
        "copy".into(),
        "-copy_unknown".into(),
        // An input seek can leave the first packet with a negative timestamp.
        "-avoid_negative_ts".into(),
        "make_zero".into(),
        // Forced because the MP4 muxer rejects the ambisonic channel layout,
        // and a clip keeps every stream. The .360 extension is retained so the
        // clip is still recognisably a GoPro dual-lens file.
        "-f".into(),
        "mov".into(),
        "-write_tmcd".into(),
        "0".into(),
        output.display().to_string(),
    ]);
    a
}

/// Resolve which encoder to use.
pub fn choose_encoder(
    survey: &encoders::Survey,
    opts: &ConvertOptions,
) -> anyhow::Result<&'static Encoder> {
    if let Some(name) = &opts.encoder {
        // An explicitly named encoder is honoured even if it is not in the
        // registry, but a registry entry that is known-broken here is refused
        // up front rather than an hour into the encode.
        if let Some(e) = encoders::lookup(name) {
            match survey.status(name) {
                Some(encoders::Status::Usable) => return Ok(e),
                Some(encoders::Status::NotBuilt) => anyhow::bail!(
                    "this ffmpeg was built without {name}. Run `gmax encoders` to see what it has."
                ),
                Some(encoders::Status::Unusable) => anyhow::bail!(
                    "{name} is in this ffmpeg but could not encode a test frame here, which \
                     usually means the matching hardware is absent. Run `gmax encoders`."
                ),
                None => return Ok(e),
            }
        }
        anyhow::bail!(
            "{name} is not an encoder gmax knows how to drive. Run `gmax encoders` for the list."
        );
    }
    survey.resolve(opts.preset, opts.accel).ok_or_else(|| {
        anyhow::anyhow!(
            "no usable encoder for --quality {}{}. Run `gmax encoders` to see what this machine \
             can do.",
            opts.preset.as_str(),
            match opts.accel {
                Some(Kind::Gpu) => " with --accel gpu",
                Some(Kind::Cpu) => " with --accel cpu",
                None => "",
            }
        )
    })
}
