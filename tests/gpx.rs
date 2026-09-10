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
    let mut g = Gpx::default();
    g.name = Some(r#"Ride </name></metadata><script>alert("x")</script> & 'quoted'"#.to_string());
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
        ..Default::default()
    });
    let mut out = Vec::new();
    gpx::write::write_to(&g, &mut out, &gpx::WriteOptions::default()).unwrap();
    let text = String::from_utf8(out).unwrap();
    // GPX 1.1 fixes this sequence; a validator rejects any other.
    let ele = text.find("<ele>").unwrap();
    let time = text.find("<time>2").or_else(|| text.find("<time>")).unwrap();
    let hdop = text.find("<hdop>").unwrap();
    let ext = text.find("<extensions>").unwrap();
    assert!(ele < hdop && hdop < ext, "ele/hdop/extensions out of order");
    let _ = time;
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
