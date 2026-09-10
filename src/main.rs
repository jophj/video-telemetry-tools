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
        Command::Analyse(args) => cmd_analyse(args, cli.quiet),
    }
}

fn cmd_analyse(args: &gmax::cli::AnalyseArgs, quiet: bool) -> Result<()> {
    use gmax::cli::AccelSourceArg;
    use gmax::moments::{self, clips, AccelSource, MetricId};

    let ext = args
        .input
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    // Video duration is only knowable from a video, and clip windows need it
    // to clamp against.
    let mut duration_s = None;
    let (samples, source, declared) = match ext.as_str() {
        "gpx" => {
            let g = gmax::gpx::read_file(&args.input)
                .with_context(|| format!("reading {}", args.input.display()))?;
            let name = g.name.clone().unwrap_or_else(|| args.input.display().to_string());
            (moments::samples_from_gpx(&g), name, Vec::new())
        }
        "csv" => {
            let text = std::fs::read_to_string(&args.input)
                .with_context(|| format!("reading {}", args.input.display()))?;
            (
                moments::samples_from_csv(&text)?,
                args.input.display().to_string(),
                Vec::new(),
            )
        }
        _ => {
            let mut file = gmax::mp4::Mp4::open(&args.input)
                .with_context(|| format!("reading {}", args.input.display()))?;
            duration_s = Some(file.duration_secs());
            let gpmf = gmax::gpmf::from_mp4(&mut file)?;
            let t = telemetry::compute(
                &gpmf,
                &telemetry::Options {
                    max_dop: args.max_dop,
                    ..Default::default()
                },
            )?;
            for w in &t.warnings {
                eprintln!("gmax: warning: {w}");
            }
            // No declared rate is offered here: the samples are at the
            // receiver's rate, and quoting the 30-per-payload IMU rate beside
            // them would invite a comparison between two different things.
            (
                moments::samples_from_telemetry(&t),
                args.input.display().to_string(),
                Vec::new(),
            )
        }
    };

    let metrics = match &args.metric {
        None => MetricId::ALL.to_vec(),
        Some(list) => {
            let mut out = Vec::new();
            for name in list {
                let name = name.trim();
                if name.is_empty() {
                    continue;
                }
                match MetricId::parse(name) {
                    Some(m) => out.push(m),
                    None => anyhow::bail!(
                        "unknown metric {name:?}. Valid: {}",
                        MetricId::ALL
                            .iter()
                            .map(|m| m.slug())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            }
            if out.is_empty() {
                anyhow::bail!("--metric was empty");
            }
            out
        }
    };

    let opts = moments::Options {
        top: args.top,
        min_gap_s: args.min_gap,
        curve_threshold_deg: args.curve_threshold,
        window_s: args.window,
        max_gap_factor: args.max_gap,
        accel_source: match args.accel_source {
            AccelSourceArg::Gps => AccelSource::Gps,
            AccelSourceArg::Imu => AccelSource::Imu,
        },
        metrics,
        ..Default::default()
    };

    let mut analysis = moments::analyse(&samples, &opts)?;
    analysis.source = source;
    analysis.declared_hz = declared;

    // Chapters, the EDL and the cue sheet all describe the same windows.
    if args.chapters.is_some() || args.edl.is_some() || args.cues.is_some() {
        let plan_opts = clips::PlanOptions {
            pre_s: args.pre,
            post_s: args.post,
            merge_gap_s: args.merge_gap,
            duration_s,
            ..Default::default()
        };
        let (windows, dropped) = clips::plan(&analysis, &plan_opts);
        if windows.is_empty() {
            eprintln!(
                "gmax: warning: no clip windows could be planned; {} moment(s) were unusable",
                dropped.len()
            );
        }
        let stem = args
            .input
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "ride".into());

        if let Some(p) = &args.chapters {
            std::fs::write(p, clips::write_ffmetadata(&windows, Some(&stem)))
                .with_context(|| format!("writing {}", p.display()))?;
            if !quiet {
                println!("wrote {} ({} chapters)", p.display(), windows.len());
            }
        }
        if let Some(p) = &args.edl {
            // The EDL's timecodes are in whole frames, so it needs the video's
            // frame rate; 29.97 is the GoPro's.
            let fps = 30000.0 / 1001.0;
            std::fs::write(p, clips::write_edl(&windows, &stem, &stem, fps))
                .with_context(|| format!("writing {}", p.display()))?;
            if !quiet {
                println!("wrote {} ({} events at {fps:.3} fps)", p.display(), windows.len());
            }
        }
        if let Some(p) = &args.cues {
            std::fs::write(p, clips::write_cue_csv(&windows))
                .with_context(|| format!("writing {}", p.display()))?;
            if !quiet {
                println!("wrote {}", p.display());
            }
        }
        if !dropped.is_empty() && !quiet {
            println!(
                "{} moment(s) could not be turned into a clip, e.g. {}",
                dropped.len(),
                dropped[0].reason
            );
        }
    }

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&moments::output::render_json(&analysis))?
        );
    } else {
        print!("{}", moments::output::render_text(&analysis));
    }
    Ok(())
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
