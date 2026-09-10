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
        Command::Encoders(args) => cmd_encoders(args),
        Command::Convert(args) => cmd_convert(args, cli.quiet),
        Command::Clips(args) => cmd_clips(args, cli.quiet),
    }
}

fn cmd_clips(args: &gmax::cli::ClipsArgs, quiet: bool) -> Result<()> {
    use gmax::ffmpeg::Ffmpeg;
    use gmax::moments::{self, clips, MetricId, Options};

    let mut file = gmax::mp4::Mp4::open(&args.input)
        .with_context(|| format!("reading {}", args.input.display()))?;
    let container_duration = file.duration_secs();
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

    let metrics = match &args.metric {
        None => MetricId::ALL.to_vec(),
        Some(list) => list
            .iter()
            .map(|n| {
                MetricId::parse(n.trim())
                    .ok_or_else(|| anyhow::anyhow!("unknown metric {:?}", n.trim()))
            })
            .collect::<Result<Vec<_>>>()?,
    };

    let samples = moments::samples_from_telemetry(&t);
    let mut analysis = moments::analyse(
        &samples,
        &Options {
            top: args.top,
            min_gap_s: args.min_gap,
            curve_threshold_deg: args.curve_threshold,
            window_s: args.window,
            metrics,
            ..Default::default()
        },
    )?;
    analysis.source = args.input.display().to_string();

    let (planned, dropped) = clips::plan(
        &analysis,
        &clips::PlanOptions {
            pre_s: args.pre,
            post_s: args.post,
            merge_gap_s: args.merge_gap,
            duration_s: Some(container_duration),
            ..Default::default()
        },
    );
    if planned.is_empty() {
        anyhow::bail!(
            "no clips could be planned from {} moment(s); the ride may be too short or have no \
             usable telemetry",
            dropped.len()
        );
    }

    let stem = args
        .input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "ride".into());
    let outdir = args
        .outdir
        .clone()
        .unwrap_or_else(|| args.input.with_file_name(format!("{stem}_highlights")));
    // A .360 clip keeps both lenses, so its extension stays .360.
    let ext = args
        .input
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_else(|| "mp4".into());

    if !quiet || args.dry_run {
        println!(
            "{} clip(s) planned from {} moment(s), into {}",
            planned.len(),
            planned.iter().map(|c| c.moments.len()).sum::<usize>(),
            outdir.display()
        );
        for c in &planned {
            println!(
                "  {:>8.2}..{:<8.2} ({:>5.2}s)  {}",
                c.start_s,
                c.end_s,
                c.duration_s(),
                c.moments
                    .iter()
                    .map(|m| m.label())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        for d in &dropped {
            println!("  skipped {}: {}", d.moment.label(), d.reason);
        }
    }
    if args.dry_run {
        return Ok(());
    }

    let ff = Ffmpeg::find(args.ffmpeg.as_deref())?;
    let layout = gmax::render::inspect(&ff, &args.input)?;
    std::fs::create_dir_all(&outdir)
        .with_context(|| format!("creating {}", outdir.display()))?;

    let mut written = Vec::new();
    for c in &planned {
        let path = outdir.join(format!("{}.{ext}", c.name(&stem)));
        if path.exists() && !args.overwrite {
            if !quiet {
                println!("skipping {} (already exists)", path.display());
            }
            written.push((c, path));
            continue;
        }
        let cmd = gmax::render::clip_args(
            &layout,
            &args.input,
            &path,
            c.start_s,
            c.duration_s(),
            args.overwrite,
        );
        ff.run(&cmd)
            .with_context(|| format!("cutting {}", path.display()))?;
        if !quiet {
            println!("wrote {}", path.display());
        }
        written.push((c, path));
    }

    // A manifest, so the clips can be traced back to the moments that chose them.
    let manifest = serde_json::json!({
        "source": args.input.display().to_string(),
        "sourceDurationSeconds": container_duration,
        "window": { "preSeconds": args.pre, "postSeconds": args.post, "mergeGapSeconds": args.merge_gap },
        "clips": written.iter().map(|(c, path)| serde_json::json!({
            "file": path.file_name().map(|f| f.to_string_lossy().to_string()),
            "startSeconds": c.start_s,
            "endSeconds": c.end_s,
            "durationSeconds": c.duration_s(),
            "moments": c.moments.iter().map(|m| serde_json::json!({
                "metric": m.metric.slug(),
                "rank": m.entry.rank,
                "label": m.label(),
                "value": m.entry.value,
                "unit": m.metric.unit(),
                "videoTimeSeconds": m.entry.video_time_s,
                "lat": m.entry.pos.map(|(lat, _)| lat),
                "lon": m.entry.pos.map(|(_, lon)| lon),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "dropped": dropped.iter().map(|d| serde_json::json!({
            "label": d.moment.label(),
            "reason": d.reason,
        })).collect::<Vec<_>>(),
    });
    let manifest_path = outdir.join(format!("{stem}_highlights.json"));
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;
    if !quiet {
        println!("wrote {}", manifest_path.display());
    }
    Ok(())
}

fn cmd_convert(args: &gmax::cli::ConvertArgs, quiet: bool) -> Result<()> {
    use gmax::cli::{AccelArg, AudioArg, PresetArg};
    use gmax::ffmpeg::{encoders, Ffmpeg, Kind, Preset};
    use gmax::render::{self, Audio, ConvertOptions, Rotation};

    // The rotation comes from the telemetry, so it is known before ffmpeg is
    // needed at all -- which is why --print-rotation works without it.
    let mut rotation = if let Some(spec) = &args.rotate {
        Some(Rotation::parse(spec)?)
    } else if args.level || args.print_rotation {
        let (mean, n) = render::mean_gravity(&args.input)?;
        let r = render::level_from_gravity(mean)?;
        if !quiet && !args.print_rotation {
            println!("levelling from {n} gravity samples: {r}");
        }
        Some(r)
    } else {
        None
    };
    // Heading turns the levelled view about the vertical. With rorder=pry the
    // yaw is applied last, so this does not disturb the horizon.
    if args.heading != 0.0 {
        let mut r = rotation.unwrap_or_default();
        r.yaw += args.heading;
        rotation = Some(r);
    }

    if args.print_rotation {
        println!("{}", rotation.unwrap_or_default());
        return Ok(());
    }

    let ff = Ffmpeg::find(args.ffmpeg.as_deref())?;
    let layout = render::inspect(&ff, &args.input)?;

    let size = match &args.size {
        None => None,
        Some(spec) => {
            let (w, h) = spec
                .split_once(['x', 'X'])
                .ok_or_else(|| anyhow::anyhow!("--size wants WxH, e.g. 3840x1920"))?;
            Some((
                w.trim().parse().context("--size width")?,
                h.trim().parse().context("--size height")?,
            ))
        }
    };

    let opts = ConvertOptions {
        preset: match args.quality {
            PresetArg::High => Preset::High,
            PresetArg::Max => Preset::Max,
            PresetArg::Av1 => Preset::Av1,
            PresetArg::Prores => Preset::Prores,
            PresetArg::H264 => Preset::H264,
        },
        encoder: args.encoder.clone(),
        quality: args.enc_quality,
        accel: args.accel.map(|a| match a {
            AccelArg::Gpu => Kind::Gpu,
            AccelArg::Cpu => Kind::Cpu,
        }),
        audio: match args.audio {
            AudioArg::Stereo => Audio::Stereo,
            AudioArg::Ambisonic => Audio::Ambisonic,
            AudioArg::Both => Audio::Both,
            AudioArg::None => Audio::None,
        },
        keep_telemetry: !args.no_telemetry,
        rotation,
        level: args.level,
        rorder: args.rorder.clone(),
        interp: args.interp.clone(),
        size,
        start_s: args.ss,
        duration_s: args.duration,
        preview: args.preview,
        jobs: args.jobs,
        chapters: args.chapters.clone(),
        overwrite: args.overwrite,
    };

    let survey = encoders::survey(&ff, args.reprobe);
    let encoder = render::choose_encoder(&survey, &opts)?;

    let output = match &args.out {
        Some(p) => p.clone(),
        None => {
            let ext = if args.preview {
                "jpg"
            } else {
                render::container_for(encoder, opts.audio)
            };
            let stem = args
                .input
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "out".into());
            args.input
                .with_file_name(format!("{stem}_equirect.{ext}"))
        }
    };

    let vaapi = ff.vaapi_device();
    let cmd = render::convert_args(
        &layout,
        &args.input,
        &output,
        encoder,
        vaapi.as_deref(),
        &opts,
    );

    if args.dry_run {
        println!("{} {}", ff.ffmpeg.display(), shell_join(&cmd));
        return Ok(());
    }

    let (w, h) = opts.size.unwrap_or_else(|| layout.geometry.native_output());
    if !quiet {
        println!(
            "stitching {} to {w}x{h} with {} ({})",
            args.input.display(),
            encoder.name,
            encoder.kind.as_str()
        );
    }
    ff.run_streaming(&cmd)?;
    if !quiet {
        println!("wrote {}", output.display());
    }
    Ok(())
}

/// Quote a command for display, so a dry run can be pasted into a shell.
fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.chars()
                .all(|c| c.is_alphanumeric() || "-_./:=+".contains(c))
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn cmd_encoders(args: &gmax::cli::EncodersArgs) -> Result<()> {
    use gmax::cli::AccelArg;
    use gmax::ffmpeg::{encoders, Ffmpeg, Kind};

    let ff = Ffmpeg::find(args.ffmpeg.as_deref())?;
    let accel = args.accel.map(|a| match a {
        AccelArg::Gpu => Kind::Gpu,
        AccelArg::Cpu => Kind::Cpu,
    });
    let survey = encoders::survey(&ff, args.reprobe);
    print!("{}", survey.render(accel));
    Ok(())
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
