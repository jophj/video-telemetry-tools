//! Stitch geometry, levelling and command construction.

mod common;
use common::fixture_or_skip;

use gmax::ffmpeg::{encoders, Kind, Preset};
use gmax::render::{self, Audio, ConvertOptions, Geometry, Rotation};

#[test]
fn geometry_decomposes_both_known_camera_layouts() {
    // GoPro Max 2: 3008 = 3 x 960 + 2 x 64.
    let g = Geometry::derive(3008, 960).unwrap();
    assert_eq!((g.face, g.overlap, g.segment, g.panel), (960, 64, 448, 512));
    assert_eq!(g.native_output(), (3840, 1920));

    // The original MAX: 4096 = 3 x 1344 + 2 x 32. The same code has to handle
    // it, which is why nothing about the layout is hardcoded.
    let g = Geometry::derive(4096, 1344).unwrap();
    assert_eq!((g.face, g.overlap, g.segment, g.panel), (1344, 32, 656, 688));
    assert_eq!(g.native_output(), (5376, 2688));
}

#[test]
fn geometry_refuses_frames_that_are_not_strips() {
    // An already-stitched equirectangular frame.
    assert!(Geometry::derive(3840, 1920).is_err());
    // No room for an overlap band.
    assert!(Geometry::derive(2880, 960).is_err());
    // An odd overlap cannot be halved.
    assert!(Geometry::derive(3009, 960).is_err() || Geometry::derive(3007, 960).is_err());
}

#[test]
fn levelling_is_derived_from_the_zenith_not_from_an_axis() {
    // Camera already upright: gravity straight along the output's zenith needs
    // no correction at all.
    let r = render::level_from_gravity([0.0, 1.0, 0.0]).unwrap();
    assert!(r.pitch.abs() < 1e-9, "pitch was {}", r.pitch);
    assert_eq!(r.roll, 0.0);

    // Gravity straight down: the camera is upside down, needing half a turn.
    let r = render::level_from_gravity([0.0, -1.0, 0.0]).unwrap();
    assert!((r.pitch.abs() - 180.0).abs() < 1e-6, "pitch was {}", r.pitch);

    // The reference clip's mean gravity. The answer is 117.87 degrees: gravity
    // sits 27.9 degrees below the equator, so reaching the zenith takes
    // 90 + 27.9. The prototype computed 62.1 -- the complement of 27.9 -- and
    // rendered either tarmac or a frame on its side.
    let r = render::level_from_gravity([-0.0334, -0.4677, 0.8833]).unwrap();
    assert!(
        (r.pitch - 117.87).abs() < 0.05,
        "pitch was {:.4}, expected 117.87",
        r.pitch
    );
    assert!((r.yaw - 2.16).abs() < 0.05, "yaw was {:.4}", r.yaw);
    assert_eq!(r.roll, 0.0, "roll would tilt the levelled horizon back over");

    // Tumbling: no single rotation levels it, and saying so beats guessing.
    assert!(render::level_from_gravity([0.01, 0.0, 0.0]).is_err());
}

#[test]
fn yaw_is_wrapped_into_a_readable_range() {
    // The raw arithmetic produces -357.84 for the reference clip; reporting a
    // 2.16 degree correction is the same rotation and far easier to sanity check.
    let r = render::level_from_gravity([-0.0334, -0.4677, 0.8833]).unwrap();
    assert!(r.yaw > -180.0 && r.yaw <= 180.0, "yaw was {}", r.yaw);
}

#[test]
fn rotation_round_trips_through_its_text_form() {
    let r = Rotation::parse("0:117.8719:-1.5").unwrap();
    assert_eq!(r.yaw, 0.0);
    assert!((r.pitch - 117.8719).abs() < 1e-9);
    assert_eq!(r.roll, -1.5);
    assert_eq!(Rotation::parse(&r.to_string()).unwrap().pitch, r.pitch);
    assert!(Rotation::parse("0:1").is_err());
    assert!(Rotation::parse("a:b:c").is_err());
}

#[test]
fn the_rotation_order_puts_yaw_last() {
    // v360's own default is ypr, which applies yaw first: any yaw then moves
    // the meridian pitch acts on, so choosing a heading destroys the levelling.
    // Measured on the reference clip, the sky's latitude after levelling holds
    // at +77.13 degrees for yaw 0, 45 and 90 under pry, and collapses to
    // +34.65 and -13.06 under ypr.
    assert_eq!(render::ROTATION_ORDER, "pry");
    assert_eq!(ConvertOptions::default().rorder, "pry");
}

#[test]
fn the_filter_graph_has_the_shape_the_layout_requires() {
    let layout = render::Layout {
        video: vec![0, 5],
        audio: Some(1),
        ambisonic: Some(6),
        gpmd: Some(4),
        timecode: vec![3],
        geometry: Geometry::derive(3008, 960).unwrap(),
        frame_rate: 30000.0 / 1001.0,
    };
    let g = render::filter_graph(
        &layout,
        3840,
        1920,
        Rotation {
            yaw: 2.0,
            pitch: 117.0,
            roll: 0.0,
        },
        "pry",
        "lanczos",
        None,
    );

    // Both discovered video indices are used, not indices 0 and 1.
    assert!(g.contains("[0:0]format=yuv444p,split=9"));
    assert!(g.contains("[0:5]format=yuv444p,split=9"));

    // Colour conversion happens before the geometric transform, to keep the
    // resampling's intermediate precision off the 4:2:0 chroma grid.
    let format_at = g.find("format=yuv444p").unwrap();
    let v360_at = g.find("v360=").unwrap();
    assert!(format_at < v360_at);

    // Four split faces, each cross-fading its duplicated-angle band.
    assert_eq!(g.matches("blend=all_expr=").count(), 4);
    // Two whole faces, cropped from the middle of each strip at 2 * panel.
    assert_eq!(g.matches("crop=960:960:1024:0").count(), 2);
    // Assembled as a 3x2 equi-angular cubemap.
    assert!(g.contains("[FL][FF][FR]hstack=3[row0]"));
    assert!(g.contains("[FD][FB][FU]hstack=3[row1]"));
    assert!(g.contains("[row0][row1]vstack=2[eac]"));
    assert!(g.contains("input=eac:output=e:w=3840:h=1920"));
    assert!(g.contains("rorder=pry"));
    assert!(g.ends_with("[vout]"));

    // A zero rotation should not emit rotation arguments at all.
    let plain = render::filter_graph(&layout, 3840, 1920, Rotation::default(), "pry", "lanczos", None);
    assert!(!plain.contains("yaw="));
}

#[test]
fn container_follows_what_the_streams_require() {
    let hevc = encoders::lookup("libx265").unwrap();
    let prores = encoders::lookup("prores_ks").unwrap();
    assert_eq!(render::container_for(hevc, Audio::Stereo), "mp4");
    // The MP4 muxer rejects a four-channel ambisonic layout outright.
    assert_eq!(render::container_for(hevc, Audio::Ambisonic), "mov");
    assert_eq!(render::container_for(hevc, Audio::Both), "mov");
    assert_eq!(render::container_for(prores, Audio::Stereo), "mov");
}

#[test]
fn every_preset_names_only_encoders_the_registry_can_drive() {
    for p in [
        Preset::High,
        Preset::Max,
        Preset::Av1,
        Preset::Prores,
        Preset::H264,
    ] {
        let candidates = p.candidates();
        assert!(!candidates.is_empty(), "{:?} has no candidates", p);
        for name in &candidates {
            assert!(
                encoders::lookup(name).is_some(),
                "{:?} names {name}, which is not in the registry",
                p
            );
        }
        // Every preset must have a software fallback, or a machine with no
        // usable GPU would have nothing to fall back to.
        assert!(
            candidates
                .iter()
                .filter_map(|n| encoders::lookup(n))
                .any(|e| e.kind == Kind::Cpu),
            "{:?} has no software candidate",
            p
        );
    }
}

#[test]
fn rate_control_flags_are_per_encoder() {
    let args = |name: &str, q: Option<u32>| {
        encoders::video_args(encoders::lookup(name).unwrap(), q, false).join(" ")
    };
    // Each family takes its quality by a different flag on a different scale,
    // which is why one --enc-quality cannot be passed through blindly.
    assert!(args("libx265", Some(20)).contains("-crf 20"));
    assert!(args("hevc_videotoolbox", Some(70)).contains("-q:v 70"));
    assert!(args("hevc_nvenc", Some(22)).contains("-cq 22"));
    assert!(args("hevc_qsv", Some(23)).contains("-global_quality 23"));
    assert!(args("hevc_amf", Some(22)).contains("-qp_i 22"));
    assert!(args("hevc_vaapi", Some(22)).contains("-qp 22"));
    assert!(args("hevc_mf", Some(24)).contains("-quality 24"));

    // Without this tag, QuickTime and most 360 players refuse HEVC in MP4.
    assert!(args("libx265", None).contains("-tag:v hvc1"));
    assert!(!args("libx264", None).contains("-tag:v hvc1"));

    // The registry default is used when no quality is given.
    assert!(args("libx265", None).contains("-crf 16"));

    // VAAPI frames are already on the GPU in the right format, so no output
    // pixel format may be forced.
    let vaapi = encoders::video_args(encoders::lookup("hevc_vaapi").unwrap(), None, true).join(" ");
    assert!(!vaapi.contains("-pix_fmt"));
}

#[test]
fn a_clip_keeps_every_stream_but_the_timecode() {
    let layout = render::Layout {
        video: vec![0, 5],
        audio: Some(1),
        ambisonic: Some(6),
        gpmd: Some(4),
        timecode: vec![3],
        geometry: Geometry::derive(3008, 960).unwrap(),
        frame_rate: 30000.0 / 1001.0,
    };
    let cmd = render::clip_args(
        &layout,
        std::path::Path::new("in.360"),
        std::path::Path::new("out.360"),
        100.0,
        10.0,
        true,
    )
    .join(" ");
    assert!(cmd.contains("-map 0"), "every stream must be kept");
    assert!(cmd.contains("-map -0:3"), "the timecode track must be dropped");
    assert!(cmd.contains("-c copy"), "a clip must be lossless");
    assert!(cmd.contains("-copy_unknown"), "the gpmd track is an unknown type");
    // Without this the MOV muxer generates fresh timecode tracks from the
    // inherited metadata, one per video stream.
    assert!(cmd.contains("-write_tmcd 0"));
    // An input seek can leave the first packet with a negative timestamp.
    assert!(cmd.contains("-avoid_negative_ts make_zero"));
    // -ss must precede -i to be an input seek.
    assert!(cmd.find("-ss").unwrap() < cmd.find("-i").unwrap());
}

// ---------------------------------------------------------- real material

#[test]
fn inspects_the_reference_clip() {
    let path = fixture_or_skip!(common::REFERENCE_360);
    let Some(_) = common::which("ffprobe") else {
        eprintln!("SKIP: ffprobe is not installed");
        return;
    };
    let ff = gmax::ffmpeg::Ffmpeg::find(None).unwrap();
    let layout = render::inspect(&ff, &path).unwrap();

    // Two strips, and their indices are 0 and 5 on this clip -- not 0 and 1,
    // and not 0 and 4 as on a clip with one fewer audio track. This is why
    // nothing may be hardcoded.
    assert_eq!(layout.video, vec![0, 5]);
    assert_eq!(layout.gpmd, Some(4));
    assert_eq!(layout.audio, Some(1));
    assert_eq!(layout.ambisonic, Some(6));
    assert_eq!(layout.timecode, vec![3]);
    assert_eq!(layout.geometry.face, 960);
    assert!((layout.frame_rate - 29.97).abs() < 0.01);
}

#[test]
fn levels_the_reference_clip_from_its_own_gravity() {
    let path = fixture_or_skip!(common::REFERENCE_360);
    let (mean, n) = render::mean_gravity(&path).unwrap();
    assert_eq!(n, 46080, "one gravity sample per video frame");

    // The mean of unit vectors is shorter than one, and by a useful amount: it
    // measures how much the camera's attitude varied. At 0.963 the machine
    // stayed close to one orientation, which is what makes a single levelling
    // rotation meaningful in the first place.
    let magnitude = gmax::math::magnitude(&mean);
    assert!(
        (0.9..1.0).contains(&magnitude),
        "mean gravity magnitude was {magnitude:.4}"
    );

    // The direction is what the levelling uses.
    let up = gmax::math::normalize(&mean);
    assert!((up[0] + 0.0334).abs() < 0.005, "up {up:?}");
    assert!((up[1] + 0.4675).abs() < 0.005, "up {up:?}");
    assert!((up[2] - 0.8833).abs() < 0.005, "up {up:?}");

    let r = render::level_from_gravity(mean).unwrap();
    assert!((r.pitch - 117.87).abs() < 0.1, "pitch was {:.4}", r.pitch);
}

#[test]
fn source_info_comes_from_the_file_itself() {
    let path = fixture_or_skip!(common::REFERENCE_360);
    let info = render::source_info(&path);
    // The camera names itself in the GPMF device record, so exiftool is not
    // needed to read it -- only to write the output's metadata.
    assert_eq!(info.model.as_deref(), Some("MAX2"));
    // The container's own creation time, 2026-09-08T11:06:53Z on this clip.
    let created = info.created.expect("a creation time");
    let t = chrono::DateTime::from_timestamp(created, 0).unwrap();
    assert_eq!(t.format("%Y-%m-%d").to_string(), "2026-09-08");
}

#[test]
fn a_missing_exiftool_is_reported_rather_than_fatal() {
    // Tagging a path that does not exist must not panic; and where exiftool is
    // absent the call reports that fact instead of failing the render.
    let result = render::tag_spherical(
        std::path::Path::new("/nonexistent/nope.mp4"),
        &render::SourceInfo::default(),
    );
    match result {
        // exiftool present: it should refuse the missing file.
        Ok(true) => panic!("tagging a nonexistent file reported success"),
        // exiftool absent: not an error, just nothing done.
        Ok(false) => {}
        Err(_) => {}
    }
}
