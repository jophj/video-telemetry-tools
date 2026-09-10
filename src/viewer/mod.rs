//! A local web server for the telemetry viewer.
//!
//! Three routes: the page itself, the telemetry as JSON, and the video with
//! byte-range support. The last is the one that matters — the browser will not
//! scrub a video it cannot seek within, and these files run to hundreds of
//! megabytes, so serving them whole is not an option.
//!
//! Deliberately synchronous, on `tiny_http`. One subcommand wanting a socket is
//! not a reason to pull an async runtime into a tool that is otherwise a
//! straight-line pipeline.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use tiny_http::{Header, Request, Response, Server, StatusCode};

/// The viewer page, compiled in so that a single binary is the whole tool.
const VIEWER_HTML: &str = include_str!("../../assets/viewer.html");

/// Serving a range larger than this in one response is pointless: the browser
/// asks for what it needs, and a whole-file response defeats seeking.
const MAX_CHUNK: u64 = 1 << 20;

pub struct Options {
    pub video: PathBuf,
    /// The telemetry JSON to serve, already rendered.
    pub telemetry_json: String,
    pub port: u16,
    pub open_browser: bool,
}

pub fn serve(opts: Options) -> anyhow::Result<()> {
    // Bound to localhost only. This serves a file off the user's disk; it has
    // no business being reachable from the network.
    let address = format!("127.0.0.1:{}", opts.port);
    let server = Server::http(&address).map_err(|e| {
        anyhow::anyhow!("could not listen on {address}: {e}. Try --port with another number.")
    })?;

    let url = format!("http://{address}/");
    println!("viewer at {url}");
    println!("serving {}", opts.video.display());
    println!("press ctrl-c to stop");

    if opts.open_browser {
        open_in_browser(&url);
    }

    let video_size = std::fs::metadata(&opts.video)
        .map(|m| m.len())
        .unwrap_or(0);
    let mime = mime_for(&opts.video);

    for request in server.incoming_requests() {
        // A dropped connection mid-scrub is normal, not an error worth
        // reporting: the browser abandons ranges it no longer needs.
        let _ = match request.url().split('?').next().unwrap_or("/") {
            "/" | "/index.html" => respond_html(request, VIEWER_HTML),
            "/telemetry.json" => respond_json(request, &opts.telemetry_json),
            "/video" => respond_video(request, &opts.video, video_size, mime),
            _ => respond_not_found(request),
        };
    }
    Ok(())
}

fn header(name: &str, value: &str) -> Header {
    // Both halves are literals from this file, so this cannot fail in practice.
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .expect("static header is well formed")
}

fn respond_html(request: Request, body: &str) -> std::io::Result<()> {
    request.respond(
        Response::from_string(body)
            .with_header(header("Content-Type", "text/html; charset=utf-8"))
            // The page is compiled in, so a stale cached copy would be
            // confusing after an upgrade.
            .with_header(header("Cache-Control", "no-store")),
    )
}

fn respond_json(request: Request, body: &str) -> std::io::Result<()> {
    request.respond(
        Response::from_string(body)
            .with_header(header("Content-Type", "application/json"))
            .with_header(header("Cache-Control", "no-store")),
    )
}

fn respond_not_found(request: Request) -> std::io::Result<()> {
    request.respond(Response::from_string("not found").with_status_code(StatusCode(404)))
}

/// Serve the video, honouring a `Range` request.
///
/// Without range support a browser will play from the start and refuse to seek,
/// which makes the viewer useless for finding a moment. The response is capped
/// at a megabyte per request so that a seek to the end of an 800 MB file does
/// not stream everything after it.
fn respond_video(
    request: Request,
    path: &Path,
    size: u64,
    mime: &str,
) -> std::io::Result<()> {
    let range = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .map(|h| h.value.as_str().to_string());

    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => {
            return request
                .respond(Response::from_string("video not found").with_status_code(StatusCode(404)))
        }
    };

    // Built as a list rather than applied with `with_header`, because the two
    // response bodies below have different reader types and a closure would be
    // monomorphised to whichever came first.
    let base = || {
        vec![
            header("Content-Type", mime),
            // Announcing this is what makes a browser willing to seek at all.
            header("Accept-Ranges", "bytes"),
        ]
    };

    let Some(spec) = range else {
        // No range asked for: send the whole thing.
        return request.respond(Response::new(
            StatusCode(200),
            base(),
            file,
            Some(size as usize),
            None,
        ));
    };

    let Some((start, end)) = parse_range(&spec, size) else {
        // A range that cannot be satisfied gets 416 and the actual size, which
        // is how the client learns what it may ask for.
        return request.respond(
            Response::from_string("range not satisfiable")
                .with_status_code(StatusCode(416))
                .with_header(header("Content-Range", &format!("bytes */{size}"))),
        );
    };

    let length = end - start + 1;
    if file.seek(SeekFrom::Start(start)).is_err() {
        return request
            .respond(Response::from_string("seek failed").with_status_code(StatusCode(500)));
    }
    let mut headers = base();
    headers.push(header(
        "Content-Range",
        &format!("bytes {start}-{end}/{size}"),
    ));
    request.respond(Response::new(
        StatusCode(206),
        headers,
        file.take(length),
        Some(length as usize),
        None,
    ))
}

/// Parse a single byte range, returning inclusive bounds.
///
/// Handles the three forms a browser actually sends: `bytes=0-1023`,
/// `bytes=1024-` for "from here on", and `bytes=-1024` for a suffix, which is
/// what a player uses to find the `moov` atom at the end of a file.
pub fn parse_range(spec: &str, size: u64) -> Option<(u64, u64)> {
    if size == 0 {
        return None;
    }
    let spec = spec.trim();
    let rest = spec.strip_prefix("bytes=")?.trim();
    // A multipart range request is legal but no browser sends one for video.
    if rest.contains(',') {
        return None;
    }
    let (from, to) = rest.split_once('-')?;
    let (from, to) = (from.trim(), to.trim());

    let (start, end) = match (from.is_empty(), to.is_empty()) {
        // "bytes=-N": the last N bytes.
        (true, false) => {
            let n: u64 = to.parse().ok()?;
            if n == 0 {
                return None;
            }
            (size.saturating_sub(n), size - 1)
        }
        // "bytes=N-": from N to the end.
        (false, true) => (from.parse().ok()?, size - 1),
        // "bytes=N-M".
        (false, false) => (from.parse().ok()?, to.parse::<u64>().ok()?.min(size - 1)),
        (true, true) => return None,
    };

    if start > end || start >= size {
        return None;
    }
    // Cap the response so a seek near the end of a large file does not stream
    // the whole remainder.
    Some((start, end.min(start + MAX_CHUNK - 1)))
}

fn mime_for(path: &Path) -> &'static str {
    match path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .as_deref()
    {
        Some("mp4") | Some("360") | Some("lrv") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        _ => "application/octet-stream",
    }
}

/// Open a URL in the default browser, best effort.
fn open_in_browser(url: &str) {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(windows) {
        // `start` is a shell builtin, so it needs cmd, and the empty string is
        // the window title `start` would otherwise take the URL for.
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    // Failing to open a browser is not a reason to fail; the URL is printed.
    let _ = std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
