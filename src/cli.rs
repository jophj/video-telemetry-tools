//! Command-line surface.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "gmax",
    version,
    about = "GoPro Max 2 toolkit: stitch, extract telemetry and GPX, rank moments, repair GPS",
    long_about = None,
    max_term_width = 100
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Print less. Warnings and errors still go to stderr.
    ///
    /// Long-form only: `convert` documents `-q` as its `--quality` preset, and
    /// a global short would claim that letter from every subcommand.
    #[arg(long, global = true)]
    pub quiet: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Extract telemetry from a .360 or .mp4 to CSV and/or JSON.
    ///
    /// Reads the GPMF track directly, so this needs no ffmpeg.
    Telemetry(TelemetryArgs),

    /// Extract a GPX track from a .360 or .mp4.
    Gpx(GpxArgs),

    /// Merge an external GPX into a GoPro track to repair poor GPS.
    Merge(MergeArgs),

    /// Rank the interesting moments of a ride.
    Analyse(AnalyseArgs),

    /// Stitch a .360 to an equirectangular video.
    Convert(ConvertArgs),

    /// Cut clips around the ride's interesting moments, or one by time.
    Clips(ClipsArgs),

    /// Serve the browser telemetry viewer.
    Viewer(ViewerArgs),

    /// List the video encoders this machine can really use.
    ///
    /// Hardware encoders are verified with a one-frame test encode, because
    /// ffmpeg advertises them even with no matching GPU in the box and they
    /// otherwise fail only once a real encode starts.
    Encoders(EncodersArgs),
}

#[derive(Args, Debug)]
pub struct ViewerArgs {
    /// The video to play. A stitched equirectangular file, or a proxy of one.
    pub video: PathBuf,

    /// Telemetry to overlay. Defaults to the telemetry inside the video, or a
    /// sibling CSV when the video has none.
    #[arg(long, value_name = "FILE")]
    pub telemetry: Option<PathBuf>,

    /// Port to listen on.
    #[arg(long, default_value_t = 8360)]
    pub port: u16,

    /// Do not open a browser.
    #[arg(long)]
    pub no_open: bool,

    /// Reject GPS fixes at or above this dilution of precision.
    #[arg(long, default_value_t = 5.0, value_name = "DOP")]
    pub max_dop: f64,
}

#[derive(Args, Debug)]
pub struct ClipsArgs {
    /// GoPro .360 or .mp4 file.
    pub input: PathBuf,

    /// Repair the camera's positions from an external GPS track before ranking.
    ///
    /// The same merge `gmax merge` performs, applied in place: the external
    /// receiver's positions are substituted where the camera's own quality
    /// metrics say its fixes are unsound, and the camera's IMU channels are
    /// kept throughout. Without this a ride whose receiver struggled is ranked,
    /// and cut, on positions it should not have trusted.
    #[arg(long, value_name = "GPX")]
    pub gps: Option<PathBuf>,

    /// Output directory. Defaults to <input stem>_highlights, or
    /// <input stem>_clips for a cut asked for by time.
    #[arg(short, long)]
    pub outdir: Option<PathBuf>,

    /// Cut one clip starting this many seconds in, instead of ranking moments.
    ///
    /// Named for ffmpeg's own seek flag, and the same as `gmax convert --ss`.
    /// The ranking options do not apply to such a cut and are refused with it;
    /// --gps and --max-dop still shape the telemetry written beside the clip.
    #[arg(
        long,
        value_name = "S",
        conflicts_with_all = [
            "top", "metric", "pre", "post", "merge_gap", "min_gap",
            "curve_threshold", "window",
        ]
    )]
    pub ss: Option<f64>,

    /// How long that clip runs. Defaults to the rest of the video.
    #[arg(long, value_name = "S", requires = "ss")]
    pub duration: Option<f64>,

    /// Moments to cut per metric.
    #[arg(long, default_value_t = 5, value_name = "N")]
    pub top: usize,

    /// Restrict to these metrics, comma separated.
    #[arg(long, value_name = "LIST", value_delimiter = ',')]
    pub metric: Option<Vec<String>>,

    /// Seconds of run-in before each moment.
    #[arg(long, default_value_t = 6.0, value_name = "S")]
    pub pre: f64,

    /// Seconds after each moment.
    #[arg(long, default_value_t = 4.0, value_name = "S")]
    pub post: f64,

    /// Merge clips that come within this of each other. Negative disables.
    #[arg(long, default_value_t = 2.0, value_name = "S", allow_hyphen_values = true)]
    pub merge_gap: f64,

    /// Minimum separation between reported moments, in seconds.
    #[arg(long, default_value_t = 5.0, value_name = "S")]
    pub min_gap: f64,

    /// Lean angle beyond which an excursion counts as a curve.
    #[arg(long, default_value_t = 15.0, value_name = "DEG")]
    pub curve_threshold: f64,

    /// Length of the curve-density window, in seconds.
    #[arg(long, default_value_t = 10.0, value_name = "S")]
    pub window: f64,

    /// Reject GPS fixes at or above this dilution of precision.
    #[arg(long, default_value_t = 5.0, value_name = "DOP")]
    pub max_dop: f64,

    /// Overwrite clips that already exist.
    #[arg(long)]
    pub overwrite: bool,

    /// Plan the clips and print them without writing anything.
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Path to the ffmpeg binary, if it is not on PATH.
    #[arg(long, value_name = "PATH")]
    pub ffmpeg: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ConvertArgs {
    /// GoPro .360 file.
    pub input: PathBuf,

    /// Output path. Defaults to <input stem>_equirect.<mp4|mov>.
    #[arg(short, long)]
    pub out: Option<PathBuf>,

    /// Quality target. A preset is a candidate list, not a fixed encoder: the
    /// first one that works on this machine wins, so run `gmax encoders` to see
    /// what each resolves to here.
    #[arg(short = 'q', long, value_enum, default_value_t = PresetArg::High)]
    pub quality: PresetArg,

    /// Encode with this specific encoder, e.g. libx265 or hevc_nvenc.
    #[arg(short = 'E', long, value_name = "NAME")]
    pub encoder: Option<String>,

    /// Rate-control value on the chosen encoder's own scale (CRF, CQ, QP, or
    /// 0-100 for VideoToolbox and Media Foundation).
    #[arg(long, value_name = "N")]
    pub enc_quality: Option<u32>,

    /// Restrict the encoder choice to hardware or software.
    #[arg(long, value_enum)]
    pub accel: Option<AccelArg>,

    /// Which audio to keep. Ambisonic forces a MOV container.
    #[arg(short = 'a', long, value_enum, default_value_t = AudioArg::Stereo)]
    pub audio: AudioArg,

    /// Level the horizon using the clip's mean gravity vector.
    #[arg(long)]
    pub level: bool,

    /// Apply an explicit yaw:pitch:roll rotation in degrees.
    ///
    /// allow_hyphen_values, because every one of these is routinely negative
    /// and clap would otherwise read "-45:118:0" as an unknown flag.
    #[arg(long, value_name = "Y:P:R", conflicts_with = "level", allow_hyphen_values = true)]
    pub rotate: Option<String>,

    /// Turn the levelled view by this many degrees about the vertical.
    ///
    /// Levelling fixes which way is up but not which way the centre of the
    /// frame looks; this chooses that, without disturbing the horizon.
    #[arg(long, value_name = "DEG", allow_hyphen_values = true, default_value_t = 0.0)]
    pub heading: f64,

    /// Print the rotation --level would apply, and exit.
    ///
    /// Use it to share one rotation across every clip of a recording.
    #[arg(long)]
    pub print_rotation: bool,

    /// Rotation order for the view transform.
    ///
    /// Defaults to pry rather than v360's own ypr so that yaw is applied last,
    /// about the already-levelled vertical, leaving heading and levelling
    /// independent.
    #[arg(long, default_value = "pry")]
    pub rorder: String,

    /// Interpolation used by the projection transform.
    #[arg(long, default_value = "lanczos")]
    pub interp: String,

    /// Output size as WxH. Defaults to the native angular resolution, which is
    /// four cube faces across.
    #[arg(long, value_name = "WxH")]
    pub size: Option<String>,

    /// Start this many seconds into the input.
    #[arg(long, value_name = "S")]
    pub ss: Option<f64>,

    /// Encode only this many seconds.
    #[arg(long, value_name = "S")]
    pub duration: Option<f64>,

    /// Write a single JPEG instead of a video, for checking framing quickly.
    #[arg(long)]
    pub preview: bool,

    /// Embed chapter markers from an ffmetadata file, as written by
    /// `gmax analyse --chapters`.
    #[arg(long, value_name = "FILE")]
    pub chapters: Option<PathBuf>,

    /// Drop the GPMF telemetry track from the output.
    #[arg(long)]
    pub no_telemetry: bool,

    /// ffmpeg filter threads.
    #[arg(short = 'j', long, value_name = "N")]
    pub jobs: Option<usize>,

    /// Overwrite an existing output.
    #[arg(long)]
    pub overwrite: bool,

    /// Print the ffmpeg command instead of running it.
    #[arg(long)]
    pub dry_run: bool,

    /// Path to the ffmpeg binary, if it is not on PATH.
    #[arg(long, value_name = "PATH")]
    pub ffmpeg: Option<PathBuf>,

    /// Re-test every encoder instead of using the cached result.
    #[arg(long)]
    pub reprobe: bool,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresetArg {
    /// Best available HEVC, hardware first.
    High,
    /// Software HEVC first: slowest, smallest transparent file.
    Max,
    /// Best available AV1: about half the size, less certain player support.
    Av1,
    /// ProRes 422 HQ, for handing to an editor.
    Prores,
    /// H.264, for maximum compatibility.
    H264,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioArg {
    Stereo,
    Ambisonic,
    Both,
    None,
}

#[derive(Args, Debug)]
pub struct EncodersArgs {
    /// Restrict the listing to hardware or software encoders.
    #[arg(long, value_enum)]
    pub accel: Option<AccelArg>,

    /// Re-test every encoder instead of using the cached result.
    #[arg(long)]
    pub reprobe: bool,

    /// Path to the ffmpeg binary, if it is not on PATH.
    #[arg(long, value_name = "PATH")]
    pub ffmpeg: Option<PathBuf>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccelArg {
    /// Hardware encoders only.
    Gpu,
    /// Software encoders only.
    Cpu,
}

#[derive(Args, Debug)]
pub struct AnalyseArgs {
    /// A GoPro .360/.mp4, a telemetry CSV, or a GPX track.
    pub input: PathBuf,

    /// Repair the camera's positions from an external GPS track before ranking.
    ///
    /// Only meaningful when the input is a video: it is the camera's own fixes
    /// that are being replaced. See `gmax merge`, which does the same thing and
    /// writes the merged track out instead of ranking it.
    #[arg(long, value_name = "GPX")]
    pub gps: Option<PathBuf>,

    /// Write the analysis as JSON instead of tables.
    #[arg(long)]
    pub json: bool,

    /// Write ffmpeg chapter markers to this path.
    ///
    /// Embed them with:
    ///   gmax convert in.360 --chapters FILE
    #[arg(long, value_name = "FILE")]
    pub chapters: Option<PathBuf>,

    /// Write a CMX3600 edit decision list to this path.
    #[arg(long, value_name = "FILE")]
    pub edl: Option<PathBuf>,

    /// Write a cue-sheet CSV to this path, for tools that will not read an EDL.
    #[arg(long, value_name = "FILE")]
    pub cues: Option<PathBuf>,

    /// Moments to report per metric.
    #[arg(long, default_value_t = 10, value_name = "N")]
    pub top: usize,

    /// Minimum separation between reported moments, in seconds.
    ///
    /// Without this a top-ten list is not ten moments: one hard stop spanning
    /// three samples would take three of the slots.
    #[arg(long, default_value_t = 5.0, value_name = "S")]
    pub min_gap: f64,

    /// Lean angle beyond which an excursion counts as a curve.
    #[arg(long, default_value_t = 15.0, value_name = "DEG")]
    pub curve_threshold: f64,

    /// Length of the curve-density window, in seconds.
    #[arg(long, default_value_t = 10.0, value_name = "S")]
    pub window: f64,

    /// Multiple of the median sample interval above which a gap breaks a
    /// derived channel rather than producing a false spike.
    #[arg(long, default_value_t = 5.0, value_name = "N")]
    pub max_gap: f64,

    /// Where longitudinal acceleration comes from.
    #[arg(long, value_enum, default_value_t = AccelSourceArg::Gps)]
    pub accel_source: AccelSourceArg,

    /// Restrict to these metrics, comma separated. Also sets report order.
    #[arg(long, value_name = "LIST", value_delimiter = ',')]
    pub metric: Option<Vec<String>>,

    /// Seconds of run-in before each moment, for chapters and the EDL.
    #[arg(long, default_value_t = 6.0, value_name = "S")]
    pub pre: f64,

    /// Seconds after each moment.
    #[arg(long, default_value_t = 4.0, value_name = "S")]
    pub post: f64,

    /// Merge windows that come within this of each other. Negative disables.
    #[arg(long, default_value_t = 2.0, value_name = "S")]
    pub merge_gap: f64,

    /// Reject GPS fixes at or above this dilution of precision.
    #[arg(long, default_value_t = 5.0, value_name = "DOP")]
    pub max_dop: f64,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccelSourceArg {
    /// Differentiate GPS speed. Comparable across every source.
    Gps,
    /// Use the IMU's vehicle-frame longitudinal channel, where there is one.
    Imu,
}

#[derive(Args, Debug)]
pub struct MergeArgs {
    /// The base track: a GoPro .360/.mp4, or a GPX already extracted from one.
    ///
    /// A video is the better input: it still carries the instants where the
    /// receiver reported a bad position, so the gaps are known rather than
    /// inferred from the surviving points.
    pub base: PathBuf,

    /// The external GPS source, as GPX.
    #[arg(long = "with", value_name = "FILE")]
    pub external: PathBuf,

    /// Output path. Defaults to <base stem>.merged.gpx.
    #[arg(short, long)]
    pub out: Option<PathBuf>,

    /// Which source wins at each point.
    #[arg(long, value_enum, default_value_t = PreferArg::QualityGated)]
    pub prefer: PreferArg,

    /// Clock offset in seconds to add to the external timestamps, skipping the
    /// automatic search.
    #[arg(long, value_name = "S", conflicts_with = "no_align")]
    pub time_offset: Option<f64>,

    /// Assume the two clocks already agree.
    #[arg(long)]
    pub no_align: bool,

    /// Camera GPS position lag in seconds, skipping the automatic measurement.
    ///
    /// A receiver's positions can describe where you were a few seconds ago.
    /// The GoPro Max 2's are stale by about 4.4 s, which at road speed puts
    /// them 70 m behind. Measured automatically by default.
    #[arg(long, value_name = "S", conflicts_with = "no_gps_lag")]
    pub gps_lag: Option<f64>,

    /// Take the camera's position timestamps at face value.
    #[arg(long)]
    pub no_gps_lag: bool,

    /// Search this many seconds either side of zero for the offset.
    #[arg(long, default_value_t = 30.0, value_name = "S")]
    pub align_span: f64,

    /// Interpolate across samples up to this far apart (or 2.5 of the source's
    /// own intervals, if longer); otherwise use the nearest within this.
    #[arg(long, default_value_t = 1.0, value_name = "S")]
    pub max_dt: f64,

    /// Reject GoPro fixes at or above this dilution of precision.
    #[arg(long, default_value_t = 5.0, value_name = "DOP")]
    pub max_dop: f64,

    /// GPX <trk><type>.
    #[arg(long, default_value = "motorcycling")]
    pub activity: String,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum PreferArg {
    /// Keep camera positions where its own quality metrics say they are sound,
    /// and substitute the external source elsewhere.
    QualityGated,
    /// Take the external position wherever the external track reaches.
    External,
    /// Keep every camera position, however poor.
    Gopro,
}

impl MergeArgs {
    pub fn out_path(&self) -> PathBuf {
        self.out
            .clone()
            .unwrap_or_else(|| with_extension(&self.base, "merged.gpx"))
    }
}

#[derive(Args, Debug)]
pub struct GpxArgs {
    /// GoPro .360, .mp4 or .lrv file.
    pub input: PathBuf,

    /// Output path. Defaults to <input stem>.gpx.
    #[arg(short, long)]
    pub out: Option<PathBuf>,

    /// GPX <trk><type>.
    #[arg(long, default_value = "motorcycling")]
    pub activity: String,

    /// Reject GPS fixes at or above this dilution of precision.
    #[arg(long, default_value_t = 5.0, value_name = "DOP")]
    pub max_dop: f64,

    /// Omit the rider telemetry extensions, writing a plain track.
    #[arg(long)]
    pub no_extensions: bool,
}

impl GpxArgs {
    pub fn gpx_path(&self) -> PathBuf {
        self.out
            .clone()
            .unwrap_or_else(|| with_extension(&self.input, "gpx"))
    }
}

#[derive(Args, Debug)]
pub struct TelemetryArgs {
    /// GoPro .360, .mp4 or .lrv file.
    pub input: PathBuf,

    /// CSV output path. Defaults to <input stem>.telemetry.csv.
    #[arg(short, long)]
    pub out: Option<PathBuf>,

    /// Skip the CSV.
    #[arg(long)]
    pub no_csv: bool,

    /// Also write the viewer's JSON. Defaults to <input stem>.telemetry.json
    /// when given without a path.
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    pub json: Option<PathBuf>,

    /// Reject GPS fixes at or above this dilution of precision.
    #[arg(long, default_value_t = 5.0, value_name = "DOP")]
    pub max_dop: f64,

    /// Reject a fix whose position is bit-identical to this many predecessors.
    ///
    /// The camera freezes its last position on losing lock; a real receiver
    /// jitters even at a standstill. Zero disables the check.
    #[arg(long, default_value_t = 30, value_name = "N")]
    pub frozen_run: usize,
}

impl TelemetryArgs {
    /// Where the CSV goes: the explicit path, else `<stem>.telemetry.csv`.
    ///
    /// The prototypes disagreed here — one used `<stem>.telemetry.csv` and
    /// another expected `<full name>.telemetry.csv` — so they could not find
    /// each other's output. This is the one convention.
    pub fn csv_path(&self) -> PathBuf {
        self.out
            .clone()
            .unwrap_or_else(|| with_extension(&self.input, "telemetry.csv"))
    }

    pub fn json_path(&self) -> Option<PathBuf> {
        match &self.json {
            None => None,
            Some(p) if p.as_os_str().is_empty() => {
                Some(with_extension(&self.input, "telemetry.json"))
            }
            Some(p) => Some(p.clone()),
        }
    }
}

/// Replace a path's extension, e.g. `GS010527.360` -> `GS010527.telemetry.csv`.
pub fn with_extension(path: &std::path::Path, ext: &str) -> PathBuf {
    let mut p = path.to_path_buf();
    p.set_extension(ext);
    p
}

/// Refuse to write `output` when it is the very file `input` names.
///
/// `-o` makes this one typo away — `gmax gpx ride.360 -o ride.360` — and the
/// cost is the recording itself, which nothing can regenerate. The comparison
/// is by what the paths resolve to rather than how they are spelled, so
/// `./ride.360`, `a/../ride.360` and a symlink to it are all caught; an output
/// that does not exist yet is resolved through its parent directory.
pub fn refuse_to_overwrite(input: &std::path::Path, output: &std::path::Path) -> anyhow::Result<()> {
    if same_file(input, output) {
        anyhow::bail!(
            "refusing to write {}: it is the input file, and writing it would destroy the \
             recording. Choose another output path.",
            output.display()
        );
    }
    Ok(())
}

/// Whether two paths name the same file. `b` need not exist.
pub fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    use std::path::Path;
    let Ok(ca) = std::fs::canonicalize(a) else {
        // An input that does not exist cannot be overwritten.
        return false;
    };
    if let Ok(cb) = std::fs::canonicalize(b) {
        // Both exist. On a case-insensitive volume two spellings can resolve
        // to different strings and still be one file, so ask the filesystem.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let (Ok(ma), Ok(mb)) = (std::fs::metadata(&ca), std::fs::metadata(&cb)) {
                return ma.dev() == mb.dev() && ma.ino() == mb.ino();
            }
        }
        return ca == cb;
    }
    let Some(name) = b.file_name() else {
        return false;
    };
    let parent = b
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::canonicalize(parent)
        .map(|p| p.join(name) == ca)
        .unwrap_or(false)
}
