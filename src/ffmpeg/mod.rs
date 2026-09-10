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
//! against the ffmpeg version so the check is paid for once.

pub mod encoders;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// A located ffmpeg installation.
#[derive(Debug, Clone)]
pub struct Ffmpeg {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// First line of `ffmpeg -version`, used as the cache key.
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

    /// Verify one encoder by actually encoding a single frame with it.
    ///
    /// `-f null -` rather than a path, so there is no `/dev/null` versus `NUL`
    /// difference between platforms.
    pub fn test_encoder(&self, enc: &Encoder) -> bool {
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
            "color=c=black:s=320x240:r=1:d=1".into(),
        ]);
        if vaapi {
            // VAAPI encodes from GPU memory, so frames have to be uploaded.
            args.push("-vf".into());
            args.push(format!("format={},hwupload", enc.pix_fmt));
        }
        args.extend([
            "-frames:v".into(),
            "1".into(),
            "-c:v".into(),
            enc.name.into(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ]);
        self.run(&args).is_ok()
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
            &path.to_string_lossy(),
        ])?;
        Ok(out.trim().parse().unwrap_or(0.0))
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
            &path.to_string_lossy(),
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
