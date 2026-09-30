//! Container and GPMF assertions against the reference clip.
//!
//! Every number here was decoded by hand from the raw bytes of
//! `sources/GS010527.360` before the parser existed, so these are checks
//! against the file, not against the parser's own output.

mod common;
use common::fixture_or_skip;

use gmax::fourcc::FourCc;
use gmax::gpmf;
use gmax::mp4::Mp4;

fn open() -> Option<(Mp4, gpmf::Gpmf)> {
    let path = common::fixture(common::REFERENCE_360)?;
    let mut f = Mp4::open(&path).expect("open reference clip");
    let g = gpmf::from_mp4(&mut f).expect("parse GPMF");
    Some((f, g))
}

#[test]
fn container_layout_matches_the_reference_clip() {
    let path = fixture_or_skip!(common::REFERENCE_360);
    let f = Mp4::open(&path).unwrap();

    assert_eq!(f.major_brand.map(|b| b.to_string()).as_deref(), Some("mp41"));
    assert_eq!(f.timescale, 30000);
    assert!((f.duration_secs() - 1537.536).abs() < 1e-3);

    // Seven streams: two HEVC, three audio (two AAC plus ambisonic PCM), a
    // timecode track and the telemetry track.
    assert_eq!(f.tracks.len(), 7);
    let video = f.tracks_by_handler("vide");
    assert_eq!(video.len(), 2, "a .360 carries one strip per lens");
    for t in &video {
        assert_eq!((t.width, t.height), (3008, 960));
        assert_eq!(t.samples.len(), 46080);
    }

    // The firmware and model live in udta, not in any stream.
    let firm = f.udta.get(&FourCc::from_str_lossy("FIRM")).unwrap();
    assert_eq!(String::from_utf8_lossy(firm).trim_end_matches('\0'), "H24.02.01.30.00");
}

#[test]
fn gpmd_payload_duration_is_the_ntsc_interval() {
    let path = fixture_or_skip!(common::REFERENCE_360);
    let f = Mp4::open(&path).unwrap();
    let g = f.gpmd_track().expect("gpmd track");

    assert_eq!(g.format.to_string(), "gpmd");
    assert_eq!(g.samples.len(), 1536);
    assert_eq!(g.timescale, 1000);

    // This is the number the whole timebase hangs off. 46080 video frames over
    // 1536 payloads is exactly 30 frames each, and the payload is 1.001 s, so a
    // stream carrying 30 samples per payload runs at 29.97 Hz -- not the 30 Hz
    // a sample count alone would suggest. The prototypes assumed 30 and were
    // 0.1% fast over the whole clip.
    let d = g.mean_sample_duration().unwrap();
    assert!((d - 1.001).abs() < 1e-9, "payload duration was {d}");

    // Chunk offsets must have come from co64: the file is 8.55 GB, so the last
    // sample sits well past the 4 GiB a 32-bit stco could address.
    let last = g.samples.last().unwrap();
    assert!(last.offset > u64::from(u32::MAX), "offset {}", last.offset);
}

#[test]
fn gps9_header_decodes_as_hand_read_from_the_raw_bytes() {
    let Some((_f, g)) = open() else {
        eprintln!("SKIP: fixture {} is not present", common::REFERENCE_360);
        return;
    };
    let series = g.series();
    let gps = series
        .get(&(1, FourCc::from_str_lossy("GPS9")))
        .expect("GPS9 stream");

    // Max 2 uses the nine-field GPS stream, not GPS5.
    assert_eq!(
        gps.name.as_deref(),
        Some("GPS (Lat., Long., Alt., 2D, 3D, days, secs, DOP, fix)")
    );
    assert_eq!(gps.type_str.as_deref(), Some("lllllllSS"));
    assert_eq!(gps.components, 9);
    assert_eq!(
        gps.units,
        vec!["deg", "deg", "m", "m/s", "m/s", "", "s", "", ""]
    );
    // Ten samples per 1.001 s payload.
    assert_eq!(gps.len(), 15375);
    let hz = gps.hz().unwrap();
    assert!((hz - 10.0).abs() < 0.01, "GPS9 rate was {hz}");
}

#[test]
fn imu_stream_headers_and_rates() {
    let Some((_f, g)) = open() else {
        eprintln!("SKIP: fixture {} is not present", common::REFERENCE_360);
        return;
    };
    let series = g.series();
    let get = |k: &str| series.get(&(1, FourCc::from_str_lossy(k))).expect(k);

    // Orientation streams run at video rate, one sample per frame.
    for key in ["GRAV", "CORI", "IORI"] {
        let s = get(key);
        assert_eq!(s.len(), 46080, "{key} sample count");
        assert!((s.hz().unwrap() - 29.97).abs() < 0.01, "{key} rate");
    }
    assert_eq!(get("GRAV").name.as_deref(), Some("Gravity Vector"));
    assert_eq!(get("GRAV").components, 3);
    // GRAV carries no ORIN, which is why it needs its own frame conversion.
    assert_eq!(get("GRAV").orin, None);
    // CORI and IORI are quaternions.
    assert_eq!(get("CORI").components, 4);

    let accl = get("ACCL");
    assert_eq!(accl.orin.as_deref(), Some("ZXY"));
    // Latin-1, not UTF-8: the superscript two is a single byte 0xb2.
    assert_eq!(accl.units, vec!["m/s²"]);
    assert!((accl.hz().unwrap() - 201.4).abs() < 1.0);

    // The gyro runs far faster than the prototype assumed.
    let gyro = get("GYRO");
    assert_eq!(gyro.units, vec!["rad/s"]);
    assert!((gyro.hz().unwrap() - 805.7).abs() < 2.0, "gyro rate {:?}", gyro.hz());
}

#[test]
fn first_samples_match_the_hand_decoded_bytes() {
    let Some((_f, g)) = open() else {
        eprintln!("SKIP: fixture {} is not present", common::REFERENCE_360);
        return;
    };
    let series = g.series();
    let get = |k: &str| series.get(&(1, FourCc::from_str_lossy(k))).expect(k);

    // GRAV[0] = (3814, -10204, 30903) / 32767, read from the raw KLV.
    let grav = get("GRAV").sample(0).to_vec();
    let expect = [0.116_398, -0.311_411, 0.943_113];
    for (a, b) in grav.iter().zip(expect) {
        assert!((a - b).abs() < 1e-6, "GRAV[0] = {grav:?}");
    }
    // A gravity vector must be a unit vector.
    let mag = gmax::math::magnitude(&[grav[0], grav[1], grav[2]]);
    assert!((mag - 1.0).abs() < 1e-4, "|GRAV[0]| = {mag}");

    // ACCL[0] = (-1257, 364, 3721) / 417 m/s².
    let accl = get("ACCL").sample(0).to_vec();
    let expect = [-3.014_388, 0.872_902, 8.923_261];
    for (a, b) in accl.iter().zip(expect) {
        assert!((a - b).abs() < 1e-5, "ACCL[0] = {accl:?}");
    }

    // CORI is relative to the clip's first frame, so it starts at identity.
    // This is why GRAV is the only absolute attitude reference available.
    let cori = get("CORI").sample(0).to_vec();
    assert!((cori[0] - 1.0).abs() < 1e-4, "CORI[0] = {cori:?}");
    for c in &cori[1..] {
        assert!(c.abs() < 1e-3, "CORI[0] = {cori:?}");
    }
}

#[test]
fn accelerometer_reoriented_is_parallel_to_gravity() {
    let Some((_f, g)) = open() else {
        eprintln!("SKIP: fixture {} is not present", common::REFERENCE_360);
        return;
    };
    let series = g.series();
    let grav = series.get(&(1, FourCc::from_str_lossy("GRAV"))).unwrap();
    let accl = series.get(&(1, FourCc::from_str_lossy("ACCL"))).unwrap();

    // This is the check that validates the whole axis convention: ACCL is
    // tagged ORIN=ZXY, and GRAV additionally has Y and Z swapped relative to
    // it. Get either wrong and the two vectors are not parallel.
    let a = gmax::gpmf::to_grav_frame(accl.sample(0), accl.orin.as_deref());
    let gv = grav.sample(0);
    let expected = gmax::math::scale(&[gv[0], gv[1], gv[2]], gmax::math::G);

    let cos = gmax::math::dot(&gmax::math::normalize(&a), &gmax::math::normalize(&expected));
    assert!(cos > 0.98, "cos angle between ACCL and GRAV was {cos}: {a:?} vs {expected:?}");

    // The magnitude should be near standard gravity, though the bike was
    // running so vibration puts it a few percent off.
    let mag = gmax::math::magnitude(&a);
    assert!((mag - gmax::math::G).abs() < 0.6, "|ACCL[0]| = {mag}");
}

#[test]
fn gpmf_parses_without_warnings() {
    let Some((_f, g)) = open() else {
        eprintln!("SKIP: fixture {} is not present", common::REFERENCE_360);
        return;
    };
    assert_eq!(g.payloads.len(), 1536);
    assert!(
        g.warnings.is_empty(),
        "unexpected GPMF warnings: {:?}",
        g.warnings
    );
    // One device, named for the camera model.
    for p in &g.payloads {
        assert_eq!(p.devices.len(), 1);
        assert_eq!(p.devices[0].name.as_deref(), Some("MAX2"));
    }
}

#[test]
fn every_stream_is_evenly_spaced_rather_than_bunched_at_payload_boundaries() {
    let Some((_f, g)) = open() else {
        eprintln!("SKIP: fixture {} is not present", common::REFERENCE_360);
        return;
    };
    // The sensors do not deliver a whole number of samples per 1.001 s
    // payload, so spreading each chunk at the nominal interval runs past the
    // next chunk's STMP. Those samples used to be nudged a nanosecond apart
    // against the boundary: 10 such pile-ups in GYRO, 4 in GPS9. Rescaled to
    // end before the next anchor, no step is less than half the mean.
    for ((_, key), s) in g.series() {
        let mean = 1.0 / s.hz().unwrap();
        let tightest = s.times.windows(2).map(|w| w[1] - w[0]).fold(f64::MAX, f64::min);
        assert!(
            tightest > mean / 2.0,
            "{key}: samples {tightest} s apart against a mean of {mean} s"
        );
    }
}
