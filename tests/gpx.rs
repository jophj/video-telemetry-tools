//! GPX reading, writing and round-tripping.

mod common;
use common::fixture_or_skip;

use gmax::gpx::{self, Gpx, TrackPoint};

#[test]
fn reads_a_motion_record_export_with_its_census_intact() {
    let path = fixture_or_skip!(common::EXTERNAL_GPX);
    let g = gpx::read_file(&path).expect("parse ride.gpx");

    // The census of motion-record-analysis's own output for its reference ride.
    assert_eq!(g.points.len(), 3745, "track points");
    assert_eq!(
        g.points.iter().filter(|p| p.lean_deg.is_some()).count(),
        3706,
        "lean is absent on the 39 samples whose source field was missing"
    );
    assert_eq!(
        g.points.iter().filter(|p| p.course_deg.is_some()).count(),
        3618,
        "course is absent on the 127 near-stationary samples"
    );
    assert_eq!(
        g.points.iter().filter(|p| p.speed_ms.is_some()).count(),
        3745
    );

    // The 50 waypoints must not leak into the track, and their names must not
    // be mistaken for the track's.
    assert_eq!(g.activity.as_deref(), Some("motorcycle"));
    assert!(
        g.name.as_deref().unwrap_or_default().contains("MotionRecord"),
        "track name was {:?}",
        g.name
    );

    // Bounds, from REQUIREMENTS.md section 6.5.
    let (minlat, minlon, maxlat, maxlon) = g.bounds().unwrap();
    assert!((minlat - 43.8505349).abs() < 1e-6, "minlat {minlat}");
    assert!((minlon - 11.1161904).abs() < 1e-6, "minlon {minlon}");
    assert!((maxlat - 44.2051629).abs() < 1e-6, "maxlat {maxlat}");
    assert!((maxlon - 11.3387998).abs() < 1e-6, "maxlon {maxlon}");

    // The lean clamp this ride hit, discoverable from the data.
    let peak = g
        .points
        .iter()
        .filter_map(|p| p.lean_deg)
        .fold(0.0f64, |a, b| a.max(b.abs()));
    assert!((peak - 55.0).abs() < 1e-9, "peak lean was {peak}");
    assert!(g.has_lean(), "an external source with lean enables lean-based alignment");

    // First point, verbatim from the file.
    let p = &g.points[0];
    assert!((p.lat - 44.1727632).abs() < 1e-7);
    assert!((p.lon - 11.3387998).abs() < 1e-7);
    assert!((p.ele_m.unwrap() - 1013.7).abs() < 0.05);
    assert_eq!(
        p.time.unwrap().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "2026-09-05T09:40:14.496Z"
    );
}

#[test]
fn tolerates_either_track_point_extension_version_and_any_prefix() {
    // The GoPro prototype wrote TrackPointExtension v1, motion-record-analysis
    // writes v2, and other tools invent their own prefixes for both. Matching
    // local names rather than qualified ones is what makes all three readable.
    let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<gpx version="1.1" creator="test"
     xmlns="http://www.topografix.com/GPX/1/1"
     xmlns:ns3="http://www.garmin.com/xmlschemas/TrackPointExtension/v2"
     xmlns:whatever="https://example.invalid/x">
  <trk><trkseg>
    <trkpt lat="44.1" lon="11.2">
      <ele>500.5</ele>
      <time>2026-09-08T11:07:00.100Z</time>
      <hdop>3.31</hdop>
      <extensions>
        <ns3:TrackPointExtension>
          <ns3:speed>12.5</ns3:speed>
          <ns3:course>201.0</ns3:course>
        </ns3:TrackPointExtension>
        <whatever:lean>-33.25</whatever:lean>
      </extensions>
    </trkpt>
  </trkseg></trk>
</gpx>"#;
    let g = gpx::read::read_str(doc).unwrap();
    assert_eq!(g.points.len(), 1);
    let p = &g.points[0];
    assert_eq!(p.speed_ms, Some(12.5));
    assert_eq!(p.course_deg, Some(201.0));
    assert_eq!(p.lean_deg, Some(-33.25));
    assert_eq!(p.hdop, Some(3.31));
}

#[test]
fn missing_fields_stay_missing_rather_than_becoming_zero() {
    let doc = r#"<gpx version="1.1" xmlns="http://www.topografix.com/GPX/1/1">
  <trk><trkseg>
    <trkpt lat="44.1" lon="11.2"><time>2026-09-08T11:07:00Z</time></trkpt>
    <trkpt lat="44.2" lon="11.3"/>
  </trkseg></trk>
</gpx>"#;
    let g = gpx::read::read_str(doc).unwrap();
    assert_eq!(g.points.len(), 2);
    // A missing elevation is not sea level and a missing speed is not stopped.
    assert_eq!(g.points[0].ele_m, None);
    assert_eq!(g.points[0].speed_ms, None);
    assert_eq!(g.points[0].lean_deg, None);
    // An empty element still yields a point.
    assert_eq!(g.points[1].time, None);
}

#[test]
fn rejects_a_document_that_is_not_gpx() {
    assert!(gpx::read::read_str("<html><body>nope</body></html>").is_err());
    assert!(gpx::read::read_str("not xml at all").is_err());
}

#[test]
fn points_without_coordinates_are_dropped() {
    let doc = r#"<gpx version="1.1" xmlns="http://www.topografix.com/GPX/1/1">
  <trk><trkseg>
    <trkpt lat="44.1"><time>2026-09-08T11:07:00Z</time></trkpt>
    <trkpt lat="44.2" lon="11.3"/>
  </trkseg></trk>
</gpx>"#;
    let g = gpx::read::read_str(doc).unwrap();
    assert_eq!(g.points.len(), 1, "a point with no longitude is not a point");
}

#[test]
fn out_of_order_points_are_sorted_defensively() {
    let doc = r#"<gpx version="1.1" xmlns="http://www.topografix.com/GPX/1/1">
  <trk><trkseg>
    <trkpt lat="44.3" lon="11.3"><time>2026-09-08T11:09:00Z</time></trkpt>
    <trkpt lat="44.1" lon="11.1"><time>2026-09-08T11:07:00Z</time></trkpt>
    <trkpt lat="44.2" lon="11.2"><time>2026-09-08T11:08:00Z</time></trkpt>
  </trkseg></trk>
</gpx>"#;
    let g = gpx::read::read_str(doc).unwrap();
    let lats: Vec<f64> = g.points.iter().map(|p| p.lat).collect();
    assert_eq!(lats, vec![44.1, 44.2, 44.3]);
}

#[test]
fn hostile_text_cannot_break_out_of_the_document() {
    let mut g = Gpx {
        name: Some(r#"Ride </name></metadata><script>alert("x")</script> & 'quoted'"#.to_string()),
        ..Default::default()
    };
    g.points.push(TrackPoint {
        lat: 44.1,
        lon: 11.2,
        source: Some("<injected/>".to_string()),
        ..Default::default()
    });

    let mut out = Vec::new();
    gpx::write::write_to(&g, &mut out, &gpx::WriteOptions::default()).unwrap();
    let text = String::from_utf8(out).unwrap();

    assert!(!text.contains("<script>"), "raw markup leaked into the output");
    assert!(!text.contains("<injected/>"));
    assert!(text.contains("&lt;script&gt;"));
    assert!(text.contains("&amp;"));
    assert!(text.contains("&apos;"));
    // And it must still parse.
    let back = gpx::read::read_str(&text).unwrap();
    assert_eq!(back.points.len(), 1);
    assert_eq!(back.name, g.name);
}

#[test]
fn round_trips_every_channel() {
    let mut g = Gpx {
        name: Some("Test ride".into()),
        activity: Some("motorcycling".into()),
        ..Default::default()
    };
    g.points.push(TrackPoint {
        lat: 44.1725166,
        lon: 11.3394181,
        ele_m: Some(511.55),
        time: Some(chrono::DateTime::parse_from_rfc3339("2026-09-08T11:07:00.100Z").unwrap().into()),
        hdop: Some(3.31),
        speed_ms: Some(12.345),
        course_deg: Some(201.4),
        lean_deg: Some(-33.25),
        accel_ms2: Some(-4.125),
        at_sensor_limit: true,
        source: Some("external".into()),
        video_time_s: Some(13.117),
        segment: 0,
    });

    let mut out = Vec::new();
    gpx::write::write_to(&g, &mut out, &gpx::WriteOptions::default()).unwrap();
    let text = String::from_utf8(out).unwrap();
    let back = gpx::read::read_str(&text).unwrap();

    assert_eq!(back.points.len(), 1);
    let (a, b) = (&g.points[0], &back.points[0]);
    assert!((a.lat - b.lat).abs() < 1e-7);
    assert!((a.lon - b.lon).abs() < 1e-7);
    assert_eq!(a.time, b.time);
    assert_eq!(a.hdop, b.hdop);
    assert_eq!(a.speed_ms, b.speed_ms);
    assert_eq!(a.lean_deg, b.lean_deg);
    assert_eq!(a.accel_ms2, b.accel_ms2);
    assert_eq!(a.at_sensor_limit, b.at_sensor_limit);
    assert_eq!(a.source, b.source);
    assert_eq!(a.video_time_s, b.video_time_s);
    // Elevation is written to one decimal, which is all a GPS altitude is
    // worth, so it round-trips to within half of the last place. 511.55 lands
    // exactly on that boundary and becomes 511.6.
    assert_eq!(gmax::math::fmt_num(b.ele_m.unwrap(), 1), "511.6");
    // 0.051 rather than 0.05: the difference is 0.0500000000000682 in binary
    // floating point, and the assertion above already pins the exact value.
    assert!((a.ele_m.unwrap() - b.ele_m.unwrap()).abs() < 0.051);
    assert_eq!(back.activity.as_deref(), Some("motorcycling"));
}

#[test]
fn element_order_follows_the_gpx_schema() {
    let mut g = Gpx::default();
    g.points.push(TrackPoint {
        lat: 44.1,
        lon: 11.2,
        ele_m: Some(500.0),
        time: Some(chrono::Utc::now()),
        hdop: Some(2.0),
        speed_ms: Some(10.0),
        course_deg: Some(90.0),
        ..Default::default()
    });
    let mut out = Vec::new();
    gpx::write::write_to(&g, &mut out, &gpx::WriteOptions::default()).unwrap();
    let text = String::from_utf8(out).unwrap();
    // GPX 1.1 fixes this sequence; a validator rejects any other. The
    // metadata's own <time> comes first in the file, so the point's is looked
    // for only after the point opens.
    let trkpt = text.find("<trkpt").unwrap();
    let after = |tag: &str| trkpt + text[trkpt..].find(tag).unwrap();
    let (ele, time, hdop, ext) = (after("<ele>"), after("<time>"), after("<hdop>"), after("<extensions>"));
    assert!(
        ele < time && time < hdop && hdop < ext,
        "ele/time/hdop/extensions out of order"
    );
    // TrackPointExtension v2 fixes its own sequence too: speed, then course.
    assert!(after("<gpxtpx:speed>") < after("<gpxtpx:course>"));
}

#[test]
fn speed_and_course_are_written_in_the_extension_version_that_defines_them() {
    // TrackPointExtension v1 has no speed or course elements at all; only v2
    // does, so writing them under the v1 namespace produced a file a
    // validating consumer rejects.
    let mut g = Gpx::default();
    g.points.push(TrackPoint {
        lat: 44.1,
        lon: 11.2,
        speed_ms: Some(10.0),
        course_deg: Some(359.97),
        ..Default::default()
    });
    let mut out = Vec::new();
    gpx::write::write_to(&g, &mut out, &gpx::WriteOptions::default()).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(r#"xmlns:gpxtpx="http://www.garmin.com/xmlschemas/TrackPointExtension/v2""#));
    assert!(!text.contains("TrackPointExtension/v1"));
    assert!(text.contains("https://www8.garmin.com/xmlschemas/TrackPointExtensionv2.xsd"));
    // v2 types course as [0, 360), so a heading that rounds up to 360 is
    // written as the 0 it means.
    assert!(text.contains("<gpxtpx:course>0</gpxtpx:course>"), "{text}");
}

#[test]
fn reads_speed_and_course_from_a_v1_extension_too() {
    // What the GoPro prototype wrote, and what earlier gmax builds wrote.
    let doc = r#"<gpx version="1.1" xmlns="http://www.topografix.com/GPX/1/1"
     xmlns:gpxtpx="http://www.garmin.com/xmlschemas/TrackPointExtension/v1">
  <trk><trkseg>
    <trkpt lat="44.1" lon="11.2"><time>2026-09-08T11:07:00Z</time>
      <extensions><gpxtpx:TrackPointExtension>
        <gpxtpx:speed>9.5</gpxtpx:speed><gpxtpx:course>12.5</gpxtpx:course>
      </gpxtpx:TrackPointExtension></extensions>
    </trkpt>
  </trkseg></trk>
</gpx>"#;
    let g = gpx::read::read_str(doc).unwrap();
    assert_eq!(g.points[0].speed_ms, Some(9.5));
    assert_eq!(g.points[0].course_deg, Some(12.5));
}

#[test]
fn an_untimed_point_does_not_stop_the_timed_ones_being_sorted() {
    // A comparator calling every pair involving an untimed point equal is not
    // a total order, and left [10, untimed, 5, 7] exactly as it was.
    let doc = r#"<gpx version="1.1" xmlns="http://www.topografix.com/GPX/1/1">
  <trk><trkseg>
    <trkpt lat="44.10" lon="11"><time>2026-09-08T11:00:10Z</time></trkpt>
    <trkpt lat="44.99" lon="11"/>
    <trkpt lat="44.05" lon="11"><time>2026-09-08T11:00:05Z</time></trkpt>
    <trkpt lat="44.07" lon="11"><time>2026-09-08T11:00:07Z</time></trkpt>
  </trkseg></trk>
</gpx>"#;
    let g = gpx::read::read_str(doc).unwrap();
    let timed: Vec<f64> = g.timed_points().iter().map(|p| p.lat).collect();
    assert_eq!(timed, vec![44.05, 44.07, 44.10]);
    assert_eq!(g.points.len(), 4, "the untimed point is kept");
}

#[test]
fn timestamps_without_a_zone_or_seconds_are_read_and_counted() {
    let doc = r#"<gpx version="1.1" xmlns="http://www.topografix.com/GPX/1/1">
  <trk><trkseg>
    <trkpt lat="44.1" lon="11"><time>2024-05-01T10:00:00</time></trkpt>
    <trkpt lat="44.2" lon="11"><time>2024-05-01T10:01Z</time></trkpt>
    <trkpt lat="44.3" lon="11"><time>2024-05-01T12:02+02:00</time></trkpt>
    <trkpt lat="44.4" lon="11"><time>2024-05-01T10:03:00.5</time></trkpt>
    <trkpt lat="44.5" lon="11"><time>yesterday</time></trkpt>
  </trkseg></trk>
</gpx>"#;
    let g = gpx::read::read_str(doc).unwrap();
    let times: Vec<String> = g
        .points
        .iter()
        .filter_map(|p| p.time)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .collect();
    assert_eq!(
        times,
        vec![
            "2024-05-01T10:00:00.000Z",
            "2024-05-01T10:01:00.000Z",
            "2024-05-01T10:02:00.000Z",
            "2024-05-01T10:03:00.500Z",
        ]
    );
    // Neither the zone assumption nor the unreadable time passes silently.
    assert!(
        g.warnings.iter().any(|w| w.starts_with("2 ") && w.contains("read as UTC")),
        "{:?}",
        g.warnings
    );
    assert!(
        g.warnings.iter().any(|w| w.starts_with("1 ") && w.contains("could not be parsed")),
        "{:?}",
        g.warnings
    );
}

#[test]
fn metadata_extensions_survive_a_round_trip() {
    // This is where a merge records its method, offset and scores, so losing
    // it on re-read loses the audit trail.
    let g = Gpx {
        metadata: vec![
            ("merge_method".into(), "lean-angle correlation".into()),
            ("merge_offset_s".into(), "1.6".into()),
        ],
        points: vec![TrackPoint { lat: 44.1, lon: 11.2, ..Default::default() }],
        ..Default::default()
    };
    let mut out = Vec::new();
    gpx::write::write_to(&g, &mut out, &gpx::WriteOptions::default()).unwrap();
    let back = gpx::read::read_str(&String::from_utf8(out).unwrap()).unwrap();
    assert_eq!(back.metadata, g.metadata);
}

#[test]
fn a_cdata_name_is_read_and_an_authors_name_is_not_the_tracks() {
    let doc = r#"<gpx version="1.1" xmlns="http://www.topografix.com/GPX/1/1">
  <metadata><author><name>Jo Rider</name></author></metadata>
  <trk><name><![CDATA[Passo & Futa]]></name><trkseg>
    <trkpt lat="44.1" lon="11"/>
  </trkseg></trk>
</gpx>"#;
    let g = gpx::read::read_str(doc).unwrap();
    assert_eq!(g.name.as_deref(), Some("Passo & Futa"));
}

#[test]
fn segment_boundaries_survive_a_round_trip() {
    // A segment break is the producer recording that its receiver stopped
    // logging; flattening it would invite interpolation straight across.
    let doc = r#"<gpx version="1.1" xmlns="http://www.topografix.com/GPX/1/1">
  <trk>
    <trkseg><trkpt lat="44.1" lon="11"/><trkpt lat="44.2" lon="11"/></trkseg>
    <trkseg><trkpt lat="44.3" lon="11"/></trkseg>
  </trk>
  <trk><trkseg><trkpt lat="44.4" lon="11"/></trkseg></trk>
</gpx>"#;
    let g = gpx::read::read_str(doc).unwrap();
    let segs: Vec<usize> = g.points.iter().map(|p| p.segment).collect();
    assert_eq!(segs, vec![0, 0, 1, 2]);

    let mut out = Vec::new();
    gpx::write::write_to(&g, &mut out, &gpx::WriteOptions::default()).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(text.matches("<trkseg>").count(), 3);
    let back = gpx::read::read_str(&text).unwrap();
    let segs: Vec<usize> = back.points.iter().map(|p| p.segment).collect();
    assert_eq!(segs, vec![0, 0, 1, 2]);
}

#[test]
fn generated_gpx_validates_against_the_official_schema() {
    let xsd = fixture_or_skip!(common::GPX_XSD);
    let Some(xmllint) = common::which("xmllint") else {
        eprintln!("SKIP: xmllint is not installed");
        return;
    };
    let path = fixture_or_skip!(common::REFERENCE_360);

    let mut f = gmax::mp4::Mp4::open(&path).unwrap();
    let gp = gmax::gpmf::from_mp4(&mut f).unwrap();
    let t = gmax::telemetry::compute(&gp, &gmax::telemetry::Options::default()).unwrap();
    let track = gpx::write::from_telemetry(&t, Some("GS010527".into()));
    assert!(track.points.len() > 8000, "expected a populated track");

    let out = std::env::temp_dir().join("gmax-schema-test.gpx");
    gpx::write_file(&track, &out, &gpx::WriteOptions::default()).unwrap();

    let status = std::process::Command::new(xmllint)
        .arg("--noout")
        .arg("--schema")
        .arg(&xsd)
        .arg(&out)
        .output()
        .expect("run xmllint");
    assert!(
        status.status.success(),
        "xmllint rejected our GPX:\n{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let _ = std::fs::remove_file(out);
}

#[test]
fn a_fix_without_its_own_utc_is_dated_as_the_telemetry_dates_it() {
    // `t0` is the clock at `times[0]`, not at zero. Dating a fix as
    // `t0 + fix.time` disagreed with `Telemetry::utc_of` — and with the merge,
    // which uses it — by however far into the recording the first sample
    // lands.
    let path = fixture_or_skip!(common::REFERENCE_360);
    let mut f = gmax::mp4::Mp4::open(&path).unwrap();
    let gp = gmax::gpmf::from_mp4(&mut f).unwrap();
    let mut t = gmax::telemetry::compute(&gp, &gmax::telemetry::Options::default()).unwrap();
    for fix in &mut t.gps {
        fix.utc = None;
    }
    let track = gpx::write::from_telemetry(&t, None);
    let valid: Vec<_> = t.gps.iter().filter(|f| f.valid).collect();
    assert_eq!(track.points.len(), valid.len());
    for (p, fix) in track.points.iter().zip(valid) {
        assert_eq!(p.time, t.utc_of(fix.time));
    }
}
