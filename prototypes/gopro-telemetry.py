#!/usr/bin/env python3
"""
Extract ride telemetry (speed, lean angle, GPS track, IMU) from GoPro .360/.MP4 files.

Reads the embedded GPMF ("gpmd") metadata track and writes a CSV and/or a GPX
track carrying every derived channel.

  ./gopro-telemetry.py RIDE.360 [-o out.csv] [--gpx ride.gpx] [--gpx-hz N]

Lean angle comes from GoPro's GRAV sensor-fusion gravity vector, which tracks true
gravity (gyro-dominant) rather than the accelerometer's apparent vertical -- so it
stays correct through sustained corners. The camera mount orientation is calibrated
per clip from the data itself, so the mount need not be level or aligned.
"""
import argparse, datetime, math, os, struct, subprocess, sys, tempfile

G = 9.80665
GPS_EPOCH = datetime.datetime(2000, 1, 1, tzinfo=datetime.timezone.utc)
TYPES = {'b':('b',1),'B':('B',1),'s':('h',2),'S':('H',2),'l':('i',4),'L':('I',4),
         'f':('f',4),'d':('d',8),'j':('q',8),'J':('Q',8),'F':('4s',4)}
WANT = ('GPS9','GPS5','ACCL','GYRO','GRAV','CORI','IORI','MAGN','MNOR')
XMLNS_GT = 'https://github.com/gopro-tools/telemetry/1'


# ---------------------------------------------------------------- GPMF parsing
def _klv(buf, off, end):
    while off + 8 <= end:
        fourcc = buf[off:off+4].decode('latin1')
        t = buf[off+4:off+5].decode('latin1')
        ssize = buf[off+5]
        repeat = struct.unpack('>H', buf[off+6:off+8])[0]
        dlen = ssize * repeat
        yield fourcc, t, ssize, repeat, off+8, dlen
        off += 8 + ((dlen + 3)//4)*4


def _nums(buf, t, ssize, repeat, dstart, typestr=None):
    if t == '?' and typestr:
        fmt = ''.join(TYPES[c][0] for c in typestr if c in TYPES)
        if not fmt:
            return []
        return [struct.unpack('>'+fmt, buf[dstart+r*ssize:dstart+(r+1)*ssize])
                for r in range(repeat)]
    if t not in TYPES:
        return []
    f, sz = TYPES[t]
    n = ssize // sz
    if n == 0:
        return []
    return [struct.unpack('>'+f*n, buf[dstart+r*ssize:dstart+(r+1)*ssize])
            for r in range(repeat)]


def parse_gpmf(raw):
    """Return ({key: {'stnm','orin','scal','samples','tmpc'}}, n_payloads)."""
    streams, npay, off = {}, 0, 0
    while off + 8 <= len(raw):
        fourcc, t, ss, rp, ds, dl = next(_klv(raw, off, len(raw)))
        if fourcc != 'DEVC':
            break
        for f2, t2, ss2, rp2, ds2, dl2 in _klv(raw, ds, ds+dl):
            if f2 != 'STRM':
                continue
            scal = typestr = stnm = orin = tmpc = None
            data = None
            for f3, t3, ss3, rp3, ds3, dl3 in _klv(raw, ds2, ds2+dl2):
                if f3 == 'SCAL':
                    scal = [v[0] for v in _nums(raw, t3, ss3, rp3, ds3)]
                elif f3 == 'TYPE':
                    typestr = raw[ds3:ds3+dl3].decode('latin1').rstrip('\x00')
                elif f3 == 'STNM':
                    stnm = raw[ds3:ds3+dl3].decode('latin1').rstrip('\x00')
                elif f3 == 'ORIN':
                    orin = raw[ds3:ds3+dl3].decode('latin1').rstrip('\x00')
                elif f3 == 'TMPC':
                    v = _nums(raw, t3, ss3, rp3, ds3)
                    tmpc = v[0][0] if v else None
                elif f3 in WANT:
                    data = (f3, _nums(raw, t3, ss3, rp3, ds3, typestr))
            if data and data[1]:
                st = streams.setdefault(data[0], {'stnm': stnm, 'orin': orin,
                                                  'scal': scal, 'samples': [],
                                                  'tmpc': []})
                st['scal'] = scal or st['scal']
                st['samples'].extend(data[1])
                if tmpc is not None:
                    st['tmpc'].append(tmpc)
        npay += 1
        off = ds + ((dl + 3)//4)*4
    return streams, npay


def scaled(st):
    sc = st['scal'] or [1]
    out = []
    for s in st['samples']:
        if len(sc) == 1:
            out.append(tuple(v/sc[0] for v in s))
        else:
            out.append(tuple(v/(sc[i] if i < len(sc) else sc[-1])
                             for i, v in enumerate(s)))
    return out


def reorient(samples, orin):
    """Reorder IMU triplets to X,Y,Z per the ORIN tag (lowercase axis = negated)."""
    if not orin or len(orin) < 3:
        return samples
    idx, sg = {}, {}
    for i, c in enumerate(orin[:3]):
        a = c.upper()
        idx[a] = i
        sg[a] = -1.0 if c.islower() else 1.0
    return [tuple(sg[a]*s[idx[a]] for a in 'XYZ') for s in samples]


def to_grav_frame(triples, orin):
    """Put an ACCL/GYRO triplet stream into the same axes GRAV uses.

    ACCL and GYRO are tagged ORIN=ZXY; GRAV carries no ORIN and has its Y and Z
    swapped relative to them. Verified on a stationary clip, where the result is
    parallel to GRAV at 9.800 m/s^2.
    """
    xyz = reorient(triples, orin)
    return [(v[0], v[2], v[1]) for v in xyz]


# ------------------------------------------------------------------ extraction
def extract_gpmd(path):
    """Demux the gpmd track to bytes (stream-copied, not decoded)."""
    q = subprocess.run(['ffprobe','-v','error','-select_streams','d',
                        '-show_entries','stream=index,codec_tag_string',
                        '-of','csv=p=0', path], capture_output=True, text=True)
    if q.returncode != 0:
        sys.exit(f"ffprobe failed on {path}:\n{q.stderr.strip()}")
    idx = None
    for line in q.stdout.splitlines():
        parts = line.split(',')
        if len(parts) >= 2 and parts[1].strip() == 'gpmd':
            idx = parts[0].strip()
            break
    if idx is None:
        sys.exit(f"no GPMF (gpmd) telemetry track found in {path}")
    with tempfile.NamedTemporaryFile(suffix='.gpmd', delete=False) as tf:
        tmp = tf.name
    try:
        r = subprocess.run(['ffmpeg','-v','error','-y','-i',path,
                            '-map',f'0:{idx}','-c','copy','-f','data',tmp],
                           capture_output=True, text=True)
        if r.returncode != 0:
            sys.exit(f"ffmpeg failed extracting telemetry:\n{r.stderr.strip()}")
        return open(tmp,'rb').read()
    finally:
        os.unlink(tmp)


def container_start_time(path):
    """Recording start as a UTC datetime, for clips whose GPS never got a fix."""
    r = subprocess.run(['ffprobe','-v','error','-show_entries',
                        'format_tags=creation_time','-of',
                        'default=nw=1:nk=1', path], capture_output=True, text=True)
    s = r.stdout.strip()
    if not s:
        return None
    try:
        return datetime.datetime.fromisoformat(s.replace('Z','+00:00'))
    except ValueError:
        return None


# ----------------------------------------------------------------- math helpers
def _norm(v):
    m = math.sqrt(sum(x*x for x in v)) or 1.0
    return [x/m for x in v]


def _dot(a, b):
    return sum(x*y for x, y in zip(a, b))


def _cross(a, b):
    return [a[1]*b[2]-a[2]*b[1], a[2]*b[0]-a[0]*b[2], a[0]*b[1]-a[1]*b[0]]


def _movavg(x, w):
    if w < 2:
        return list(x)
    n = len(x)
    h = w//2
    c = [0.0]
    for v in x:
        c.append(c[-1]+v)
    return [(c[min(n,i+h+1)]-c[max(0,i-h)])/(min(n,i+h+1)-max(0,i-h))
            for i in range(n)]


def _corr_sign(a, b):
    """+1 if the two series move together, -1 if they oppose."""
    return -1.0 if sum(x*y for x, y in zip(a, b)) < 0 else 1.0


def calibrate_mount(grav, upright_idx=None):
    """Find the upright reference and lateral axis in the camera frame.

    The upright reference is the mean gravity direction over `upright_idx`
    (straight-line riding, where the bike really is upright). Falling back to
    the whole clip works because left/right lean largely cancels, but it leaves
    a bias on rides that turn mostly one way.

    Lean sweeps gravity along one direction; the principal component of the
    deviations from upright recovers that lateral axis (power iteration).
    """
    sub = [grav[i] for i in upright_idx] if upright_idx else grav
    ref = _norm([sum(v[i] for v in sub)/len(sub) for i in range(3)])
    dev = [[v[i]-ref[i] for i in range(3)] for v in grav]
    C = [[sum(d[i]*d[j] for d in dev)/len(dev) for j in range(3)]
         for i in range(3)]
    vec = [1.0, 0.0, 0.0]
    for _ in range(300):
        nv = [sum(C[i][j]*vec[j] for j in range(3)) for i in range(3)]
        if not any(nv):
            break
        vec = _norm(nv)
    lat = _norm([vec[i]-ref[i]*_dot(vec, ref) for i in range(3)])
    return ref, lat


# ---------------------------------------------------------------------- compute
def compute(streams, npay):
    """Derive every per-sample channel, on the GRAV timebase."""
    ch = {}
    grav = [_norm(v) for v in scaled(streams['GRAV'])]   # unit, points UP
    n = len(grav)
    hz_g = n/npay
    ch['n'], ch['hz'] = n, hz_g

    gkey = 'GPS9' if 'GPS9' in streams else 'GPS5'
    gps = scaled(streams[gkey])
    hz_p = len(gps)/npay

    # When the GPS loses lock the camera keeps emitting samples with a DOP
    # sentinel of 99.99 and the last-known values frozen in place. Those must be
    # masked or they corrupt every derived figure.
    if gkey == 'GPS9':
        ok = [r[7] < 99.98 and not (r[0] == 0.0 and r[1] == 0.0) for r in gps]
    else:
        ok = [not (r[0] == 0.0 and r[1] == 0.0) for r in gps]

    def gi(i):
        return min(len(gps)-1, int(i*hz_p/hz_g))

    def speed_at(i):
        j = gi(i)
        return gps[j][3] if ok[j] else None

    # ---- IMU, resampled onto the GRAV timebase
    def resample(src, hz_src, half_s):
        """Average onto the GRAV timebase over a +/- half_s second window.

        The window has to be a real duration, not a few samples: a bike over
        tarmac puts multi-g vibration through the accelerometer, so a short
        window reports road buzz rather than the load on the machine.
        """
        w = max(1, int(hz_src*half_s))
        out = []
        for i in range(n):
            j = int(i*hz_src/hz_g)
            lo, hi = max(0, j-w), min(len(src), j+w)
            if hi <= lo:
                out.append((0.0, 0.0, 0.0))
                continue
            out.append(tuple(sum(s[k] for s in src[lo:hi])/(hi-lo)
                             for k in range(3)))
        return out

    gyro = acc = None
    if 'GYRO' in streams:
        g_raw = to_grav_frame([g[:3] for g in scaled(streams['GYRO'])],
                              streams['GYRO']['orin'])
        gyro = resample(g_raw, len(g_raw)/npay, 0.02)
    if 'ACCL' in streams:
        a_raw = to_grav_frame([a[:3] for a in scaled(streams['ACCL'])],
                              streams['ACCL']['orin'])
        acc = resample(a_raw, len(a_raw)/npay, 0.2)

    def yaw_series(axis_up):
        if gyro is None:
            return [0.0]*n
        return _movavg([_dot(gyro[i], axis_up[i]) for i in range(n)], 15)

    # ---- two-pass mount calibration
    up_ref, lat_ax = calibrate_mount(grav)
    yaw = yaw_series(grav)
    straight = [i for i in range(n)
                if (speed_at(i) or 0) > 5.0
                and abs(math.degrees(yaw[i])) < 2.0]
    if len(straight) > max(30, 0.02*n):
        up_ref, lat_ax = calibrate_mount(grav, straight)
        yaw = yaw_series(grav)
    else:
        straight = None
    fwd = _norm(_cross(lat_ax, up_ref))

    # ---- lean, signed so positive = right-hand turn
    lean = _movavg([math.degrees(math.atan2(_dot(v, lat_ax), _dot(v, up_ref)))
                    for v in grav], 5)
    fast = [i for i in range(n) if (speed_at(i) or 0) > 6.0]
    if fast and sum(lean[i]*yaw[i] for i in fast) < 0:
        lean = [-x for x in lean]
        lat_ax = [-x for x in lat_ax]
        fwd = _norm(_cross(lat_ax, up_ref))

    # ---- forward axis polarity: longitudinal accel must agree with dv/dt
    spd_g = [speed_at(i) for i in range(n)]
    dv = [0.0]*n
    step = max(1, int(hz_g/2))
    for i in range(n):
        a, b = max(0, i-step), min(n-1, i+step)
        if spd_g[a] is not None and spd_g[b] is not None:
            dv[i] = (spd_g[b]-spd_g[a])/((b-a)/hz_g)
    if acc is not None:
        def lon_of(f):
            out = []
            for i in range(n):
                fh = _norm([f[k]-grav[i][k]*_dot(f, grav[i]) for k in range(3)])
                ah = [acc[i][k]-grav[i][k]*_dot(acc[i], grav[i]) for k in range(3)]
                out.append(_dot(ah, fh))
            return out
        if _corr_sign(lon_of(fwd), dv) < 0:
            fwd = [-x for x in fwd]

    # ---- body rates and road-frame accelerations
    roll_rate = pitch_rate = None
    if gyro is not None:
        roll_rate = [math.degrees(_dot(gyro[i], fwd)) for i in range(n)]
        dlean = [0.0]*n
        for i in range(1, n-1):
            dlean[i] = (lean[i+1]-lean[i-1])*hz_g/2
        if _corr_sign(roll_rate, dlean) < 0:
            roll_rate = [-x for x in roll_rate]
        pitch_rate = [math.degrees(_dot(gyro[i], lat_ax)) for i in range(n)]

    a_lat = a_lon = a_vert = a_tot = None
    if acc is not None:
        a_lat, a_lon, a_vert, a_tot = [], [], [], []
        for i in range(n):
            up = grav[i]
            av = _dot(acc[i], up)
            ah = [acc[i][k]-up[k]*av for k in range(3)]
            fh = _norm([fwd[k]-up[k]*_dot(fwd, up) for k in range(3)])
            left = _cross(up, fh)
            a_lon.append(_dot(ah, fh)/G)
            a_lat.append(-_dot(ah, left)/G)     # positive = rightward
            a_vert.append((av-G)/G)
            a_tot.append(math.sqrt(_dot(acc[i], acc[i]))/G)
        if _corr_sign(a_lat, lean) < 0:
            a_lat = [-x for x in a_lat]

    # ---- pitch (bike nose-up angle, relative to the straight-line reference)
    pitch = _movavg([math.degrees(math.asin(max(-1.0, min(1.0, _dot(fwd, v)))))
                     for v in grav], 15)

    # ---- GPS-derived course over ground
    course = [None]*n
    vi = [i for i, v in enumerate(ok) if v]
    if len(vi) > 2:
        hd = {}
        for a, b in zip(vi, vi[1:]):
            if b - a > 5:
                continue
            dlat = math.radians(gps[b][0]-gps[a][0])
            dlon = math.radians(gps[b][1]-gps[a][1])*math.cos(
                math.radians(gps[b][0]))
            if abs(dlat) < 1e-9 and abs(dlon) < 1e-9:
                continue
            hd[b] = math.degrees(math.atan2(dlon, dlat)) % 360.0
        for i in range(n):
            course[i] = hd.get(gi(i))

    # ---- IMU die temperature (one value per payload, i.e. per second)
    temp = None
    tm = (streams.get('GYRO') or {}).get('tmpc') or \
         (streams.get('ACCL') or {}).get('tmpc')
    if tm:
        temp = [tm[min(len(tm)-1, int(i/hz_g))] for i in range(n)]

    # ---- absolute UTC time per sample
    t0 = None
    if gkey == 'GPS9':
        for i, r in enumerate(gps):
            if ok[i]:
                t0 = (GPS_EPOCH + datetime.timedelta(days=r[5], seconds=r[6])
                      - datetime.timedelta(seconds=i/hz_p))
                break
    ch.update(gps=gps, ok=ok, gi=gi, hz_p=hz_p, gkey=gkey, t0=t0,
              lean=lean, pitch=pitch, yaw=[math.degrees(y) for y in yaw],
              roll_rate=roll_rate, pitch_rate=pitch_rate, course=course,
              a_lat=a_lat, a_lon=a_lon, a_vert=a_vert, a_tot=a_tot,
              temp=temp, straight=straight, grav=grav, vi=vi)
    return ch


# ----------------------------------------------------------------------- output
def _f(v, nd=2):
    return '' if v is None else f'{v:.{nd}f}'


COLS = [
    ('time_s',        lambda c, i, j: f'{i/c["hz"]:.3f}'),
    ('utc',           lambda c, i, j: c['utc'](i) or ''),
    ('lat',           lambda c, i, j: _f(c['gps'][j][0], 7) if c['ok'][j] else ''),
    ('lon',           lambda c, i, j: _f(c['gps'][j][1], 7) if c['ok'][j] else ''),
    ('alt_m',         lambda c, i, j: _f(c['gps'][j][2]) if c['ok'][j] else ''),
    ('speed_kmh',     lambda c, i, j: _f(c['gps'][j][3]*3.6) if c['ok'][j] else ''),
    ('speed_3d_kmh',  lambda c, i, j: _f(c['gps'][j][4]*3.6) if c['ok'][j] else ''),
    ('course_deg',    lambda c, i, j: _f(c['course'][i], 1)),
    ('lean_deg',      lambda c, i, j: _f(c['lean'][i])),
    ('lean_rate_dps', lambda c, i, j: _f(c['roll_rate'][i], 1) if c['roll_rate'] else ''),
    ('pitch_deg',     lambda c, i, j: _f(c['pitch'][i])),
    ('pitch_rate_dps',lambda c, i, j: _f(c['pitch_rate'][i], 1) if c['pitch_rate'] else ''),
    ('yaw_rate_dps',  lambda c, i, j: _f(c['yaw'][i], 1)),
    ('accel_lon_g',   lambda c, i, j: _f(c['a_lon'][i], 3) if c['a_lon'] else ''),
    ('accel_lat_g',   lambda c, i, j: _f(c['a_lat'][i], 3) if c['a_lat'] else ''),
    ('accel_vert_g',  lambda c, i, j: _f(c['a_vert'][i], 3) if c['a_vert'] else ''),
    ('accel_total_g', lambda c, i, j: _f(c['a_tot'][i], 3) if c['a_tot'] else ''),
    ('gps_dop',       lambda c, i, j: _f(c['gps'][j][7]) if c['ok'][j] and c['gkey'] == 'GPS9' else ''),
    ('imu_temp_c',    lambda c, i, j: _f(c['temp'][i], 1) if c['temp'] else ''),
    ('gps_valid',     lambda c, i, j: str(int(c['ok'][j]))),
]


def write_csv(path, c):
    with open(path, 'w') as f:
        f.write(','.join(name for name, _ in COLS) + '\n')
        for i in range(c['n']):
            j = c['gi'](i)
            f.write(','.join(fn(c, i, j) for _, fn in COLS) + '\n')


def _esc(s):
    return (s.replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;'))


def write_gpx(path, c, name, rate=None, activity='motorcycling'):
    """GPX 1.1 track. Every channel rides along in <extensions>.

    Points are emitted only where the GPS had a fix, since a <trkpt> without a
    position is meaningless. Default cadence is the GPS rate, because positions
    simply repeat at the faster GRAV rate.
    """
    step = 1
    if rate:
        step = max(1, int(round(c['hz']/rate)))
    else:
        step = max(1, int(round(c['hz']/c['hz_p'])))

    ext = [
        ('lean_deg',       'lean',       lambda i, j: _f(c['lean'][i])),
        ('lean_rate_dps',  'roll_rate',  lambda i, j: _f(c['roll_rate'][i], 1) if c['roll_rate'] else ''),
        ('pitch_deg',      'pitch',      lambda i, j: _f(c['pitch'][i])),
        ('pitch_rate_dps', 'pitch_rate', lambda i, j: _f(c['pitch_rate'][i], 1) if c['pitch_rate'] else ''),
        ('yaw_rate_dps',   'yaw',        lambda i, j: _f(c['yaw'][i], 1)),
        ('accel_lon_g',    'a_lon',      lambda i, j: _f(c['a_lon'][i], 3) if c['a_lon'] else ''),
        ('accel_lat_g',    'a_lat',      lambda i, j: _f(c['a_lat'][i], 3) if c['a_lat'] else ''),
        ('accel_vert_g',   'a_vert',     lambda i, j: _f(c['a_vert'][i], 3) if c['a_vert'] else ''),
        ('accel_total_g',  'a_tot',      lambda i, j: _f(c['a_tot'][i], 3) if c['a_tot'] else ''),
        ('speed_3d_ms',    None,         lambda i, j: _f(c['gps'][j][4], 3)),
        ('imu_temp_c',     'temp',       lambda i, j: _f(c['temp'][i], 1) if c['temp'] else ''),
        ('video_time_s',   None,         lambda i, j: f'{i/c["hz"]:.3f}'),
    ]

    npt = 0
    with open(path, 'w') as f:
        f.write('<?xml version="1.0" encoding="UTF-8"?>\n'
                '<gpx version="1.1" creator="gopro-telemetry.py"\n'
                '     xmlns="http://www.topografix.com/GPX/1/1"\n'
                '     xmlns:gpxtpx="http://www.garmin.com/xmlschemas/'
                'TrackPointExtension/v1"\n'
                f'     xmlns:gt="{XMLNS_GT}">\n')
        f.write(f'<metadata><name>{_esc(name)}</name>')
        if c['t0']:
            f.write(f'<time>{c["t0"].strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3]}Z</time>')
        f.write('</metadata>\n')
        f.write(f'<trk><name>{_esc(name)}</name>'
                f'<type>{_esc(activity)}</type><trkseg>\n')
        for i in range(0, c['n'], step):
            j = c['gi'](i)
            if not c['ok'][j]:
                continue
            r = c['gps'][j]
            f.write(f'<trkpt lat="{r[0]:.7f}" lon="{r[1]:.7f}">'
                    f'<ele>{r[2]:.2f}</ele>')
            t = c['utc'](i)
            if t:
                f.write(f'<time>{t}</time>')
            if c['gkey'] == 'GPS9':
                f.write(f'<hdop>{r[7]:.2f}</hdop>')
            f.write('<extensions>')
            f.write('<gpxtpx:TrackPointExtension>'
                    f'<gpxtpx:speed>{r[3]:.3f}</gpxtpx:speed>')
            if c['course'][i] is not None:
                f.write(f'<gpxtpx:course>{c["course"][i]:.1f}</gpxtpx:course>')
            f.write('</gpxtpx:TrackPointExtension>')
            for tag, guard, fn in ext:
                if guard is not None and not c.get(guard):
                    continue
                v = fn(i, j)
                if v != '':
                    f.write(f'<gt:{tag}>{v}</gt:{tag}>')
            f.write('</extensions></trkpt>\n')
            npt += 1
        f.write('</trkseg></trk></gpx>\n')
    return npt, c['hz']/step


# ------------------------------------------------------------------------ main
def main():
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('video')
    ap.add_argument('-o','--out',
                    help='CSV output path (default: <video>.telemetry.csv)')
    ap.add_argument('--no-csv', action='store_true', help='skip the CSV')
    ap.add_argument('--gpx', nargs='?', const='', default=None,
                    help='write a GPX track (default path: <video>.gpx)')
    ap.add_argument('--gpx-hz', type=float, default=None,
                    help='GPX sample rate (default: the GPS rate, ~10 Hz)')
    ap.add_argument('--activity', default='motorcycling',
                    help='GPX <type> for the track (default: motorcycling)')
    args = ap.parse_args()

    streams, npay = parse_gpmf(extract_gpmd(args.video))
    if 'GRAV' not in streams:
        sys.exit("clip has no GRAV stream; cannot derive lean angle")
    if not any(k in streams for k in ('GPS9','GPS5')):
        sys.exit("clip has no GPS stream")

    c = compute(streams, npay)
    base = os.path.splitext(args.video)[0]

    # Absolute timestamps: GPS time where there was ever a fix, otherwise the
    # container's creation_time, which is the recording start.
    t0 = c['t0'] or container_start_time(args.video)
    c['t0'] = t0
    if t0:
        def utc(i):
            t = t0 + datetime.timedelta(seconds=i/c['hz'])
            return t.astimezone(datetime.timezone.utc).strftime(
                '%Y-%m-%dT%H:%M:%S.%f')[:-3] + 'Z'
    else:
        def utc(i):
            return None
    c['utc'] = utc

    if not args.no_csv:
        out = args.out or (base + '.telemetry.csv')
        write_csv(out, c)

    gpx_path = None
    if args.gpx is not None:
        gpx_path = args.gpx or (base + '.gpx')

    ok, gps, vi = c['ok'], c['gps'], c['vi']
    lean = c['lean']
    spd = [r[3] for r in gps]

    dist = 0.0
    for a, b in zip(vi, vi[1:]):
        if b - a > 5:               # don't bridge a gap in coverage
            continue
        dlat = math.radians(gps[b][0]-gps[a][0])
        dlon = math.radians(gps[b][1]-gps[a][1])*math.cos(
            math.radians(gps[b][0]))
        dist += 6371000.0*math.sqrt(dlat*dlat + dlon*dlon)

    # GPS altitude is too noisy to differentiate directly; smooth over ~5 s and
    # ignore sub-metre wiggles, else noise accumulates into a fake climb figure.
    # Smoothing and accumulation must both stop at a coverage gap: the altitude
    # either side of a drop-out is unrelated, and bridging one clip's gaps
    # produced 2850 m of "climb" inside a 458 m band.
    runs, cur = [], []
    for a, b in zip(vi, vi[1:] + [None]):
        cur.append(a)
        if b is None or b - a > 5:
            runs.append(cur)
            cur = []
    gain = 0.0
    for run in runs:
        if len(run) < 3:
            continue
        alt_s = _movavg([gps[i][2] for i in run], max(3, int(c['hz_p']*5)))
        ref = alt_s[0]
        for a in alt_s[1:]:
            if a - ref > 1.0:
                gain += a - ref
                ref = a
            elif a < ref:
                ref = a

    print(f"{os.path.basename(args.video)}  ({npay} s, {c['n']} samples "
          f"@ {c['hz']:.0f} Hz)")
    if t0:
        print(f"  start         {t0.astimezone(datetime.timezone.utc):%Y-%m-%d %H:%M:%S} UTC"
              + ("" if c['gkey'] == 'GPS9' and c['t0'] else " (from container)"))
    mov = [spd[i] for i in vi if spd[i] > 1.0]
    if vi:
        av = [gps[i][2] for i in vi]
        print(f"  gps fix       {100.0*len(vi)/max(1,len(ok)):.0f}% of clip "
              f"({len(vi)}/{len(ok)} samples)")
        print(f"  distance      {dist/1000:.2f} km   climb {gain:.0f} m   "
              f"alt {min(av):.0f}-{max(av):.0f} m")
        print(f"  speed         max {max(spd[i] for i in vi)*3.6:.1f} km/h   "
              f"avg moving {(sum(mov)/len(mov)*3.6) if mov else 0:.1f} km/h")
    else:
        print("  gps fix       none in this clip -- lean/IMU still valid")
    print(f"  lean          max left {min(lean):.1f}°   max right {max(lean):.1f}°"
          + ("" if c['straight'] else "   (upright ref from whole clip)"))
    if c['a_lat']:
        def pct(xs, q):
            v = sorted(xs)
            return v[min(len(v)-1, max(0, int(q*len(v))))]
        print(f"  load (p99.5)  lat {pct([abs(x) for x in c['a_lat']], 0.995):.2f} g   "
              f"brake {pct(c['a_lon'], 0.005):.2f} g   "
              f"drive {pct(c['a_lon'], 0.995):.2f} g")
    print("  streams       " + ', '.join(
        f"{k}@{len(streams[k]['samples'])/npay:.0f}Hz" for k in streams))
    if not args.no_csv:
        print(f"  -> {out}")
    if gpx_path:
        npt, rate = write_gpx(gpx_path, c, os.path.basename(base),
                              args.gpx_hz, args.activity)
        print(f"  -> {gpx_path}  ({npt} trkpt @ {rate:.0f} Hz)")
        if npt == 0:
            print("     warning: no GPS fix anywhere in this clip, so the GPX "
                  "has no points.\n"
                  "     Lean and IMU data are still valid -- use the CSV.")


if __name__ == '__main__':
    main()
