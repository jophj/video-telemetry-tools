//! Container and GPMF parsing against synthetic bytes.
//!
//! The reference clip is 8.55 GB and lives outside the repository, so the
//! assertions in `gpmf.rs` skip on a fresh clone. These do not: every buffer
//! here is built by hand, byte by byte, to reach one path the reference clip
//! never exercises — a corrupt size, an unpadded element, a stream whose
//! metadata only appears once — and so they run everywhere.

use std::path::PathBuf;

use gmax::fourcc::FourCc;
use gmax::gpmf::{self, Gpmf};
use gmax::mp4::{Mp4, Mp4Error};

// ------------------------------------------------------------- GPMF building

/// One KLV element, padded to four bytes as a camera writes it.
fn klv(key: &[u8; 4], kind: u8, size: u8, repeat: u16, data: &[u8]) -> Vec<u8> {
    let mut out = klv_unpadded(key, kind, size, repeat, data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

fn klv_unpadded(key: &[u8; 4], kind: u8, size: u8, repeat: u16, data: &[u8]) -> Vec<u8> {
    assert_eq!(
        data.len(),
        size as usize * repeat as usize,
        "test KLV is inconsistent"
    );
    let mut out = key.to_vec();
    out.push(kind);
    out.push(size);
    out.extend_from_slice(&repeat.to_be_bytes());
    out.extend_from_slice(data);
    out
}

fn nest(key: &[u8; 4], children: &[Vec<u8>]) -> Vec<u8> {
    let body = children.concat();
    klv(key, 0, 1, body.len() as u16, &body)
}

fn devc(id: u32, streams: &[Vec<u8>]) -> Vec<u8> {
    let mut children = vec![klv(b"DVID", b'L', 4, 1, &id.to_be_bytes())];
    children.extend_from_slice(streams);
    nest(b"DEVC", &children)
}

fn strm(children: &[Vec<u8>]) -> Vec<u8> {
    nest(b"STRM", children)
}

fn stmp(micros: u64) -> Vec<u8> {
    klv(b"STMP", b'J', 8, 1, &micros.to_be_bytes())
}

fn scal(divisor: i16) -> Vec<u8> {
    klv(b"SCAL", b's', 2, 1, &divisor.to_be_bytes())
}

fn i16s(samples: &[[i16; 3]]) -> Vec<u8> {
    samples
        .iter()
        .flatten()
        .flat_map(|v| v.to_be_bytes())
        .collect()
}

fn accl(samples: &[[i16; 3]]) -> Vec<u8> {
    klv(b"ACCL", b's', 6, samples.len() as u16, &i16s(samples))
}

/// Parse payloads laid end to end, each `duration` seconds long.
fn parse(payloads: &[Vec<u8>], duration: f64) -> Gpmf {
    let data = payloads.concat();
    let mut spans = Vec::new();
    let mut at = 0;
    for (i, p) in payloads.iter().enumerate() {
        spans.push((at, p.len(), i as f64 * duration, duration));
        at += p.len();
    }
    Gpmf::parse(&data, &spans).expect("synthetic GPMF parses")
}

fn key(k: &str) -> (u32, FourCc) {
    (1, FourCc::from_str_lossy(k))
}

// -------------------------------------------------------------- GPMF parsing

#[test]
fn an_unpadded_final_element_ends_the_payload_instead_of_panicking() {
    // Six bytes of ACCL, not padded to eight, as the last thing in the STRM,
    // the STRM the last thing in the DEVC, and the DEVC the whole payload.
    // Rounding the final element up to four bytes steps past the end of every
    // one of those buffers.
    let accl = klv_unpadded(b"ACCL", b's', 6, 1, &i16s(&[[1, 2, 3]]));
    let body = accl.clone();
    let strm = klv_unpadded(b"STRM", 0, 1, body.len() as u16, &body);
    let body = [klv(b"DVID", b'L', 4, 1, &1u32.to_be_bytes()), strm].concat();
    let payload = klv_unpadded(b"DEVC", 0, 1, body.len() as u16, &body);
    assert_ne!(
        payload.len() % 4,
        0,
        "the test must end on an unpadded element"
    );

    let g = parse(&[payload], 1.0);
    assert!(g.warnings.is_empty(), "{:?}", g.warnings);
    let s = &g.series()[&key("ACCL")];
    assert_eq!(s.sample(0), &[1.0, 2.0, 3.0]);
}

#[test]
fn scal_and_type_carry_over_to_payloads_that_omit_them() {
    // Only the first payload says how to scale ACCL and how to lay out GPS9.
    // Decoded on its own, the second would come out ten times too large and
    // its GPS9 would be dropped for want of a layout.
    let gps = |lat: i32| {
        let mut b = lat.to_be_bytes().to_vec();
        b.extend_from_slice(&7u16.to_be_bytes());
        b.extend_from_slice(&[0, 0]); // two bytes of trailing padding in the struct
        klv(b"GPS9", b'?', 8, 1, &b)
    };
    let first = devc(
        1,
        &[
            strm(&[scal(10), accl(&[[10, 20, 30]])]),
            strm(&[
                klv(
                    b"SCAL",
                    b'l',
                    4,
                    2,
                    &[100i32.to_be_bytes(), 1i32.to_be_bytes()].concat(),
                ),
                klv(b"TYPE", b'c', 2, 1, b"lS"),
                gps(4_400),
            ]),
        ],
    );
    let second = devc(1, &[strm(&[accl(&[[40, 50, 60]])]), strm(&[gps(4_500)])]);

    let g = parse(&[first, second], 1.0);
    assert!(g.warnings.is_empty(), "{:?}", g.warnings);
    let series = g.series();
    let a = &series[&key("ACCL")];
    assert_eq!(a.sample(1), &[4.0, 5.0, 6.0]);
    let p = &series[&key("GPS9")];
    assert_eq!(p.len(), 2, "the second GPS9 needs the first payload's TYPE");
    assert_eq!(p.sample(1), &[45.0, 7.0]);
    // The chunk itself reports what it was decoded with.
    let chunk = &g.payloads[1].devices[0].streams[0];
    assert_eq!(chunk.scal, vec![10.0]);
}

#[test]
fn a_chunk_that_changes_width_is_skipped_with_a_warning() {
    let two_wide = klv(b"ACCL", b's', 4, 1, &[0, 7, 0, 8]);
    let g = parse(
        &[
            devc(1, &[strm(&[accl(&[[1, 2, 3]])])]),
            devc(1, &[strm(std::slice::from_ref(&two_wide))]),
            devc(1, &[strm(&[accl(&[[4, 5, 6]])])]),
            devc(1, &[strm(&[two_wide])]),
        ],
        1.0,
    );
    // Once, not once per payload: a recording is fifteen hundred of them.
    assert_eq!(g.warnings.len(), 1, "{:?}", g.warnings);
    assert!(g.warnings[0].contains("ACCL"), "{:?}", g.warnings);

    // Appended, the two-wide sample would have shifted the third payload's
    // values by two fields.
    let s = &g.series()[&key("ACCL")];
    assert_eq!(s.len(), 2);
    assert_eq!(s.times.len(), s.values.len() / 3);
    assert_eq!(s.sample(1), &[4.0, 5.0, 6.0]);
}

#[test]
fn a_struct_steps_over_text_fields_and_expands_array_notation() {
    // L, then one char, then S: 7 bytes. Dropping the `c` would read the S
    // from the char's byte.
    let mut lcs = 70_000u32.to_be_bytes().to_vec();
    lcs.push(b'x');
    lcs.extend_from_slice(&513u16.to_be_bytes());
    // f[2]L: two floats then a u32, 12 bytes. Without expanding the bracket
    // the L would be read from the second float.
    let mut farr = 1.5f32.to_be_bytes().to_vec();
    farr.extend_from_slice(&(-2.0f32).to_be_bytes());
    farr.extend_from_slice(&9u32.to_be_bytes());
    let g = parse(
        &[devc(
            1,
            &[
                strm(&[
                    klv(b"TYPE", b'c', 3, 1, b"LcS"),
                    klv(b"MAGN", b'?', 7, 1, &lcs),
                ]),
                strm(&[
                    klv(b"TYPE", b'c', 5, 1, b"f[2]L"),
                    klv(b"GYRO", b'?', 12, 1, &farr),
                ]),
            ],
        )],
        1.0,
    );
    assert!(g.warnings.is_empty(), "{:?}", g.warnings);
    let series = g.series();
    let m = &series[&key("MAGN")];
    assert_eq!(m.components, 2);
    assert_eq!(m.sample(0), &[70_000.0, 513.0]);
    let y = &series[&key("GYRO")];
    assert_eq!(y.components, 3);
    assert_eq!(y.sample(0), &[1.5, -2.0, 9.0]);
}

#[test]
fn fixed_point_fields_decode_as_q15_16_and_q31_32() {
    let q = (-98_304i32).to_be_bytes(); // -1.5 in Q15.16
    let big_q = (3i64 << 31).to_be_bytes(); // 1.5 in Q31.32
    let g = parse(
        &[devc(
            1,
            &[
                strm(&[klv(b"ACCL", b'q', 4, 1, &q)]),
                strm(&[klv(b"GYRO", b'Q', 8, 1, &big_q)]),
            ],
        )],
        1.0,
    );
    let series = g.series();
    assert_eq!(series[&key("ACCL")].sample(0), &[-1.5]);
    assert_eq!(series[&key("GYRO")].sample(0), &[1.5]);
}

#[test]
fn a_struct_wider_than_its_samples_is_refused_rather_than_misread() {
    // `llll` is 16 bytes, the samples 8: each sample's tail would come from
    // the next sample, and the last would be half a sample.
    let data: Vec<u8> = (1..=4i32).flat_map(|v| v.to_be_bytes()).collect();
    let g = parse(
        &[devc(
            1,
            &[strm(&[
                klv(b"TYPE", b'c', 4, 1, b"llll"),
                klv(b"GPS9", b'?', 8, 2, &data),
            ])],
        )],
        1.0,
    );
    assert_eq!(g.warnings.len(), 1, "{:?}", g.warnings);
    assert!(g.series()[&key("GPS9")].is_empty());
}

#[test]
fn samples_are_respaced_to_end_before_the_next_chunks_anchor() {
    // Four samples per 1 s payload, but the camera stamps the second chunk
    // at 0.5 s. Spread at the nominal 0.25 s they would run past it to
    // 0.75 s, and the old nudge bunched them a nanosecond apart.
    let chunk = |us| devc(1, &[strm(&[stmp(us), accl(&[[1, 1, 1]; 4])])]);
    let g = parse(&[chunk(0), chunk(500_000)], 1.0);
    let t = &g.series()[&key("ACCL")].times;
    let expect = [0.0, 0.125, 0.25, 0.375, 0.5, 0.75, 1.0, 1.25];
    for (a, b) in t.iter().zip(expect) {
        assert!((a - b).abs() < 1e-9, "times {t:?}");
    }
}

#[test]
fn chunks_without_stmp_join_the_stmp_clock_rather_than_the_container_clock() {
    // A clip cut 100 s into a ride: its container clock starts at zero but
    // STMP keeps the ride's time. A chunk missing its STMP belongs 1 s after
    // the one before, at 101 s, not at the container's 1 s.
    let g = parse(
        &[
            devc(1, &[strm(&[stmp(100_000_000), accl(&[[1, 1, 1]; 2])])]),
            devc(1, &[strm(&[accl(&[[1, 1, 1]; 2])])]),
        ],
        1.0,
    );
    let t = &g.series()[&key("ACCL")].times;
    let expect = [100.0, 100.5, 101.0, 101.5];
    for (a, b) in t.iter().zip(expect) {
        assert!((a - b).abs() < 1e-9, "times {t:?}");
    }
}

#[test]
fn data_after_the_zero_padding_is_reported() {
    let mut payload = devc(1, &[strm(&[accl(&[[1, 2, 3]])])]);
    payload.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, b'J', b'U', b'N', b'K']);
    let g = parse(&[payload], 1.0);
    assert_eq!(g.warnings.len(), 1, "{:?}", g.warnings);
    assert!(g.warnings[0].contains("zero padding"), "{:?}", g.warnings);
    assert_eq!(g.series()[&key("ACCL")].len(), 1);
}

// -------------------------------------------------------------- MP4 building

fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn u32s(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

fn mvhd(version: u8, created: u64) -> Vec<u8> {
    let mut b = vec![version, 0, 0, 0];
    if version == 1 {
        b.extend_from_slice(&created.to_be_bytes());
        b.extend_from_slice(&0u64.to_be_bytes());
        b.extend_from_slice(&1000u32.to_be_bytes());
        b.extend_from_slice(&2000u64.to_be_bytes());
    } else {
        b.extend_from_slice(&u32s(&[created as u32, 0, 1000, 2000]));
    }
    b.extend_from_slice(&[0; 80]);
    mp4_box(b"mvhd", &b)
}

/// The sample-table boxes a test may want to replace.
struct Tables {
    stsz: Vec<u8>,
    stco: Vec<u8>,
    edts: Option<Vec<u8>>,
}

impl Tables {
    /// One chunk at `offset` holding samples of the given sizes.
    fn one_chunk(offset: u32, sizes: &[u32]) -> Self {
        let mut stsz = u32s(&[0, 0, sizes.len() as u32]);
        stsz.extend_from_slice(&u32s(sizes));
        Tables {
            stsz: mp4_box(b"stsz", &stsz),
            stco: mp4_box(b"stco", &u32s(&[0, 1, offset])),
            edts: None,
        }
    }
}

fn gpmd_trak(id: u32, samples: u32, t: &Tables) -> Vec<u8> {
    let mut tkhd = vec![0u8; 4];
    tkhd.extend_from_slice(&u32s(&[0, 0, id, 0, 0]));
    tkhd.extend_from_slice(&[0; 8 + 8 + 36 + 8]);
    let mdhd = mp4_box(
        b"mdhd",
        &[u32s(&[0, 0, 0, 1000, samples * 1000]), vec![0; 4]].concat(),
    );
    let hdlr = mp4_box(
        b"hdlr",
        &[u32s(&[0, 0]), b"meta".to_vec(), vec![0; 13]].concat(),
    );
    let stsd = mp4_box(
        b"stsd",
        &[u32s(&[0, 1, 16]), b"gpmd".to_vec(), vec![0; 8]].concat(),
    );
    let stts = mp4_box(b"stts", &u32s(&[0, 1, samples, 1000]));
    let stsc = mp4_box(b"stsc", &u32s(&[0, 1, 1, samples, 1]));
    let stbl = mp4_box(
        b"stbl",
        &[stsd, stts, t.stsz.clone(), stsc, t.stco.clone()].concat(),
    );
    let minf = mp4_box(b"minf", &stbl);
    let mdia = mp4_box(b"mdia", &[mdhd, hdlr, minf].concat());
    let mut body = mp4_box(b"tkhd", &tkhd);
    if let Some(edts) = &t.edts {
        body.extend_from_slice(edts);
    }
    body.extend_from_slice(&mdia);
    mp4_box(b"trak", &body)
}

/// Two GPMF payloads, one ACCL sample each, the second stamped 1 s on.
fn two_payloads() -> Vec<Vec<u8>> {
    vec![
        devc(1, &[strm(&[stmp(0), scal(2), accl(&[[2, 4, 6]])])]),
        devc(1, &[strm(&[stmp(1_000_000), accl(&[[8, 10, 12]])])]),
    ]
}

/// ftyp, then an mdat holding `payloads`, then a moov whose tracks the
/// caller builds from the offset of the first payload and the sizes.
fn mp4_bytes(payloads: &[Vec<u8>], moov_extra: impl Fn(u32, &[u32]) -> Vec<Vec<u8>>) -> Vec<u8> {
    let ftyp = mp4_box(
        b"ftyp",
        &[b"mp41".to_vec(), vec![0; 4], b"mp41".to_vec()].concat(),
    );
    let data = payloads.concat();
    let offset = (ftyp.len() + 8) as u32;
    let sizes: Vec<u32> = payloads.iter().map(|p| p.len() as u32).collect();
    let mut moov = mvhd(0, 3_000_000_000);
    for trak in moov_extra(offset, &sizes) {
        moov.extend_from_slice(&trak);
    }
    [ftyp, mp4_box(b"mdat", &data), mp4_box(b"moov", &moov)].concat()
}

fn good_mp4() -> Vec<u8> {
    mp4_bytes(&two_payloads(), |offset, sizes| {
        vec![gpmd_trak(
            4,
            sizes.len() as u32,
            &Tables::one_chunk(offset, sizes),
        )]
    })
}

/// A scratch file unique to this test, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str, bytes: &[u8]) -> Self {
        let path =
            std::env::temp_dir().join(format!("gmax-parsers-{}-{name}.mp4", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// --------------------------------------------------------------- MP4 parsing

#[test]
fn a_synthetic_file_opens_and_its_telemetry_parses_without_the_fixture() {
    let f = Scratch::new("good", &good_mp4());
    let mut mp4 = Mp4::open(&f.0).unwrap();
    assert!(mp4.warnings.is_empty(), "{:?}", mp4.warnings);
    assert_eq!(
        mp4.major_brand.map(|b| b.to_string()).as_deref(),
        Some("mp41")
    );
    assert_eq!(mp4.creation_time, Some(3_000_000_000 - 2_082_844_800));
    let track = mp4.gpmd_track().unwrap();
    assert_eq!(track.samples.len(), 2);
    assert!(track.edit_list_is_trivial());

    let g = gpmf::from_mp4(&mut mp4).unwrap();
    assert!(g.warnings.is_empty(), "{:?}", g.warnings);
    let s = &g.series()[&key("ACCL")];
    assert_eq!(s.times, vec![0.0, 1.0]);
    // SCAL came from the first payload only.
    assert_eq!(s.sample(1), &[4.0, 5.0, 6.0]);
}

#[test]
fn a_track_with_a_broken_sample_table_is_skipped_not_fatal() {
    let bytes = mp4_bytes(&two_payloads(), |offset, sizes| {
        // Three samples at one per chunk, but only one chunk offset.
        let mut broken = Tables::one_chunk(offset, sizes);
        broken.stsz = mp4_box(b"stsz", &u32s(&[0, 0, 3, 1, 1, 1]));
        let bad = gpmd_trak(9, 1, &broken);
        vec![
            bad,
            gpmd_trak(4, sizes.len() as u32, &Tables::one_chunk(offset, sizes)),
        ]
    });
    let f = Scratch::new("broken-track", &bytes);
    let mut mp4 = Mp4::open(&f.0).expect("one bad track must not fail the file");
    assert_eq!(mp4.warnings.len(), 1, "{:?}", mp4.warnings);
    assert!(mp4.warnings[0].contains("track 9"), "{:?}", mp4.warnings);
    assert_eq!(mp4.tracks.len(), 1);
    // The warning travels with the telemetry, so the user sees it.
    let g = gpmf::from_mp4(&mut mp4).unwrap();
    assert!(
        g.warnings.iter().any(|w| w.contains("track 9")),
        "{:?}",
        g.warnings
    );
}

#[test]
fn a_uniform_sample_size_that_cannot_fit_the_file_is_refused() {
    // A billion samples of a megabyte each: taken at its word this is a
    // four-gigabyte table of sizes, allocated before anything is checked.
    let bytes = mp4_bytes(&two_payloads(), |offset, sizes| {
        let mut t = Tables::one_chunk(offset, sizes);
        t.stsz = mp4_box(b"stsz", &u32s(&[0, 1 << 20, 1 << 30]));
        vec![gpmd_trak(4, 2, &t)]
    });
    let f = Scratch::new("uniform", &bytes);
    let mp4 = Mp4::open(&f.0).unwrap();
    assert!(mp4.tracks.is_empty());
    assert!(mp4.warnings[0].contains("cannot fit"), "{:?}", mp4.warnings);
}

#[test]
fn an_stz2_with_an_invalid_field_size_is_refused_rather_than_read_as_16_bit() {
    let bytes = mp4_bytes(&two_payloads(), |offset, sizes| {
        let mut t = Tables::one_chunk(offset, sizes);
        t.stsz = mp4_box(b"stz2", &[0, 0, 0, 0, 0, 0, 0, 12, 0, 0, 0, 2, 0, 0, 0, 0]);
        vec![gpmd_trak(4, 2, &t)]
    });
    let f = Scratch::new("stz2", &bytes);
    let mp4 = Mp4::open(&f.0).unwrap();
    assert!(mp4.tracks.is_empty());
    assert!(mp4.warnings[0].contains("stz2"), "{:?}", mp4.warnings);
}

#[test]
fn a_sample_outside_the_file_is_an_error_not_an_allocation() {
    let bytes = mp4_bytes(&two_payloads(), |_, sizes| {
        vec![gpmd_trak(4, 2, &Tables::one_chunk(u32::MAX - 64, sizes))]
    });
    let f = Scratch::new("outside", &bytes);
    let mut mp4 = Mp4::open(&f.0).unwrap();
    let id = mp4.gpmd_track().unwrap().id;
    assert!(matches!(
        mp4.read_track_data(id),
        Err(Mp4Error::SampleOutOfRange { .. })
    ));
}

#[test]
fn a_chunk_offset_that_overflows_skips_the_track() {
    let bytes = mp4_bytes(&two_payloads(), |_, sizes| {
        let mut t = Tables::one_chunk(0, sizes);
        t.stco = mp4_box(
            b"co64",
            &[u32s(&[0, 1]), (u64::MAX - 4).to_be_bytes().to_vec()].concat(),
        );
        vec![gpmd_trak(4, 2, &t)]
    });
    let f = Scratch::new("overflow", &bytes);
    let mp4 = Mp4::open(&f.0).unwrap();
    assert!(mp4.tracks.is_empty());
    assert!(
        mp4.warnings[0].contains("address space"),
        "{:?}",
        mp4.warnings
    );
}

#[test]
fn a_creation_time_past_i64_is_treated_as_absent() {
    let mut bytes = good_mp4();
    let v0 = mvhd(0, 3_000_000_000);
    let v1 = mvhd(1, u64::MAX);
    let at = bytes
        .windows(v0.len())
        .position(|w| w == v0.as_slice())
        .unwrap();
    bytes.splice(at..at + v0.len(), v1.iter().copied());
    // The moov grew by the difference; fix its size.
    let moov = bytes.windows(4).rposition(|w| w == b"moov").unwrap() - 4;
    let size = (bytes.len() - moov) as u32;
    bytes[moov..moov + 4].copy_from_slice(&size.to_be_bytes());

    let f = Scratch::new("mvhd-v1", &bytes);
    let mp4 = Mp4::open(&f.0).unwrap();
    assert_eq!(mp4.creation_time, None);
    assert_eq!(mp4.gpmd_track().unwrap().samples.len(), 2);
}

#[test]
fn a_top_level_box_smaller_than_its_header_is_named_not_reported_as_no_moov() {
    let mut bytes = mp4_box(b"ftyp", b"mp41\0\0\0\0");
    bytes.extend_from_slice(&[0, 0, 0, 4, b'j', b'u', b'n', b'k']);
    bytes.extend_from_slice(&good_mp4());
    let f = Scratch::new("tiny-box", &bytes);
    match Mp4::open(&f.0) {
        Err(Mp4Error::BadBoxSize { offset, size, .. }) => assert_eq!((offset, size), (16, 4)),
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("opened a file with a corrupt top-level box"),
    }
}

#[test]
fn a_child_box_smaller_than_its_header_is_warned_about() {
    let bytes = mp4_bytes(&two_payloads(), |offset, sizes| {
        vec![
            gpmd_trak(4, sizes.len() as u32, &Tables::one_chunk(offset, sizes)),
            // A three-byte box: every box after it in the moov would vanish.
            vec![0, 0, 0, 3, b'j', b'u', b'n', b'k'],
        ]
    });
    let f = Scratch::new("tiny-child", &bytes);
    let mp4 = Mp4::open(&f.0).unwrap();
    assert_eq!(mp4.tracks.len(), 1);
    assert!(
        mp4.warnings.iter().any(|w| w.contains("size of 3")),
        "{:?}",
        mp4.warnings
    );
}

#[test]
fn an_edit_list_is_read_and_reported_when_it_could_move_samples() {
    // An empty edit of 40 ms, then the media: what a stream copy writes.
    let elst = [u32s(&[0, 2, 40, u32::MAX, 1 << 16, 2000, 0, 1 << 16])].concat();
    let edts = mp4_box(b"edts", &mp4_box(b"elst", &elst));
    // Without STMP the payload times are all there is, so the edit matters.
    let unstamped = vec![
        devc(1, &[strm(&[accl(&[[1, 1, 1]])])]),
        devc(1, &[strm(&[accl(&[[1, 1, 1]])])]),
    ];
    let build = |payloads: &[Vec<u8>]| {
        mp4_bytes(payloads, |offset, sizes| {
            let mut t = Tables::one_chunk(offset, sizes);
            t.edts = Some(edts.clone());
            vec![gpmd_trak(4, sizes.len() as u32, &t)]
        })
    };

    let f = Scratch::new("elst", &build(&unstamped));
    let mut mp4 = Mp4::open(&f.0).unwrap();
    let track = mp4.gpmd_track().unwrap();
    assert_eq!(track.edits.len(), 2);
    assert_eq!(track.edits[0].media_time, -1);
    assert!(!track.edit_list_is_trivial());
    let g = gpmf::from_mp4(&mut mp4).unwrap();
    assert!(
        g.warnings.iter().any(|w| w.contains("edit list")),
        "{:?}",
        g.warnings
    );

    // With STMP throughout, the edit list times nothing and is not worth a
    // warning on every clip ever cut.
    let f = Scratch::new("elst-stamped", &build(&two_payloads()));
    let mut mp4 = Mp4::open(&f.0).unwrap();
    let g = gpmf::from_mp4(&mut mp4).unwrap();
    assert!(g.warnings.is_empty(), "{:?}", g.warnings);
}

// ------------------------------------------------------------ never panicking

/// xorshift64: deterministic, so a failure reproduces, and no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Damage `bytes` a few random bytes at a time. Setting a byte to 0x00 or
/// 0xff is more likely than a random value to hit a size's edge cases.
fn damage(bytes: &mut [u8], rng: &mut Rng) {
    for _ in 0..1 + rng.below(4) {
        let i = rng.below(bytes.len());
        bytes[i] = match rng.below(3) {
            0 => 0,
            1 => 0xff,
            _ => rng.next() as u8,
        };
    }
}

#[test]
fn gpmf_parsing_never_panics_on_truncated_or_damaged_payloads() {
    let payloads = [
        two_payloads(),
        vec![devc(
            1,
            &[
                strm(&[
                    klv(b"TYPE", b'c', 6, 1, b"f[2]Lc"),
                    klv(b"GPS9", b'?', 13, 1, &[1; 13]),
                ]),
                strm(&[klv(b"ACCL", b'q', 4, 2, &[7; 8])]),
            ],
        )],
    ]
    .concat();
    let data = payloads.concat();
    let whole = |data: &[u8]| {
        let _ = Gpmf::parse(data, &[(0, data.len(), 0.0, 1.0)]).map(|g| g.series());
        let _ =
            Gpmf::parse(data, &[(0, 40, 0.0, 1.0), (40, data.len(), 1.0, 0.0)]).map(|g| g.series());
    };
    for len in 0..=data.len() {
        whole(&data[..len]);
    }
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..20_000 {
        let mut d = data.clone();
        damage(&mut d, &mut rng);
        whole(&d);
    }
}

#[test]
fn mp4_opening_never_panics_on_truncated_or_damaged_files() {
    let good = good_mp4();
    let f = Scratch::new("fuzz", &good);
    let attempt = |bytes: &[u8]| {
        std::fs::write(&f.0, bytes).unwrap();
        if let Ok(mut mp4) = Mp4::open(&f.0) {
            let _ = gpmf::from_mp4(&mut mp4).map(|g| g.series());
        }
    };
    for len in 0..=good.len() {
        attempt(&good[..len]);
    }
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for _ in 0..3_000 {
        let mut d = good.clone();
        damage(&mut d, &mut rng);
        attempt(&d);
    }
}
