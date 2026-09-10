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
        Command::Merge(args) => cmd_merge(args, cli.quiet),
    }
}

fn cmd_merge(args: &gmax::cli::MergeArgs, quiet: bool) -> Result<()> {
    use gmax::cli::PreferArg;
    use gmax::merge;

    let external = gmax::gpx::read_file(&args.external)
        .with_context(|| format!("reading {}", args.external.display()))?;
    if external.points.is_empty() {
        anyhow::bail!("{} contains no track points", args.external.display());
    }

    // A video base knows where the receiver failed; a GPX base only knows where
    // it succeeded, and the gaps have to be reconstructed.
    let is_gpx = args
        .base
        .extension()
        .map(|e| e.eq_ignore_ascii_case("gpx"))
        .unwrap_or(false);

    let (slots, name) = if is_gpx {
        let base = gmax::gpx::read_file(&args.base)
            .with_context(|| format!("reading {}", args.base.display()))?;
        let name = base.name.clone();
        (merge::slots_from_gpx(&base), name)
    } else {
        let mut file = gmax::mp4::Mp4::open(&args.base)
            .with_context(|| format!("reading {}", args.base.display()))?;
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
            .base
            .file_stem()
            .map(|s| s.to_string_lossy().to_string());
        (merge::slots_from_telemetry(&t), name)
    };

    if slots.is_empty() {
        anyhow::bail!(
            "the base track has no timestamped positions, so there is nothing to align against"
        );
    }

    let opts = merge::Options {
        prefer: match args.prefer {
            PreferArg::QualityGated => merge::Prefer::QualityGated,
            PreferArg::External => merge::Prefer::External,
            PreferArg::Gopro => merge::Prefer::Gopro,
        },
        align: if args.no_align {
            merge::Align::None
        } else if let Some(o) = args.time_offset {
            merge::Align::Fixed(o)
        } else {
            merge::Align::Auto
        },
        lag: if args.no_gps_lag {
            merge::Lag::None
        } else if let Some(l) = args.gps_lag {
            merge::Lag::Fixed(l)
        } else {
            merge::Lag::Auto
        },
        span: args.align_span,
        max_dt: args.max_dt,
    };

    let (mut merged, report) = merge::merge(&slots, &external, &opts);
    merged.name = name;

    let path = args.out_path();
    let wopts = gmax::gpx::WriteOptions {
        activity: args.activity.clone(),
        ..Default::default()
    };
    gmax::gpx::write_file(&merged, &path, &wopts)
        .with_context(|| format!("writing {}", path.display()))?;

    if !quiet {
        print!("{}", report.summary());
        println!("wrote {} ({} track points)", path.display(), merged.points.len());
    }
    Ok(())
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
