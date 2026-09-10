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
