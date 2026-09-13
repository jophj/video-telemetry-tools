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
        Command::Viewer(args) => cmd_viewer(args),
    }
}

fn cmd_viewer(args: &gmax::cli::ViewerArgs) -> Result<()> {
    // The viewer needs the telemetry as its own JSON. Three ways to get it, in
    // order of preference: an explicit file, a sibling CSV, or the video's own
    // GPMF track.
    //
    // The sibling beats the embedded track deliberately. A file someone put
    // there is a considered answer; a GPMF track is only raw sensor data, and
    // on a clip cut out of a longer ride it is data that cannot be calibrated
    // from what is in the clip — `gmax clips` leaves the ride's calibration
    // beside each clip for exactly this reason. Which one was used is always
    // announced, so preferring it is never silent.
    let json = if let Some(path) = &args.telemetry {
        eprintln!("gmax: telemetry from {}", path.display());
        telemetry_json_from(path, args.max_dop, Some(&args.video))?
    } else {
        // A stitched proxy has no telemetry of its own and its stem carries a
        // suffix the sibling's does not, so look under both spellings.
        let sibling = [
            Some(gmax::cli::with_extension(&args.video, "telemetry.csv")),
            strip_suffix_variant(&args.video),
        ]
        .into_iter()
        .flatten()
        .find(|p| p.exists());
        match sibling {
            Some(p) => {
                eprintln!("gmax: telemetry from {}", p.display());
                telemetry_json_from(&p, args.max_dop, Some(&args.video))?
            }
            None => {
                eprintln!("gmax: telemetry from the track inside {}", args.video.display());
                telemetry_json_from(&args.video, args.max_dop, Some(&args.video)).map_err(|e| {
                    e.context(
                        "no telemetry found: pass --telemetry, or run `gmax telemetry` on the \
                         original .360 first",
                    )
                })?
            }
        }
    };

    gmax::viewer::serve(gmax::viewer::Options {
        video: args.video.clone(),
        telemetry_json: json,
        port: args.port,
        open_browser: !args.no_open,
    })
}

/// `GS010527_proxy.mp4` -> `GS010527.telemetry.csv`.
///
/// A viewer is usually pointed at a stitched or proxy rendering, whose stem
/// carries a suffix the telemetry file does not have.
fn strip_suffix_variant(video: &std::path::Path) -> Option<std::path::PathBuf> {
    let stem = video.file_stem()?.to_string_lossy().to_string();
    for suffix in ["_equirect", "_proxy", "_stitched", "_hud"] {
        if let Some(base) = stem.strip_suffix(suffix) {
            return Some(video.with_file_name(format!("{base}.telemetry.csv")));
        }
    }
    None
}

/// Render the viewer's JSON from a video, a CSV or a GPX.
fn telemetry_json_from(
    path: &std::path::Path,
    max_dop: f64,
    video: Option<&std::path::Path>,
) -> Result<String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let video_name = video
        .and_then(|v| v.file_name())
        .map(|s| s.to_string_lossy().to_string());

    if ext == "csv" {
        // The viewer's contract is built from the CSV's own columns.
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        return gmax::telemetry::output::json_from_csv(&text, video_name.as_deref(), None)
            .with_context(|| format!("converting {}", path.display()));
    }

    let mut file = gmax::mp4::Mp4::open(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let duration = file.duration_secs();
    let gpmf = gmax::gpmf::from_mp4(&mut file)?;
    let t = telemetry::compute(
        &gpmf,
        &telemetry::Options {
            max_dop,
            ..Default::default()
        },
    )?;
    for w in &t.warnings {
        eprintln!("gmax: warning: {w}");
    }
    let mut out = Vec::new();
    gmax::telemetry::output::write_json_to(
        &t,
        &mut out,
        video_name.as_deref(),
        Some(duration),
        path.file_name().map(|s| s.to_string_lossy().to_string()).as_deref(),
    )?;
    Ok(String::from_utf8(out)?)
}

/// Where a clip's calibration sits: `<clip stem>.telemetry.csv`.
///
/// The name `gmax telemetry` already uses, and the one the viewer already
/// looks for beside a stitched rendering — so one file serves both the `.360`
/// clip and the `_equirect.mp4` made from it.
fn clip_telemetry_path(clip: &std::path::Path) -> std::path::PathBuf {
    gmax::cli::with_extension(clip, "telemetry.csv")
}

/// Write the parent ride's telemetry, sliced to one clip's own samples.
///
/// The range is taken from the cut file rather than from the planned window,
/// because a stream copy starts at the keyframe at or before the requested
/// instant and so runs a little long. The clip's GPMF keeps its parent's
/// timestamps, which is what lets the two be matched exactly instead of
/// approximately.
fn write_clip_telemetry(
    parent: &telemetry::Telemetry,
    clip: &std::path::Path,
    planned: &gmax::moments::clips::Clip,
) -> Result<()> {
    let (start, end) = match gmax::mp4::Mp4::open(clip)
        .ok()
        .and_then(|mut f| gmax::gpmf::from_mp4(&mut f).ok())
        .and_then(|g| telemetry::timeline(&g))
        .filter(|t| t.len() > 1)
    {
        Some(t) => (t[0], t[t.len() - 1]),
        // No GPMF in the cut file, or timestamps that were not carried through:
        // fall back to the window that was asked for. Off by up to a keyframe
        // interval, which is better than nothing to calibrate against.
        None => (planned.start_s, planned.end_s),
    };
    let (from, to) = parent.range_covering(start, end);
    if to <= from {
        anyhow::bail!("the clip's timeline does not overlap the source's");
    }
    telemetry::output::write_csv(&parent.slice(from, to), &clip_telemetry_path(clip))
        .with_context(|| format!("writing {}", clip_telemetry_path(clip).display()))
}

fn cmd_clips(args: &gmax::cli::ClipsArgs, quiet: bool) -> Result<()> {
    use gmax::ffmpeg::Ffmpeg;
    use gmax::moments::{self, clips, MetricId, Options};

    // Validated before any I/O, so a mistyped metric is reported as one.
    let metrics = match &args.metric {
        None => MetricId::ALL.to_vec(),
        Some(list) => list
            .iter()
            .map(|n| {
                MetricId::parse(n.trim()).ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown metric {:?}. Valid: {}",
                        n.trim(),
                        MetricId::ALL
                            .iter()
                            .map(|m| m.slug())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?,
    };

    let mut file = gmax::mp4::Mp4::open(&args.input)
        .with_context(|| format!("reading {}", args.input.display()))?;
    let container_duration = file.duration_secs();
    let gpmf = gmax::gpmf::from_mp4(&mut file)?;
    let mut t = telemetry::compute(
        &gpmf,
        &telemetry::Options {
            max_dop: args.max_dop,
            ..Default::default()
        },
    )?;
    for w in &t.warnings {
        eprintln!("gmax: warning: {w}");
    }
    if let Some(gpx) = &args.gps {
        repair_positions(&mut t, gpx, quiet)?;
    }

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

    // Give each clip the ride's calibration, as a sibling the viewer prefers
    // over the clip's own GPMF track.
    //
    // The track is still in there and still correct as raw sensor data, but
    // nothing reading the clip alone can calibrate it: the roll zero needs
    // straight-line riding and the gravity reference needs coasting, and a
    // ten-second clip of one corner has neither. Left to itself a clip fits a
    // mount from that one corner and reports a lean several degrees off the
    // ride it was cut from -- which is exactly the disagreement that is
    // confusing to run into and impossible to adjudicate from inside the clip.
    for (c, path) in &written {
        if let Err(e) = write_clip_telemetry(&t, path, c) {
            eprintln!("gmax: warning: no telemetry written for {}: {e:#}", path.display());
        } else if !quiet {
            println!("wrote {}", clip_telemetry_path(path).display());
        }
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

    // Chapters are written in the source's time base, so a trimmed output needs
    // them shifted; ffmpeg takes the timestamps literally.
    let mut opts = opts;
    let _shifted_chapters; // keeps the temporary file alive until ffmpeg has run
    if let Some(path) = &opts.chapters {
        if opts.start_s.is_some() || opts.duration_s.is_some() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let shifted = gmax::moments::clips::shift_ffmetadata(
                &text,
                opts.start_s.unwrap_or(0.0),
                opts.duration_s,
            );
            let temp = std::env::temp_dir().join(format!(
                "gmax-chapters-{}.txt",
                std::process::id()
            ));
            std::fs::write(&temp, &shifted)
                .with_context(|| format!("writing {}", temp.display()))?;
            if !quiet {
                let kept = shifted.matches("[CHAPTER]").count();
                let total = text.matches("[CHAPTER]").count();
                println!(
                    "chapters: {kept} of {total} fall inside the requested segment, shifted to \
                     start at zero"
                );
            }
            opts.chapters = Some(temp.clone());
            _shifted_chapters = Some(temp);
        } else {
            _shifted_chapters = None;
        }
    } else {
        _shifted_chapters = None;
    }

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

    // An equirectangular file is only a 360 video if it says so: players decide
    // whether to offer spherical navigation from Google's GSpherical XMP block,
    // which ffmpeg does not write.
    if !opts.preview {
        let info = render::source_info(&args.input);
        match render::tag_spherical(&output, &info) {
            Ok(true) => {
                if !quiet {
                    println!("tagged as equirectangular 360 (spherical metadata)");
                }
            }
            Ok(false) => eprintln!(
                "gmax: warning: exiftool not found, so {} carries no spherical metadata and \
                 players will show it as a flat image. Install exiftool and re-run, or tag it \
                 yourself.",
                output.display()
            ),
            Err(e) => eprintln!("gmax: warning: {e}"),
        }
    }

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

/// Repair a telemetry's positions from an external GPS track, in place.
///
/// Shared by `analyse` and `clips`, which both rank a ride and must rank the
/// same ride. The merge itself is `gmax merge`'s, at its defaults: this is the
/// same operation, kept in memory instead of written out, so that the moments
/// engine sees the repaired positions while still reading the camera's own IMU
/// channels straight from the telemetry.
fn repair_positions(t: &mut telemetry::Telemetry, gpx: &std::path::Path, quiet: bool) -> Result<()> {
    use gmax::merge;

    let external = gmax::gpx::read_file(gpx)
        .with_context(|| format!("reading {}", gpx.display()))?;
    if external.points.is_empty() {
        anyhow::bail!("{} contains no track points", gpx.display());
    }
    let slots = merge::slots_from_telemetry(t);
    if slots.is_empty() {
        anyhow::bail!("this recording has no timestamped positions to align against");
    }
    let (merged, report) = merge::merge(&slots, &external, &merge::Options::default());
    for w in &report.warnings {
        eprintln!("gmax: warning: {w}");
    }
    let repaired = merge::apply_to_telemetry(t, &merged);
    if !quiet {
        print!("{}", report.summary());
        println!("repaired {repaired} previously unusable fix(es) from {}", gpx.display());
    }
    Ok(())
}

fn cmd_analyse(args: &gmax::cli::AnalyseArgs, quiet: bool) -> Result<()> {
    use gmax::cli::AccelSourceArg;
    use gmax::moments::{self, clips, AccelSource, MetricId};

    // Arguments are validated before any I/O, so a mistyped metric is reported
    // as a mistyped metric rather than as whatever the input file happens to
    // fail at first.
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

    let ext = args
        .input
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    // Video duration is only knowable from a video, and clip windows need it
    // to clamp against.
    let mut duration_s = None;
    let (samples, source, declared) = match ext.as_str() {
        "gpx" | "csv" if args.gps.is_some() => {
            anyhow::bail!(
                "--gps repairs the camera's own fixes, so it needs the video. {} carries only \
                 the positions that survived, with no record of where the receiver failed; \
                 merge it with `gmax merge` instead.",
                args.input.display()
            )
        }
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
            let mut t = telemetry::compute(
                &gpmf,
                &telemetry::Options {
                    max_dop: args.max_dop,
                    ..Default::default()
                },
            )?;
            for w in &t.warnings {
                eprintln!("gmax: warning: {w}");
            }
            if let Some(gpx) = &args.gps {
                repair_positions(&mut t, gpx, quiet)?;
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
