use anyhow::{Context, Result};
use clap::Parser;

use gmax::cli::{Cli, Command};
use gmax::telemetry;

fn main() {
    if let Err(err) = run() {
        eprintln!("gmax: {err}");
        for cause in err.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Command::Telemetry(args) => cmd_telemetry(args, cli.quiet),
        Command::Gpx(args) => cmd_gpx(args, cli.quiet),
    }
}

fn cmd_gpx(args: &gmax::cli::GpxArgs, quiet: bool) -> Result<()> {
    let mut file = gmax::mp4::Mp4::open(&args.input)
        .with_context(|| format!("reading {}", args.input.display()))?;
    let gpmf = gmax::gpmf::from_mp4(&mut file)?;
    let opts = telemetry::Options {
        max_dop: args.max_dop,
        ..Default::default()
    };
    let t = telemetry::compute(&gpmf, &opts)?;
    for w in &t.warnings {
        eprintln!("gmax: warning: {w}");
    }

    let name = args
        .input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string());
    let track = gmax::gpx::write::from_telemetry(&t, name);
    let path = args.gpx_path();
    let wopts = gmax::gpx::WriteOptions {
        activity: args.activity.clone(),
        extensions: !args.no_extensions,
        ..Default::default()
    };
    gmax::gpx::write_file(&track, &path, &wopts)
        .with_context(|| format!("writing {}", path.display()))?;
    if !quiet {
        println!("wrote {} ({} track points)", path.display(), track.points.len());
    }
    Ok(())
}

fn cmd_telemetry(args: &gmax::cli::TelemetryArgs, quiet: bool) -> Result<()> {
    let mut file = gmax::mp4::Mp4::open(&args.input)
        .with_context(|| format!("reading {}", args.input.display()))?;
    let gpmf = gmax::gpmf::from_mp4(&mut file)?;

    let opts = telemetry::Options {
        max_dop: args.max_dop,
        frozen_run: args.frozen_run,
        ..Default::default()
    };
    let t = telemetry::compute(&gpmf, &opts)?;

    for w in &t.warnings {
        eprintln!("gmax: warning: {w}");
    }

    if !args.no_csv {
        let path = args.csv_path();
        telemetry::output::write_csv(&t, &path)
            .with_context(|| format!("writing {}", path.display()))?;
        if !quiet {
            println!("wrote {} ({} rows)", path.display(), t.n);
        }
    }

    if let Some(path) = args.json_path() {
        let video = args.input.file_name().map(|s| s.to_string_lossy().to_string());
        telemetry::output::write_json(
            &t,
            &path,
            video.as_deref(),
            Some(file.duration_secs()),
            None,
        )
        .with_context(|| format!("writing {}", path.display()))?;
        if !quiet {
            println!("wrote {}", path.display());
        }
    }

    if !quiet {
        print!("{}", telemetry::output::summary(&t));
    }
    Ok(())
}
