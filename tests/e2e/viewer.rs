//! `gmax viewer` serving a section, as a real process over real HTTP.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::common::sections::{gmax_binary, section_or_skip};
use crate::support::parse_json;

/// The viewer process, killed when the test ends however it ends.
struct Viewer(Child);

impl Drop for Viewer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A response split into its status line, headers and body.
struct Response {
    status: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn request(port: u16, path: &str, extra: &str) -> Option<Response> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}Connection: close\r\n\r\n"
    )
    .ok()?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).ok()?;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines.next()?.to_string();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Some(Response {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    })
}

#[test]
fn the_viewer_serves_a_sections_telemetry_and_video_ranges() {
    let video = section_or_skip!("hard_corner");
    let size = video.metadata().unwrap().len();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _viewer = Viewer(
        Command::new(gmax_binary())
            .arg("viewer")
            .arg(&video)
            .args(["--no-open", "--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("run gmax viewer"),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let telemetry = loop {
        if let Some(r) = request(port, "/telemetry.json", "") {
            break r;
        }
        assert!(Instant::now() < deadline, "the viewer never came up");
        std::thread::sleep(Duration::from_millis(50));
    };

    assert!(
        telemetry.status.starts_with("HTTP/1.1 200"),
        "{}",
        telemetry.status
    );
    assert_eq!(telemetry.header("Content-Type"), Some("application/json"));
    let json = parse_json(&String::from_utf8_lossy(&telemetry.body));
    // From the section's own GPMF track: 11 s at 29.97 Hz, named for the file.
    let n = json["n"].as_u64().unwrap();
    assert!((300..=340).contains(&n), "{n} samples");
    assert_eq!(json["video"], "hard_corner.360");
    assert_eq!(json["ch"]["lean"].as_array().unwrap().len() as u64, n);

    // A ranged request, as a browser's video element makes when scrubbing.
    let r = request(port, "/video", "Range: bytes=1000-1999\r\n").expect("no response for a range");
    assert!(r.status.starts_with("HTTP/1.1 206"), "{}", r.status);
    assert_eq!(
        r.header("Content-Range"),
        Some(format!("bytes 1000-1999/{size}").as_str())
    );
    assert_eq!(r.header("Accept-Ranges"), Some("bytes"));
    assert_eq!(r.body.len(), 1000);
    let mut expected = vec![0u8; 1000];
    let mut f = std::fs::File::open(&video).unwrap();
    std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(1000)).unwrap();
    f.read_exact(&mut expected).unwrap();
    assert!(
        r.body == expected,
        "the range served is not those bytes of the file"
    );

    // The tail of the file, as a suffix range.
    let r = request(port, "/video", "Range: bytes=-16\r\n").unwrap();
    assert!(r.status.starts_with("HTTP/1.1 206"), "{}", r.status);
    assert_eq!(r.body.len(), 16);

    // And a range wholly past the end is refused, naming the real size.
    let r = request(port, "/video", &format!("Range: bytes={}-\r\n", size + 10)).unwrap();
    assert!(r.status.starts_with("HTTP/1.1 416"), "{}", r.status);
    assert_eq!(
        r.header("Content-Range"),
        Some(format!("bytes */{size}").as_str())
    );
}

#[test]
fn the_viewer_refuses_a_gpx_as_its_telemetry_by_name() {
    // Regression: a GPX fell through to the MP4 reader and was refused as
    // "not an ISO base media file". It has none of the per-frame channels the
    // viewer plays, so the refusal should say that it is a GPX.
    let video = section_or_skip!("hard_corner");
    let Some(phone) = crate::common::fixture(crate::common::REFERENCE_PHONE_GPX) else {
        eprintln!("SKIP: the phone GPX is not present on this machine");
        return;
    };
    // Spawned with a deadline: if the refusal ever regressed into serving,
    // the test must fail rather than hang on a server that never exits.
    let mut child = Command::new(crate::common::sections::gmax_binary())
        .args(["viewer".as_ref(), video.as_os_str(), "--telemetry".as_ref(), phone.as_os_str()])
        .args(["--port", "0"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run gmax viewer");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("gmax viewer served a GPX instead of refusing it");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert!(!status.success());
    assert!(err.contains("GPX"), "the refusal does not say why: {err}");
    assert!(!err.contains("ISO base media"), "{err}");
}
