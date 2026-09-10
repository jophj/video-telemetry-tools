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
    #[arg(short, long, global = true)]
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
}

#[derive(Args, Debug)]
pub struct AnalyseArgs {
    /// A GoPro .360/.mp4, a telemetry CSV, or a GPX track.
    pub input: PathBuf,

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

    /// Reject a substitution whose bracketing external points are further away
    /// than this in time.
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
