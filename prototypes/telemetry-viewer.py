#!/usr/bin/env python3
"""
Serve a 360 video with its ride telemetry as a live HUD overlay, in the browser.

Renders the equirectangular video on a WebGL sphere (drag to look around) and
draws lean angle, speed and load from the telemetry CSV on top, sample-accurate
to the playhead.  Includes controls to nudge the data against the video, because
the two clocks do not always agree.

  ./telemetry-viewer.py GS010527_proxy.mp4 [--telemetry RIDE.telemetry.csv]

The GPMF timebase is corrected on the way in.  gopro-telemetry.py derives the
sample rate as samples/payloads = 30.000 Hz, but each GPMF payload actually
spans 1.001 s (the NTSC 1000/1001 factor -- the gpmd track's packet duration
says so outright), so its time_s column runs 0.1% fast: 0.6 s of drift by
t=600 s, 1.5 s over a full 26-minute chapter.  --rate overrides the corrected
29.97003 Hz if a clip disagrees.
"""
import argparse, csv, http.server, json, math, os, re, socketserver, subprocess, sys, threading, webbrowser

HERE = os.path.dirname(os.path.abspath(__file__))
VIEWER_HTML = os.path.join(HERE, 'viewer.html')

# CSV column -> (JSON key, decimal places).  Only what the HUD draws.
CHANNELS = [
    ('lean_deg',       'lean',     2),
    ('lean_rate_dps',  'leanrate', 1),
    ('pitch_deg',      'pitch',    2),
    ('pitch_rate_dps', 'pitchrate',1),
    ('yaw_rate_dps',   'yaw',      1),
    ('speed_kmh',      'speed',    1),
    ('accel_lon_g',    'alon',     3),
    ('accel_lat_g',    'alat',     3),
    ('accel_vert_g',   'avert',    3),
    ('accel_total_g',  'atot',     3),
    ('alt_m',          'alt',      1),
    ('course_deg',     'course',   1),
    ('lat',            'lat',      6),
    ('lon',            'lon',      6),
    ('imu_temp_c',     'temp',     1),
    ('gps_dop',        'dop',      2),
]

NTSC = 1000.0 / 1001.0


# ------------------------------------------------------------------ telemetry
def probe_payload_dur(path):
    """Real duration of one GPMF payload, from the gpmd track's packet timing.

    Returns None if the file has no gpmd track or ffprobe is unavailable.  The
    camera writes one payload per video second, which on an NTSC-rate clip is
    1.001 s, not 1 s.
    """
    try:
        streams = subprocess.run(
            ['ffprobe', '-v', 'error', '-show_entries',
             'stream=index,codec_tag_string', '-of', 'csv=p=0', path],
            capture_output=True, text=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError):
        return None
    idx = None
    for line in streams.splitlines():
        parts = line.split(',')
        if len(parts) >= 2 and parts[1].strip() == 'gpmd':
            idx = parts[0].strip()
            break
    if idx is None:
        return None
    try:
        pk = subprocess.run(
            ['ffprobe', '-v', 'error', '-select_streams', idx, '-read_intervals', '%+#4',
             '-show_entries', 'packet=duration_time', '-of', 'csv=p=0', path],
            capture_output=True, text=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError):
        return None
    for line in pk.splitlines():
        line = line.strip().rstrip(',')
        try:
            d = float(line)
        except ValueError:
            continue
        if d > 0:
            return d
    return None


def first_payload_dur(candidates):
    """Payload duration from the first candidate that actually carries a gpmd track.

    Existing-but-telemetry-free files (a transcoded proxy, say) must not stop the
    search, or the timebase silently falls back to the NTSC assumption.
    """
    for c in candidates:
        if not c or not os.path.exists(c):
            continue
        d = probe_payload_dur(c)
        if d:
            return d, c
    return None, None


def load_telemetry(path, rate=None, source=None):
    """Read the telemetry CSV into a columnar dict, on a corrected timebase."""
    with open(path, newline='') as fh:
        rows = list(csv.DictReader(fh))
    if not rows:
        sys.exit(f'{path}: no telemetry rows')
    have = rows[0].keys()
    missing = [c for c, _, _ in CHANNELS if c not in have]

    n = len(rows)
    # Nominal rate as gopro-telemetry.py wrote it: samples / whole seconds.
    t_last = float(rows[-1]['time_s'])
    nominal = (n - 1) / t_last if t_last else 30.0
    nominal = round(nominal)                      # 30 Hz, or 60 in the 5.3K modes

    if rate:
        hz, why = rate, 'given by --rate'
    else:
        pdur, src = first_payload_dur(source or [])
        if pdur:
            hz = nominal / pdur
            why = (f'{nominal} Hz / {pdur:.6f} s payload, measured from '
                   f'{os.path.basename(src)}')
        else:
            hz = nominal * NTSC
            why = f'{nominal} Hz x 1000/1001 (assumed NTSC; no gpmd track found)'

    ch = {}
    for col, key, dp in CHANNELS:
        if col not in have:
            continue
        out = []
        for r in rows:
            v = r[col]
            out.append(None if v in ('', None) else round(float(v), dp))
        if any(v is not None for v in out):
            ch[key] = out

    valid = [r.get('gps_valid') == '1' for r in rows]
    return {
        'n': n,
        'hz': hz,
        'hz_nominal': nominal,
        'hz_note': why,
        'dur_s': (n - 1) / hz,
        'start_utc': rows[0].get('utc', ''),
        'gps_fix_pct': round(100.0 * sum(valid) / n, 1),
        'gps_valid': [1 if v else 0 for v in valid],
        'ch': ch,
        'missing': missing,
    }


# --------------------------------------------------------------------- server
class Handler(http.server.BaseHTTPRequestHandler):
    server_version = 'gopro-telemetry-viewer'
    protocol_version = 'HTTP/1.1'     # keep-alive; seeking a 900 MB file needs it
    video = None
    payload = b''
    meta = {}

    def log_message(self, fmt, *args):
        pass                                      # the console is for ffmpeg-style notes

    def _headers(self, code, ctype, length, extra=None):
        self.send_response(code)
        self.send_header('Content-Type', ctype)
        self.send_header('Content-Length', str(length))
        self.send_header('Cache-Control', 'no-store')
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()

    def do_GET(self):
        self._serve(body=True)

    def do_HEAD(self):
        self._serve(body=False)

    def _serve(self, body):
        path = self.path.split('?', 1)[0]
        if path in ('/', '/index.html'):
            return self._file(VIEWER_HTML, 'text/html; charset=utf-8', body)
        if path == '/telemetry.json':
            self._headers(200, 'application/json', len(self.payload))
            if body:
                self.wfile.write(self.payload)
            return
        if path == '/video':
            return self._ranged(self.video, 'video/mp4', body)
        self.send_error(404)

    def _file(self, path, ctype, body):
        try:
            with open(path, 'rb') as fh:
                data = fh.read()
        except OSError:
            return self.send_error(404, f'missing {os.path.basename(path)}')
        self._headers(200, ctype, len(data))
        if body:
            self.wfile.write(data)

    def _ranged(self, path, ctype, body):
        """Serve a file honouring Range, so the browser can seek an 8 GB movie."""
        try:
            size = os.path.getsize(path)
        except OSError:
            return self.send_error(404)
        rng = self.headers.get('Range')
        start, end = 0, size - 1
        code, extra = 200, {'Accept-Ranges': 'bytes'}
        if rng:
            m = re.match(r'bytes=(\d*)-(\d*)\s*$', rng)
            if not m:
                self.send_response(416)
                self.send_header('Content-Range', f'bytes */{size}')
                self.send_header('Content-Length', '0')
                return self.end_headers()
            a, b = m.group(1), m.group(2)
            if a == '':                           # suffix range: last N bytes
                if b == '' or int(b) == 0:
                    self.send_response(416)
                    self.send_header('Content-Range', f'bytes */{size}')
                    self.send_header('Content-Length', '0')
                    return self.end_headers()
                start = max(0, size - int(b))
            else:
                start = int(a)
                if start >= size:
                    self.send_response(416)
                    self.send_header('Content-Range', f'bytes */{size}')
                    self.send_header('Content-Length', '0')
                    return self.end_headers()
                if b != '':
                    end = min(int(b), size - 1)
            code = 206
            extra['Content-Range'] = f'bytes {start}-{end}/{size}'
        length = end - start + 1
        self._headers(code, ctype, length, extra)
        if not body:
            return
        try:
            with open(path, 'rb') as fh:
                fh.seek(start)
                left = length
                while left > 0:
                    chunk = fh.read(min(1 << 20, left))
                    if not chunk:
                        break
                    self.wfile.write(chunk)
                    left -= len(chunk)
        except (BrokenPipeError, ConnectionResetError):
            # The player seeked away mid-body.  Normal, but the response is now
            # truncated, so this connection cannot be reused under keep-alive.
            self.close_connection = True


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


# ------------------------------------------------------------------------ main
def guess_csv(video):
    """Find the telemetry CSV for a video, allowing for _equirect/_proxy suffixes."""
    d, base = os.path.split(os.path.abspath(video))
    stem = os.path.splitext(base)[0]
    stems = [stem, re.sub(r'_(equirect|proxy|stitched|hud)$', '', stem)]
    for s in dict.fromkeys(stems):
        for cand in (f'{s}.telemetry.csv', f'{s}.csv'):
            p = os.path.join(d, cand)
            if os.path.exists(p):
                return p
    return None


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('video', help='equirectangular MP4 to play')
    ap.add_argument('-t', '--telemetry', help='telemetry CSV (default: alongside the video)')
    ap.add_argument('--rate', type=float,
                    help='telemetry sample rate in Hz (default: measured from the '
                         'GPMF payload duration, else nominal x 1000/1001)')
    ap.add_argument('--gpmf-source',
                    help='file whose gpmd track sets the timebase (default: the '
                         'video, then the .360 beside it)')
    ap.add_argument('-p', '--port', type=int, default=8360)
    ap.add_argument('--no-browser', action='store_true')
    args = ap.parse_args()

    if not os.path.exists(args.video):
        sys.exit(f'{args.video}: not found')
    csv_path = args.telemetry or guess_csv(args.video)
    if not csv_path:
        sys.exit('no telemetry CSV found; pass --telemetry, or run gopro-telemetry.py first')
    if not os.path.exists(csv_path):
        sys.exit(f'{csv_path}: not found')

    # The timebase comes from a gpmd track.  Prefer one the user named, then the
    # video itself (the stitch keeps the track), then the .360 next to the CSV.
    if args.gpmf_source:
        cands = [args.gpmf_source]
    else:
        stem = re.sub(r'_(equirect|proxy|stitched|hud)$', '',
                      os.path.splitext(os.path.abspath(args.video))[0])
        cstem = re.sub(r'\.telemetry$', '', os.path.splitext(os.path.abspath(csv_path))[0])
        cands = [args.video]
        for s in dict.fromkeys([stem, cstem]):
            cands += [s + ext for ext in ('.360', '.MP4', '.mp4', '_equirect.mp4')]

    data = load_telemetry(csv_path, rate=args.rate, source=cands)
    data['video'] = os.path.basename(args.video)
    data['clip'] = os.path.basename(csv_path)

    vdur = None
    try:
        vdur = float(subprocess.run(
            ['ffprobe', '-v', 'error', '-show_entries', 'format=duration',
             '-of', 'csv=p=0', args.video],
            capture_output=True, text=True, check=True).stdout.strip())
        data['video_dur_s'] = round(vdur, 3)
    except (OSError, subprocess.CalledProcessError, ValueError):
        pass

    Handler.video = os.path.abspath(args.video)
    Handler.payload = json.dumps(data, separators=(',', ':')).encode()
    Handler.meta = data

    size_mb = os.path.getsize(Handler.video) / 1e6
    print(f'{data["clip"]}  ({data["n"]} samples, {data["dur_s"]:.1f} s '
          f'@ {data["hz"]:.5f} Hz)')
    print(f'  timebase      {data["hz_note"]}')
    drift = data['n'] / data['hz_nominal'] - data['n'] / data['hz']
    print(f'  vs nominal    {data["hz_nominal"]} Hz would drift '
          f'{drift:+.2f} s over the clip')
    print(f'  gps fix       {data["gps_fix_pct"]}% of samples')
    if data['missing']:
        print(f'  absent        {", ".join(data["missing"])}')
    print(f'  video         {data["video"]}  ({size_mb:.0f} MB'
          + (f', {vdur:.1f} s)' if vdur else ')'))
    if vdur and vdur < data['dur_s'] - 1:
        print(f'  note          video covers only the first {vdur:.0f} s of '
              f'{data["dur_s"]:.0f} s of telemetry')
    print(f'  telemetry     {len(Handler.payload)/1e6:.1f} MB of JSON')

    url = f'http://127.0.0.1:{args.port}/'
    with Server(('127.0.0.1', args.port), Handler) as srv:
        print(f'  -> {url}   (ctrl-c to stop)')
        if not args.no_browser:
            threading.Timer(0.4, webbrowser.open, [url]).start()
        try:
            srv.serve_forever()
        except KeyboardInterrupt:
            print('\nstopped')


if __name__ == '__main__':
    main()
