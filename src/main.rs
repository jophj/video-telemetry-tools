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
    let (json, source) = if let Some(path) = &args.telemetry {
        eprintln!("gmax: telemetry from {}", path.display());
        (telemetry_json_from(path, args.max_dop, Some(&args.video))?, path.clone())
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
                (telemetry_json_from(&p, args.max_dop, Some(&args.video))?, p)
            }
            None => {
                eprintln!("gmax: telemetry from the track inside {}", args.video.display());
                let json = telemetry_json_from(&args.video, args.max_dop, Some(&args.video))
                    .map_err(|e| {
                        e.context(
                            "no telemetry found: pass --telemetry, or run `gmax telemetry` on \
                             the original .360 first",
                        )
                    })?;
                (json, args.video.clone())
            }
        }
    };

    gmax::viewer::serve(gmax::viewer::Options {
        video: args.video.clone(),
        telemetry_json: json,
        telemetry_source: source.file_name().map(|s| s.to_string_lossy().to_string()),
        fps: video_frame_rate(&args.video),
        port: args.port,
        open_browser: !args.no_open,
    })
}

/// The video's frame rate, from its own sample table.
///
/// Read from the container rather than assumed, so the viewer's frame step is
/// one frame of *this* video: 29.97 for the Max 2, but 25, 50 or 60 for a
/// proxy or another camera. `None` for a file this reader cannot open, and
/// the page falls back to 29.97.
fn video_frame_rate(video: &std::path::Path) -> Option<f64> {
    let file = gmax::mp4::Mp4::open(video).ok()?;
    let track = file.tracks_by_handler("vide").into_iter().next()?;
    track.mean_sample_duration().map(|d| 1.0 / d)
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

/// Render the viewer's JSON from a video or a CSV.
///
/// A GPX is refused by name. The viewer plays per-frame IMU channels against
/// the video, and a GPX has positions at its receiver's own cadence and at
/// most a lean angle -- nothing to fill the lean-rate, pitch and acceleration
/// traces with, and no reliable tie to video frames. Read as a video instead,
/// it failed with a complaint about MP4 boxes that said nothing useful.
fn telemetry_json_from(
    path: &std::path::Path,
    max_dop: f64,
    video: Option<&std::path::Path>,
) -> Result<String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if ext == "gpx" {
        anyhow::bail!(
            "{} is a GPX, which the viewer cannot play: it needs the camera's per-frame \
             channels. Pass the camera's video, or the CSV `gmax telemetry` writes from it",
            path.display()
        );
    }
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
    let (from, to) = match gmax::mp4::Mp4::open(clip)
        .ok()
        .and_then(|mut f| gmax::gpmf::from_mp4(&mut f).ok())
        .and_then(|g| telemetry::timeline(&g))
        .filter(|t| t.len() > 1)
    {
        // The clip's GPMF keeps the parent's timestamps, so these are already
        // on the parent's timeline.
        Some(t) => parent.range_covering(t[0], t[t.len() - 1]),
        // No GPMF in the cut file, or timestamps that were not carried through:
        // fall back to the window that was asked for. Off by up to a keyframe
        // interval, which is better than nothing to calibrate against. That
        // window was planned in video time, so it has to be moved onto the
        // GPMF timeline; taken raw, it named the wrong stretch of any
        // recording whose timeline does not start at zero.
        None => parent.video_range(planned.start_s, planned.end_s),
    };
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
    let telemetry = gmax::gpmf::from_mp4(&mut file).and_then(|gpmf| {
            telemetry::compute(
                &gpmf,
                &telemetry::Options {
                    max_dop: args.max_dop,
                    ..Default::default()
                },
            )
        });
    // Ranking needs the telemetry. A cut asked for by time does not: it is the
    // way to get a clip out of a ride whose telemetry is too broken to read,
    // so there it only costs the sibling CSV.
    let mut t = match telemetry {
        Ok(t) => Some(t),
        Err(e) if args.ss.is_some() => {
            eprintln!("gmax: warning: no telemetry, so none is written beside the clip: {e:#}");
            None
        }
        Err(e) => return Err(e),
    };
    if let Some(t) = &mut t {
        for w in &t.warnings {
            eprintln!("gmax: warning: {w}");
        }
        if let Some(gpx) = &args.gps {
            repair_positions(t, gpx, quiet)?;
        }
    }

    let plan_opts = clips::PlanOptions {
        pre_s: args.pre,
        post_s: args.post,
        merge_gap_s: args.merge_gap,
        duration_s: Some(container_duration),
        ..Default::default()
    };

    // A cut asked for by time skips the ranking entirely. Not just to save the
    // work: it is the one way to get a clip out of a ride whose telemetry is
    // too sparse or too broken to analyse, and that is exactly when someone
    // reaches for it.
    let (planned, dropped) = match args.ss {
        Some(start) => (
            vec![clips::manual(start, args.duration, &plan_opts)?],
            Vec::new(),
        ),
        None => {
            let t = t.as_ref().expect("ranking bails without telemetry");
            let samples = moments::samples_from_telemetry(t);
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
            let (planned, dropped) = clips::plan(&analysis, &plan_opts);
            if planned.is_empty() {
                anyhow::bail!(
                    "no clips could be planned from {} moment(s); the ride may be too short or \
                     have no usable telemetry",
                    dropped.len()
                );
            }
            (planned, dropped)
        }
    };

    let stem = args
        .input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "ride".into());
    // "highlights" is a claim the ranking earns and a hand-cut clip does not.
    let dir_name = if args.ss.is_some() {
        format!("{stem}_clips")
    } else {
        format!("{stem}_highlights")
    };
    let outdir = args
        .outdir
        .clone()
        .unwrap_or_else(|| args.input.with_file_name(&dir_name));
    // A .360 clip keeps both lenses, so its extension stays .360.
    let ext = args
        .input
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_else(|| "mp4".into());

    if !quiet || args.dry_run {
        if args.ss.is_some() {
            println!("1 clip requested, into {}", outdir.display());
        } else {
            println!(
                "{} clip(s) planned from {} moment(s), into {}",
                planned.len(),
                planned.iter().map(|c| c.moments.len()).sum::<usize>(),
                outdir.display()
            );
        }
        for c in &planned {
            // A hand-cut clip has no moments to describe it, so it is named by
            // the file it will become.
            let what = if c.moments.is_empty() {
                c.name(&stem)
            } else {
                c.moments.iter().map(|m| m.label()).collect::<Vec<_>>().join(", ")
            };
            println!(
                "  {:>8.2}..{:<8.2} ({:>5.2}s)  {what}",
                c.start_s,
                c.end_s,
                c.duration_s(),
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
    // Only the stream list: a stream copy does not care what shape the video
    // is, so a single-track .mp4 cuts exactly like a two-strip .360.
    let streams = gmax::render::probe_streams(&ff, &args.input)?;
    require_utf8(&args.input)?;
    require_utf8(&outdir)?;
    std::fs::create_dir_all(&outdir)
        .with_context(|| format!("creating {}", outdir.display()))?;

    let mut written = Vec::new();
    for c in &planned {
        let path = outdir.join(format!("{}.{ext}", c.name(&stem)));
        gmax::cli::refuse_to_overwrite(&args.input, &path)?;
        if path.exists() && !args.overwrite {
            if !quiet {
                println!("skipping {} (already exists)", path.display());
            }
            written.push((c, path));
            continue;
        }
        let cmd = gmax::render::clip_args(
            &streams.timecode,
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
    for (c, path) in written.iter().filter(|_| t.is_some()) {
        let Some(t) = &t else { break };
        if let Err(e) = write_clip_telemetry(t, path, c) {
            eprintln!("gmax: warning: no telemetry written for {}: {e:#}", path.display());
        } else if !quiet {
            println!("wrote {}", clip_telemetry_path(path).display());
        }
    }

    // A manifest, so the clips can be traced back to the moments that chose them.
    let mut clips_json: Vec<serde_json::Value> = written
        .iter()
        .map(|(c, path)| {
            let mut clip = serde_json::json!({
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
            });
            // A hand cut records what was asked for beside what was cut, since
            // the window is clamped to the video.
            if let Some(start) = args.ss {
                clip["requested"] = serde_json::json!({
                    "startSeconds": start,
                    "durationSeconds": args.duration,
                });
            }
            clip
        })
        .collect();
    let manifest_path = outdir.join(format!("{dir_name}.json"));
    // Hand cuts arrive one per run into the same directory, so each run adds
    // to the manifest rather than replacing it; a re-cut of the same file
    // replaces its own entry. A ranking run is one complete plan and rewrites.
    if args.ss.is_some() {
        let previous = std::fs::read_to_string(&manifest_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|m| m.get("clips").and_then(|c| c.as_array()).cloned())
            .unwrap_or_default();
        let fresh: Vec<_> = clips_json.iter().map(|c| c["file"].clone()).collect();
        let mut kept: Vec<_> = previous
            .into_iter()
            .filter(|c| !fresh.contains(&c["file"]))
            .collect();
        kept.append(&mut clips_json);
        clips_json = kept;
    }
    let mut manifest = serde_json::json!({
        "source": args.input.display().to_string(),
        "sourceDurationSeconds": container_duration,
        "clips": clips_json,
        "dropped": dropped.iter().map(|d| serde_json::json!({
            "label": d.moment.label(),
            "reason": d.reason,
        })).collect::<Vec<_>>(),
    });
    if args.ss.is_none() {
        manifest["window"] = serde_json::json!({
            "preSeconds": args.pre,
            "postSeconds": args.post,
            "mergeGapSeconds": args.merge_gap,
        });
    }
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;
    if !quiet {
        println!("wrote {}", manifest_path.display());
    }
    Ok(())
}

fn cmd_convert(args: &gmax::cli::ConvertArgs, quiet: bool) -> Result<()> {
    use gmax::cli::{AccelArg, AudioArg, PresetArg, ReprojectArg};
    use gmax::ffmpeg::{encoders, Ffmpeg, Kind, Preset};
    use gmax::render::{self, Audio, ConvertOptions, Reproject, Rotation};

    if let Some(out) = &args.out {
        gmax::cli::refuse_to_overwrite(&args.input, out)?;
    }

    // The rotation comes from the telemetry, so it is known before ffmpeg is
    // needed at all -- which is why --print-rotation works without it.
    let mut rotation = if let Some(spec) = &args.rotate {
        Some(Rotation::parse(spec)?)
    } else if args.level || args.print_rotation {
        // A segment is levelled by its own gravity, not the whole ride's.
        let window = (args.ss.is_some() || args.duration.is_some())
            .then(|| (args.ss.unwrap_or(0.0), args.duration));
        let (mean, n) = render::mean_gravity(&args.input, window)?;
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
        reproject: match args.reproject {
            ReprojectArg::Auto => Reproject::Auto,
            ReprojectArg::Gpu => Reproject::Gpu,
            ReprojectArg::Cpu => Reproject::Cpu,
        },
        gpu_shader: None,
        size,
        start_s: args.ss,
        duration_s: args.duration,
        preview: args.preview,
        jobs: args.jobs,
        chapters: args.chapters.clone(),
        overwrite: args.overwrite,
    };

    let (w, h) = opts.size.unwrap_or_else(|| layout.geometry.native_output());

    // A preview is a JPEG: no encoder, so no survey, which is most of what
    // would otherwise make the quick check slow.
    let encoder = if args.preview {
        None
    } else {
        let survey = encoders::survey(&ff, args.reprobe);
        Some(render::choose_encoder_for_size(&ff, &survey, &opts, (w, h))?)
    };

    let output = match &args.out {
        Some(p) => p.clone(),
        None => {
            let ext = match encoder {
                None => "jpg",
                Some(e) => render::container_for(e, opts.audio),
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
    gmax::cli::refuse_to_overwrite(&args.input, &output)?;
    require_utf8(&args.input)?;
    require_utf8(&output)?;
    // ffmpeg's -n declines an existing output but still exits successfully,
    // and the spherical tagging after it would then rewrite the old file. So
    // the check is made here, where it can stop everything.
    if output.exists() && !args.overwrite && !args.dry_run {
        anyhow::bail!(
            "{} already exists; pass --overwrite to replace it",
            output.display()
        );
    }

    // Chapters are written in the source's time base, so a trimmed output needs
    // them shifted; ffmpeg takes the timestamps literally.
    let mut opts = opts;
    // Deletes the shifted copy when this function returns, however it returns;
    // it only has to outlive the ffmpeg run.
    let mut _shifted_chapters: Option<TempFile> = None;
    if let Some(path) = &opts.chapters {
        require_utf8(path)?;
        if opts.start_s.is_some() || opts.duration_s.is_some() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let shifted = gmax::moments::clips::shift_ffmetadata(
                &text,
                opts.start_s.unwrap_or(0.0),
                opts.duration_s,
            );
            let temp = TempFile(std::env::temp_dir().join(format!(
                "gmax-chapters-{}.txt",
                std::process::id()
            )));
            std::fs::write(&temp.0, &shifted)
                .with_context(|| format!("writing {}", temp.0.display()))?;
            if !quiet {
                let kept = shifted.matches("[CHAPTER]").count();
                let total = text.matches("[CHAPTER]").count();
                println!(
                    "chapters: {kept} of {total} fall inside the requested segment, shifted to \
                     start at zero"
                );
            }
            opts.chapters = Some(temp.0.clone());
            _shifted_chapters = Some(temp);
        }
    }

    // The shader is written before the probe, which runs it, and has to
    // outlive the ffmpeg run that reads it. A dry run keeps it, since the
    // command it prints names it.
    let mut _shader: Option<TempFile> = None;
    let shader_path = std::env::temp_dir().join(format!("gmax-eac-{}.hook", std::process::id()));
    if let Some(path) = gpu_reprojection(&ff, &layout, &opts, (w, h), &shader_path)? {
        if !args.dry_run {
            _shader = Some(TempFile(path.clone()));
        }
        opts.gpu_shader = Some(path);
    }

    let cmd = match encoder {
        None => render::preview_args(&layout, &args.input, &output, &opts),
        Some(e) => render::convert_args(
            &layout,
            &args.input,
            &output,
            e,
            ff.vaapi_device().as_deref(),
            &opts,
        ),
    };

    if args.dry_run {
        println!("{}", shell_join(&ff.ffmpeg.display().to_string(), &cmd));
        return Ok(());
    }

    let (w, h) = opts.size.unwrap_or_else(|| layout.geometry.native_output());
    if !quiet {
        let on = if opts.gpu_shader.is_some() { "GPU" } else { "CPU" };
        match encoder {
            Some(e) => println!(
                "stitching {} to {w}x{h} with {} ({}), reprojecting on the {on}",
                args.input.display(),
                e.name,
                e.kind.as_str()
            ),
            None => println!("previewing {} at {w}x{h}, reprojecting on the {on}", args.input.display()),
        }
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

/// A temporary file that is deleted when it goes out of scope.
/// Settle where a conversion reprojects, writing the GPU shader to `path`
/// when it is the GPU. `None` means the CPU.
///
/// The interpolation is checked first, since no probe can rescue an
/// unported one. `auto` falls back to the CPU on any failure, saying why
/// only when the reason is not simply that this machine lacks the means.
fn gpu_reprojection(
    ff: &gmax::ffmpeg::Ffmpeg,
    layout: &gmax::render::Layout,
    opts: &gmax::render::ConvertOptions,
    out: (u32, u32),
    path: &std::path::Path,
) -> Result<Option<std::path::PathBuf>> {
    use gmax::render::{gpu, Reproject};

    let explicit = match opts.reproject {
        Reproject::Cpu => return Ok(None),
        Reproject::Gpu => true,
        Reproject::Auto => false,
    };
    let Some(interp) = gpu::Interp::parse(&opts.interp) else {
        if explicit {
            anyhow::bail!(
                "--interp {} has no GPU implementation; use near, linear or lanczos, or \
                 --reproject cpu",
                opts.interp
            );
        }
        return Ok(None);
    };
    let text = path.display().to_string();
    if !gpu::path_is_graph_safe(&text) {
        let why = format!(
            "the temporary directory's path ({text}) has characters ffmpeg's filter syntax \
             would need escaped; point TMPDIR somewhere plainer"
        );
        if explicit {
            anyhow::bail!(why);
        }
        eprintln!("gmax: warning: reprojecting on the CPU: {why}");
        return Ok(None);
    }

    let face = layout.geometry.face;
    let atlas = (3 * face, 2 * face);
    let shader = gpu::shader(
        out.0,
        out.1,
        atlas.0,
        atlas.1,
        opts.rotation.unwrap_or_default(),
        &opts.rorder,
        interp,
    );
    std::fs::write(path, shader).with_context(|| format!("writing {}", path.display()))?;
    if ff.test_libplacebo(path, atlas, out) {
        return Ok(Some(path.to_path_buf()));
    }
    let _ = std::fs::remove_file(path);
    if explicit {
        anyhow::bail!(
            "libplacebo could not run the reprojection shader: this ffmpeg may be built \
             without it, or there is no working Vulkan device. Use --reproject cpu."
        );
    }
    Ok(None)
}

struct TempFile(std::path::PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Refuse a path that is not valid Unicode, before ffmpeg mangles it.
///
/// The ffmpeg command line is assembled as text, so such a path would reach
/// ffmpeg with replacement characters in it and fail as "no such file" —
/// or, for an output, be written under a name nobody asked for.
fn require_utf8(path: &std::path::Path) -> Result<()> {
    if path.to_str().is_none() {
        anyhow::bail!(
            "{} is not valid Unicode, which gmax cannot pass to ffmpeg; rename it",
            path.display()
        );
    }
    Ok(())
}

/// Quote a command for display, so a dry run can be pasted into a shell.
///
/// The program is quoted too: an ffmpeg under `C:\Program Files` or
/// `~/My Tools` is the norm, not the exception. POSIX shells get single
/// quotes; on Windows the quoting is the one the C runtime's argument parser
/// undoes, which is what ffmpeg.exe will see.
fn shell_join(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(|a| {
            if cfg!(windows) {
                quote_windows(a)
            } else {
                quote_posix(a)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_posix(a: &str) -> String {
    if !a.is_empty() && a.chars().all(|c| c.is_alphanumeric() || "-_./:=+".contains(c)) {
        a.to_string()
    } else {
        format!("'{}'", a.replace('\'', "'\\''"))
    }
}

/// Quote one argument by the MSVC runtime's rules: wrap in double quotes,
/// escape an embedded quote with a backslash, and double any backslashes that
/// precede a quote (including the closing one), since only those are special.
fn quote_windows(a: &str) -> String {
    if !a.is_empty() && !a.chars().any(|c| c.is_whitespace() || "\"&|<>^%!()".contains(c)) {
        return a.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in a.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
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
            for w in &g.warnings {
                eprintln!("gmax: warning: {w}");
            }
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

    gmax::cli::refuse_to_overwrite(&args.base, &args.out_path())?;
    gmax::cli::refuse_to_overwrite(&args.external, &args.out_path())?;

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
    gmax::cli::refuse_to_overwrite(&args.input, &args.gpx_path())?;
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
    if !args.no_csv {
        gmax::cli::refuse_to_overwrite(&args.input, &args.csv_path())?;
    }
    if let Some(json) = args.json_path() {
        gmax::cli::refuse_to_overwrite(&args.input, &json)?;
    }
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

#[cfg(test)]
mod tests {
    use super::{quote_posix, quote_windows};

    #[test]
    fn posix_quoting_survives_spaces_and_quotes() {
        assert_eq!(quote_posix("plain/path.mp4"), "plain/path.mp4");
        assert_eq!(quote_posix("/my tools/ffmpeg"), "'/my tools/ffmpeg'");
        assert_eq!(quote_posix("it's"), "'it'\\''s'");
        assert_eq!(quote_posix(""), "''");
    }

    #[test]
    fn windows_quoting_follows_the_c_runtime_rules() {
        assert_eq!(quote_windows(r"C:\ffmpeg\bin\ffmpeg.exe"), r"C:\ffmpeg\bin\ffmpeg.exe");
        assert_eq!(
            quote_windows(r"C:\Program Files\ffmpeg\bin\ffmpeg.exe"),
            r#""C:\Program Files\ffmpeg\bin\ffmpeg.exe""#
        );
        // Backslashes are literal except before a quote, where they double.
        assert_eq!(quote_windows(r#"a "b""#), r#""a \"b\"""#);
        assert_eq!(quote_windows(r"dir with space\"), r#""dir with space\\""#);
        assert_eq!(quote_windows(""), r#""""#);
    }
}
