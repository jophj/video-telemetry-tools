//! The viewer's byte-range handling and its CSV-to-JSON path.

mod common;

use gmax::viewer::parse_range;

const SIZE: u64 = 911_496_707;

#[test]
fn explicit_ranges_are_inclusive() {
    assert_eq!(parse_range("bytes=0-1023", SIZE), Some((0, 1023)));
    assert_eq!(parse_range("bytes=100-199", SIZE), Some((100, 199)));
    // A single byte is a legal range.
    assert_eq!(parse_range("bytes=5-5", SIZE), Some((5, 5)));
}

#[test]
fn a_suffix_range_finds_the_end_of_the_file() {
    // This is the request a player makes first, to read the moov atom that
    // sits at the end of a non-faststart MP4. Getting it wrong means the video
    // never plays at all.
    assert_eq!(parse_range("bytes=-2048", SIZE), Some((SIZE - 2048, SIZE - 1)));
    // A suffix longer than the file is the whole file, not an error.
    assert_eq!(parse_range("bytes=-99999999999", SIZE), Some((0, (1 << 20) - 1)));
    // Zero bytes is meaningless.
    assert_eq!(parse_range("bytes=-0", SIZE), None);
}

#[test]
fn an_open_ended_range_is_capped() {
    // Without a cap, seeking near the start of an 800 MB file would stream the
    // whole remainder in one response and defeat seeking entirely.
    let (start, end) = parse_range("bytes=500000000-", SIZE).unwrap();
    assert_eq!(start, 500_000_000);
    assert_eq!(end - start + 1, 1 << 20);

    // The cap also applies to an over-large explicit range.
    let (start, end) = parse_range("bytes=0-99999999", SIZE).unwrap();
    assert_eq!((start, end), (0, (1 << 20) - 1));
}

#[test]
fn an_end_past_the_file_is_clamped_not_rejected() {
    let small = 100;
    assert_eq!(parse_range("bytes=50-999", small), Some((50, 99)));
}

#[test]
fn unsatisfiable_and_malformed_ranges_are_refused() {
    assert_eq!(parse_range("bytes=99999999999-", SIZE), None);
    assert_eq!(parse_range("bytes=abc-def", SIZE), None);
    assert_eq!(parse_range("bytes=-", SIZE), None);
    assert_eq!(parse_range("bytes=200-100", SIZE), None, "start after end");
    assert_eq!(parse_range("items=0-10", SIZE), None, "wrong unit");
    assert_eq!(parse_range("0-10", SIZE), None, "no unit");
    // Multipart ranges are legal HTTP but no browser sends one for video, and
    // answering one wrongly is worse than declining.
    assert_eq!(parse_range("bytes=0-10,20-30", SIZE), None);
    // An empty file has no satisfiable range.
    assert_eq!(parse_range("bytes=0-10", 0), None);
}

#[test]
fn whitespace_is_tolerated() {
    assert_eq!(parse_range("  bytes= 0 - 1023 ", SIZE), Some((0, 1023)));
}

#[test]
fn builds_viewer_json_from_a_csv() {
    // The viewer is usually pointed at a stitched or proxy rendering, which has
    // no GPMF track, so the channels come from the CSV instead.
    let csv = "time_s,utc,lat,lon,alt_m,speed_kmh,speed_3d_kmh,course_deg,lean_deg,\
lean_rate_dps,pitch_deg,pitch_rate_dps,yaw_rate_dps,accel_lon_g,accel_lat_g,accel_vert_g,\
accel_total_g,gps_dop,imu_temp_c,gps_valid\n\
0.000,2026-09-08T11:06:47.103Z,,,,,,,-14.67,-0.1,-1.09,-0.7,0.2,0.005,-0.000,0.004,1.004,,46.2,0\n\
0.033,2026-09-08T11:06:47.136Z,44.1725166,11.3394181,511.55,1.35,0.00,,-14.67,0.3,-1.10,0.3,-0.0,0.012,0.001,-0.004,0.996,3.31,46.2,1\n\
0.067,2026-09-08T11:06:47.170Z,44.1725166,11.3394181,511.55,1.35,0.00,201.4,-14.60,-0.2,-1.10,-0.8,0.1,0.004,0.002,0.000,1.000,3.31,46.2,1\n";

    let json = gmax::telemetry::output::json_from_csv(csv, Some("proxy.mp4"), Some(600.0)).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

    assert_eq!(v["n"], 3);
    assert_eq!(v["video"], "proxy.mp4");
    assert_eq!(v["video_dur_s"], 600.0);
    assert_eq!(v["start_utc"], "2026-09-08T11:06:47.103Z");
    // Two of three rows carry a fix.
    assert_eq!(v["gps_valid"], serde_json::json!([0, 1, 1]));
    assert!((v["gps_fix_pct"].as_f64().unwrap() - 66.7).abs() < 0.1);

    // The rate is measured from the CSV's own timestamps rather than assumed.
    // Over three rows spanning 0.067 s that is 2 / 0.067 = 29.851 Hz -- and
    // pointedly not 30, which is what makes it a measurement.
    let hz = v["hz"].as_f64().unwrap();
    assert!((hz - 29.8507).abs() < 0.001, "hz was {hz}");
    assert_ne!(hz, 30.0);

    // Every channel the viewer expects is present and the right length.
    for key in [
        "lean", "leanrate", "pitch", "pitchrate", "yaw", "speed", "alon", "alat", "avert",
        "atot", "alt", "course", "lat", "lon", "temp", "dop",
    ] {
        let ch = v["ch"][key].as_array().unwrap_or_else(|| panic!("{key} missing"));
        assert_eq!(ch.len(), 3, "{key} has the wrong length");
    }

    // A blank cell becomes null, not zero: the first row has no fix, so it has
    // no position, and reading it as 0,0 would put the ride off West Africa.
    assert!(v["ch"]["lat"][0].is_null());
    assert_eq!(v["ch"]["lat"][1], 44.172517);
    // Absent for two rows, present for the third.
    assert!(v["ch"]["course"][1].is_null());
    assert_eq!(v["ch"]["course"][2], 201.4);
    // Lean is always available, GPS or not.
    assert_eq!(v["ch"]["lean"][0], -14.67);

    // A channel with no data anywhere is reported as missing so the viewer can
    // hide its gauge rather than draw an empty one.
    let missing = v["missing"].as_array().unwrap();
    assert!(missing.is_empty(), "unexpectedly missing: {missing:?}");
}

#[test]
fn csv_without_a_header_is_refused() {
    assert!(gmax::telemetry::output::json_from_csv("", None, None).is_err());
    assert!(gmax::telemetry::output::json_from_csv("time_s,utc\n", None, None).is_err());
}

#[test]
fn the_viewer_page_no_longer_compensates_for_fixed_bugs() {
    // These two defaults existed only to undo bugs that are now fixed at
    // source: the lateral sign flip and a 72-degree pitch to see past the
    // unlevelled horizon. If the page ever reverts, the viewer would show
    // lean backwards again.
    let html = include_str!("../assets/viewer.html");
    assert!(
        html.contains("flipLateral:false"),
        "the lateral sign flip must default to off"
    );
    assert!(
        html.contains("const sign = () => A.flipLateral ? -1 : 1;"),
        "unflipped must be the default path"
    );
    assert!(
        html.contains("const DEFAULT_VIEW = {yaw:0, pitch:0, roll:0, fov:95};"),
        "the default view must look straight ahead"
    );
    // And the saved-state key must have moved, or a stale entry from the Python
    // viewer would restore the old always-on flip.
    assert!(html.contains("gopro-telemetry-viewer/align2"));
}

#[test]
fn a_csv_that_does_not_start_at_zero_reports_its_length_not_its_end() {
    // A clip cut by a stream copy keeps the GPMF's original timestamps, so
    // `gmax telemetry` on an eighteen-minutes-in clip produces a CSV whose
    // time_s column starts at 1098. Reading the last value as the duration
    // made the viewer think an eleven-second clip was eighteen minutes long,
    // and squeezed its whole trace into the first percent of the scrubber.
    let head = "time_s,utc,lat,lon,alt_m,speed_kmh,speed_3d_kmh,course_deg,lean_deg,\
lean_rate_dps,pitch_deg,pitch_rate_dps,yaw_rate_dps,accel_lon_g,accel_lat_g,accel_vert_g,\
accel_total_g,gps_dop,imu_temp_c,gps_valid\n";
    let row = |t: f64| {
        format!("{t:.3},2026-09-12T11:16:03.411Z,,,,,,,-14.67,-0.1,-1.09,-0.7,0.2,0.005,-0.000,0.004,1.004,,46.2,0\n")
    };
    let csv = format!("{head}{}{}{}", row(1098.157), row(1098.190), row(1098.224));
    let v: serde_json::Value =
        serde_json::from_str(&gmax::telemetry::output::json_from_csv(&csv, None, None).unwrap())
            .unwrap();
    let dur = v["dur_s"].as_f64().unwrap();
    assert!((dur - 0.067).abs() < 1e-3, "dur_s was {dur}, expected the span");
    // The rate is still measured correctly from the same column.
    assert!((v["hz"].as_f64().unwrap() - 29.9).abs() < 1.0, "hz {}", v["hz"]);
}

#[test]
fn only_this_servers_own_name_is_accepted_as_host() {
    use gmax::viewer::host_allowed;
    for ok in ["127.0.0.1:8360", "localhost:8360", "LOCALHOST:8360", "[::1]:8360"] {
        assert!(host_allowed(Some(ok), 8360), "{ok} was refused");
    }
    // A rebinding page arrives under its own name, or the right name on the
    // wrong port; and a request without a Host is not a browser's.
    for bad in ["evil.example:8360", "127.0.0.1:80", "localhost", "127.0.0.1.evil:8360", ""] {
        assert!(!host_allowed(Some(bad), 8360), "{bad} was accepted");
    }
    assert!(!host_allowed(None, 8360));
}

/// The viewer running as a real process, killed when the test ends.
struct Viewer(std::process::Child);

impl Drop for Viewer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn binary() -> std::path::PathBuf {
    let mut p = std::env::current_exe().expect("test binary path");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join(if cfg!(windows) { "gmax.exe" } else { "gmax" })
}

/// Send one raw request and return the whole response, or `None` on timeout.
fn get(port: u16, path: &str, host: &str) -> Option<String> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(3))).ok()?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").ok()?;
    let mut out = Vec::new();
    s.read_to_end(&mut out).ok()?;
    Some(String::from_utf8_lossy(&out).into_owned())
}

#[test]
fn a_stalled_video_download_does_not_block_the_page() {
    let dir = std::env::temp_dir().join(format!("gmax-viewer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Far larger than any socket buffer, so a whole-file response that nobody
    // reads blocks its writer.
    let video = dir.join("big.mp4");
    std::fs::write(&video, vec![0u8; 32 << 20]).unwrap();
    let csv = dir.join("big.telemetry.csv");
    std::fs::write(
        &csv,
        "time_s,utc,lean_deg\n0.000,2026-09-08T11:06:47.103Z,1.0\n0.033,2026-09-08T11:06:47.136Z,2.0\n",
    )
    .unwrap();

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _viewer = Viewer(
        std::process::Command::new(binary())
            .args(["viewer", video.to_str().unwrap(), "--no-open", "--port", &port.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("run gmax viewer"),
    );
    let host = format!("127.0.0.1:{port}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while get(port, "/info.json", &host).is_none() {
        assert!(std::time::Instant::now() < deadline, "the viewer never came up");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Ask for the whole video and never read a byte of it.
    use std::io::Write;
    let mut stalled = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(stalled, "GET /video HTTP/1.1\r\nHost: {host}\r\n\r\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));

    let telemetry = get(port, "/telemetry.json", &host).expect("telemetry blocked by the stall");
    assert!(telemetry.starts_with("HTTP/1.1 200"), "{telemetry}");
    let info = get(port, "/info.json", &host).expect("info blocked by the stall");
    // The caption names the CSV when the telemetry carries no clip name.
    assert!(info.contains("\"telemetry\":\"big.telemetry.csv\""), "{info}");

    // And a page arriving under another name is turned away.
    let rebound = get(port, "/telemetry.json", &format!("evil.example:{port}")).unwrap();
    assert!(rebound.starts_with("HTTP/1.1 403"), "{rebound}");

    drop(stalled);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_viewer_caption_is_built_without_inner_html() {
    // The clip name is a file name, and a file can be named like markup.
    let html = include_str!("../assets/viewer.html");
    assert!(!html.contains("el('note').innerHTML"), "the caption is parsed as HTML again");
    assert!(html.contains("function renderNote("));
    // The frame step follows the video rather than assuming 29.97.
    assert!(html.contains("FRAME = 1 / info.fps"));
}
