//! Driving ffmpeg, and working out which encoders this machine can really use.
//!
//! ffmpeg is run as a child process rather than linked. That is what makes
//! Windows support cheap — no libav headers, no pkg-config, no build script —
//! and it costs nothing here, because the pipeline is one long filter graph per
//! output rather than a per-frame loop. It also means `telemetry`, `gpx`,
//! `merge` and `analyse` need no ffmpeg at all: they read the container
//! directly.
//!
//! The interesting problem is encoder selection. **Which encoders a build
//! exposes says nothing about which ones the machine can run.** The same ffmpeg
//! binary advertises `hevc_nvenc` with no NVIDIA card in the box and `h264_qsv`
//! with no Intel GPU, and both fail only once the real encode starts — which,
//! on a 25-minute 360 stitch, is an hour in. So every hardware encoder is
//! verified up front with a one-frame test encode, and the verdicts are cached
//! against the ffmpeg binary so the check is paid for once.

pub mod encoders;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub use encoders::{Encoder, Kind, Preset, Status};

#[derive(Debug, thiserror::Error)]
pub enum FfmpegError {
    #[error(
        "ffmpeg was not found. Install it, put it on PATH, or pass --ffmpeg <path>.\n\
         The telemetry, gpx, merge and analyse commands do not need it."
    )]
    NotFound,
    #[error("{program} failed with status {status}:\n{stderr}")]
    Failed {
        program: String,
        status: String,
        stderr: String,
    },
    #[error("could not run {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

type Result<T> = std::result::Result<T, FfmpegError>;

/// The frame size every hardware encoder is proven at during the survey.
///
/// The Max 2's native equirectangular output, so a verdict holds for the
/// common case without a second probe. A larger output, such as the original
/// MAX's 5376x2688, is re-proven at its real size just before the encode; see
/// `render::choose_encoder_for_size`.
pub const PROBE_SIZE: (u32, u32) = (3840, 1920);

/// How long a one-frame probe may take before it is presumed hung.
const PROBE_DEADLINE: Duration = Duration::from_secs(20);

/// A path as an ffmpeg or ffprobe URL that can only mean that file.
///
/// ffmpeg reads a leading `-` as an option and anything before a colon as a
/// protocol, so `-x.mp4` or `clip:1.mov` would be taken for something else
/// entirely. The explicit `file:` protocol has neither problem, and ffmpeg
/// still guesses the container from the extension after it.
pub fn file_url(path: &Path) -> String {
    format!("file:{}", path.display())
}

/// A located ffmpeg installation.
#[derive(Debug, Clone)]
pub struct Ffmpeg {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// First line of `ffmpeg -version`, shown in the survey and part of the
    /// cache key.
    pub version: String,
}

impl Ffmpeg {
    /// Locate ffmpeg and ffprobe.
    ///
    /// Order: an explicit path, then `$FFMPEG`/`$FFPROBE`, then `PATH`, then the
    /// places installers put it on each platform.
    pub fn find(explicit: Option<&Path>) -> Result<Ffmpeg> {
        let ffmpeg = match explicit {
            Some(p) => p.to_path_buf(),
            None => locate("ffmpeg").ok_or(FfmpegError::NotFound)?,
        };
        // Prefer the ffprobe sitting beside the ffmpeg we found, so a local
        // build is not silently mixed with a system one.
        let sibling = ffmpeg.parent().map(|d| d.join(exe("ffprobe")));
        let ffprobe = match sibling {
            Some(p) if p.is_file() => p,
            _ => locate("ffprobe").unwrap_or_else(|| PathBuf::from(exe("ffprobe"))),
        };
        let version = Command::new(&ffmpeg)
            .arg("-version")
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .unwrap_or_default();
        Ok(Ffmpeg {
            ffmpeg,
            ffprobe,
            version,
        })
    }

    /// Run ffmpeg, returning an error carrying its stderr.
    pub fn run(&self, args: &[String]) -> Result<()> {
        let out = Command::new(&self.ffmpeg)
            .args(args)
            // Without this, an ffmpeg inside a loop steals the terminal's stdin
            // and the loop stalls.
            .stdin(Stdio::null())
            .output()
            .map_err(|source| FfmpegError::Spawn {
                program: self.ffmpeg.display().to_string(),
                source,
            })?;
        if !out.status.success() {
            return Err(FfmpegError::Failed {
                program: "ffmpeg".into(),
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(())
    }

    /// Run ffmpeg, letting its progress reach the terminal.
    pub fn run_streaming(&self, args: &[String]) -> Result<()> {
        let status = Command::new(&self.ffmpeg)
            .args(args)
            .stdin(Stdio::null())
            .status()
            .map_err(|source| FfmpegError::Spawn {
                program: self.ffmpeg.display().to_string(),
                source,
            })?;
        if !status.success() {
            return Err(FfmpegError::Failed {
                program: "ffmpeg".into(),
                status: status.to_string(),
                stderr: "see the output above".into(),
            });
        }
        Ok(())
    }

    fn probe(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.ffprobe)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|source| FfmpegError::Spawn {
                program: self.ffprobe.display().to_string(),
                source,
            })?;
        if !out.status.success() {
            return Err(FfmpegError::Failed {
                program: "ffprobe".into(),
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Encoders this build exposes, which is not the same as can run.
    pub fn listed_encoders(&self) -> Vec<String> {
        Command::new(&self.ffmpeg)
            .args(["-hide_banner", "-encoders"])
            .stdin(Stdio::null())
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter_map(|l| {
                        let mut parts = l.split_whitespace();
                        let flags = parts.next()?;
                        // The flag column starts with V for a video encoder.
                        flags.starts_with('V').then(|| parts.next())?
                    })
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A VAAPI render node, on the platforms that have them.
    pub fn vaapi_device(&self) -> Option<PathBuf> {
        if !cfg!(target_os = "linux") {
            return None;
        }
        let dir = std::fs::read_dir("/dev/dri").ok()?;
        let mut nodes: Vec<PathBuf> = dir
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("renderD"))
                    .unwrap_or(false)
            })
            .collect();
        nodes.sort();
        nodes.into_iter().next()
    }

    /// Verify one encoder by actually encoding a single frame with it, at the
    /// size the survey assumes.
    pub fn test_encoder(&self, enc: &Encoder) -> bool {
        let (w, h) = PROBE_SIZE;
        self.test_encoder_at(enc, w, h)
    }

    /// Verify one encoder by encoding a single `w` x `h` frame with it.
    ///
    /// The frame is fed exactly as a real encode feeds it: the encoder's own
    /// pixel format and profile, from the same [`encoders::video_args`] the
    /// render uses. A tiny 8-bit probe passes on hardware that then refuses
    /// the real 10-bit stream, and on H.264 encoders whose width limit sits
    /// below a stitched frame, so the size matters as much as the format.
    ///
    /// `-f null -` rather than a path, so there is no `/dev/null` versus `NUL`
    /// difference between platforms. A wedged driver can hang an encoder
    /// rather than fail it, so the probe is given a deadline and a hang counts
    /// as a failure.
    pub fn test_encoder_at(&self, enc: &Encoder, w: u32, h: u32) -> bool {
        let mut args: Vec<String> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-nostdin".into(),
        ];
        let vaapi = enc.name.ends_with("_vaapi");
        if vaapi {
            match self.vaapi_device() {
                Some(dev) => {
                    args.push("-vaapi_device".into());
                    args.push(dev.display().to_string());
                }
                None => return false,
            }
        }
        args.extend([
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            format!("color=c=black:s={w}x{h}:r=1:d=1"),
        ]);
        if vaapi {
            // VAAPI encodes from GPU memory, so frames have to be uploaded.
            args.push("-vf".into());
            args.push(format!("format={},hwupload", enc.pix_fmt));
        }
        args.extend(["-frames:v".into(), "1".into()]);
        args.extend(encoders::video_args(enc, None, vaapi));
        args.extend(["-f".into(), "null".into(), "-".into()]);
        self.run_with_deadline(&args, PROBE_DEADLINE)
    }

    /// Whether `libplacebo` can run a reprojection shader here.
    ///
    /// The real shader is run, once, on a blank frame of the real atlas size:
    /// an ffmpeg built without libplacebo, a machine with no Vulkan device, a
    /// driver that cannot compile the shader and a shader this libplacebo
    /// rejects all fail the same way, which is the one answer wanted.
    pub fn test_libplacebo(&self, shader: &Path, atlas: (u32, u32), out: (u32, u32)) -> bool {
        let args: Vec<String> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-nostdin".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            format!("color=c=black:s={}x{}:r=1:d=1,format=yuv444p", atlas.0, atlas.1),
            "-vf".into(),
            format!(
                "libplacebo=w={}:h={}:format=yuv444p:dithering=none:custom_shader_path={}",
                out.0,
                out.1,
                shader.display()
            ),
            "-frames:v".into(),
            "1".into(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ];
        self.run_with_deadline(&args, PROBE_DEADLINE)
    }

    /// Run ffmpeg quietly, killing it if it outlives `deadline`.
    ///
    /// Only for probes, whose output is not wanted: a real encode can
    /// legitimately take hours and must never be cut off.
    fn run_with_deadline(&self, args: &[String], deadline: Duration) -> bool {
        let Ok(mut child) = Command::new(&self.ffmpeg)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if started.elapsed() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    }

    // ------------------------------------------------------------ ffprobe

    /// Duration in seconds from the container header.
    pub fn duration(&self, path: &Path) -> Result<f64> {
        let out = self.probe(&[
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
            &file_url(path),
        ])?;
        // A zero here would read as an empty file rather than as a failure,
        // and anything clamped against it would silently come out empty.
        out.trim().parse().map_err(|_| FfmpegError::Failed {
            program: "ffprobe".into(),
            status: "no duration".into(),
            stderr: format!("could not read a duration for {}: {:?}", path.display(), out.trim()),
        })
    }

    /// Every stream in the file.
    ///
    /// Parsed from ffprobe's JSON rather than its CSV. The CSV is positional
    /// but omits fields that do not apply to a stream *and* emits them in its
    /// own order rather than the requested one, so a video stream (no
    /// `channels`) and an audio stream (no `width`) shift the columns relative
    /// to each other. Keyed output cannot go wrong that way.
    pub fn streams(&self, path: &Path) -> Result<Vec<StreamInfo>> {
        let out = self.probe(&[
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_entries",
            "stream=index,codec_type,codec_name,codec_tag_string,channels,width,height,r_frame_rate",
            &file_url(path),
        ])?;
        let parsed: serde_json::Value = serde_json::from_str(&out).map_err(|e| {
            FfmpegError::Failed {
                program: "ffprobe".into(),
                status: "unparseable JSON".into(),
                stderr: e.to_string(),
            }
        })?;

        // ffprobe lists each stream of a .360 twice, because of the track
        // reference groups, so entries are de-duplicated by index. A BTreeMap
        // also puts them back in index order, which the CSV does not guarantee.
        let mut seen: std::collections::BTreeMap<usize, StreamInfo> = Default::default();
        let Some(streams) = parsed.get("streams").and_then(|s| s.as_array()) else {
            return Ok(Vec::new());
        };
        for s in streams {
            let Some(index) = s.get("index").and_then(|v| v.as_u64()) else {
                continue;
            };
            let text = |k: &str| {
                s.get(k)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            let number = |k: &str| s.get(k).and_then(|v| v.as_u64()).map(|v| v as u32);
            seen.entry(index as usize).or_insert_with(|| StreamInfo {
                index: index as usize,
                codec_type: text("codec_type"),
                codec_name: text("codec_name"),
                codec_tag: text("codec_tag_string"),
                channels: number("channels"),
                width: number("width"),
                height: number("height"),
                frame_rate: s
                    .get("r_frame_rate")
                    .and_then(|v| v.as_str())
                    .and_then(parse_rational),
            });
        }
        Ok(seen.into_values().collect())
    }
}

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub index: usize,
    pub codec_type: String,
    pub codec_name: String,
    pub codec_tag: String,
    pub channels: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f64>,
}

fn parse_rational(s: &str) -> Option<f64> {
    let (a, b) = s.split_once('/')?;
    let (a, b): (f64, f64) = (a.parse().ok()?, b.parse().ok()?);
    (b != 0.0).then(|| a / b)
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Find a program: environment override, then `PATH`, then the usual places.
fn locate(name: &str) -> Option<PathBuf> {
    let var = name.to_uppercase();
    if let Some(p) = std::env::var_os(&var) {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let file = exe(name);
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(&file);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    // Where the common installers put it, so that a user who has ffmpeg but no
    // PATH entry is not told it is missing.
    let fallbacks: &[&str] = if cfg!(target_os = "macos") {
        &["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"]
    } else if cfg!(windows) {
        &[
            r"C:\Program Files\ffmpeg\bin",
            r"C:\ffmpeg\bin",
            r"C:\Program Files (x86)\ffmpeg\bin",
        ]
    } else {
        &["/usr/bin", "/usr/local/bin", "/snap/bin"]
    };
    fallbacks
        .iter()
        .map(|d| Path::new(d).join(&file))
        .find(|p| p.is_file())
}
