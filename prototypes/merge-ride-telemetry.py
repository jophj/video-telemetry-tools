#!/usr/bin/env python3
"""
Merge GoPro telemetry CSV into a MotionRecord/Kurvenfokus ride JSON, as one GPX.

The JSON is the source: it decides the track points, their positions, times and
elevations. The CSV (from gopro-telemetry.py) is merged onto those points by
absolute UTC time -- nothing from either side is dropped or overwritten. Where
both files measure the same quantity, both values are written under their own
field name: the phone's under `mr:`, the camera's under `gt:`.

  ./merge-ride-telemetry.py RIDE.json RIDE.telemetry.csv [-o merged.gpx]
  ./merge-ride-telemetry.py RIDE.json RIDE.telemetry.csv --csv-mode mean
  ./merge-ride-telemetry.py RIDE.json RIDE.telemetry.csv --time-offset -4.5

The two devices keep their own clocks, so the shift that best lines the camera's
GPS track up with the phone's is estimated from the tracks themselves before
matching. `--time-offset` sets it by hand, `--no-auto-offset` assumes zero.
"""
import argparse, bisect, csv, datetime, json, math, os, statistics, sys

XMLNS_GT = 'https://github.com/gopro-tools/telemetry/1'   # GoPro CSV channels
XMLNS_MR = 'https://github.com/gopro-tools/motionrecord/1'  # phone JSON channels
XMLNS_TPX = 'http://www.garmin.com/xmlschemas/TrackPointExtension/v1'

# CSV columns that are angles in degrees -- averaged circularly, never linearly.
ANGLE_COLS = ('course_deg',)
# CSV columns that are not measurements to average.
PASSTHROUGH_COLS = ('utc', 'gps_valid')
# Extra min/max are worth having for these: the CSV runs 30x faster than the
# JSON, so a single nearest sample hides the peak of every corner.
EXTREMA_COLS = ('lean_deg', 'accel_lat_g', 'accel_lon_g', 'accel_total_g', 'speed_kmh')


# ------------------------------------------------------------------- loading
def load_json(path):
    """Return (ride_meta, hardware_meta, [sample dicts sorted by time])."""
    with open(path) as fh:
        doc = json.load(fh)
    data = doc.get('data') or []
    if not data:
        sys.exit(f'{path}: no "data" array')
    for s in data:
        if 'gpsTimestamp' not in s and 'date' not in s:
            sys.exit(f'{path}: sample without gpsTimestamp/date')
        s['_t'] = float(s.get('gpsTimestamp', s.get('date')))
    data.sort(key=lambda s: s['_t'])
    return doc.get('ride', {}), doc.get('hardware', {}), data


def load_csv(path):
    """Return ([row dicts sorted by utc], [times], [column names])."""
    with open(path, newline='') as fh:
        rd = csv.DictReader(fh)
        cols = list(rd.fieldnames or [])
        if 'utc' not in cols:
            sys.exit(f'{path}: no "utc" column -- cannot align by time')
        rows = []
        for r in rd:
            if not r.get('utc'):
                continue
            r['_t'] = iso_to_epoch(r['utc'])
            rows.append(r)
    if not rows:
        sys.exit(f'{path}: no rows')
    rows.sort(key=lambda r: r['_t'])
    return rows, [r['_t'] for r in rows], cols


def iso_to_epoch(s):
    return datetime.datetime.fromisoformat(s.replace('Z', '+00:00')).timestamp()


def epoch_to_iso(t):
    dt = datetime.datetime.fromtimestamp(t, datetime.timezone.utc)
    return dt.strftime('%Y-%m-%dT%H:%M:%S.') + f'{dt.microsecond // 1000:03d}Z'


def fnum(v):
    """Float from a CSV cell, or None if blank/non-numeric."""
    if v is None or v == '':
        return None
    try:
        return float(v)
    except ValueError:
        return None


# --------------------------------------------------------------- time offset
def haversine(lat1, lon1, lat2, lon2):
    p = math.pi / 180
    a = (math.sin((lat2 - lat1) * p / 2) ** 2 +
         math.cos(lat1 * p) * math.cos(lat2 * p) * math.sin((lon2 - lon1) * p / 2) ** 2)
    return 12742000 * math.asin(min(1.0, math.sqrt(a)))


def match_distance(samples, rows, times, offset):
    """Median metres between JSON positions and the CSV fix nearest in time."""
    d = []
    for s in samples[::5]:
        r = nearest(rows, times, s['_t'] - offset, 0.5)
        if r is None:
            continue
        lat, lon = fnum(r.get('lat')), fnum(r.get('lon'))
        if lat is None or lon is None or (lat == 0 and lon == 0):
            continue
        d.append(haversine(s['gpsLat'], s['gpsLon'], lat, lon))
    return (statistics.median(d), len(d)) if d else (float('inf'), 0)


def auto_offset(samples, rows, times, span, step):
    """Offset (s, added to CSV times) that best lines the two GPS tracks up."""
    best, best_d, best_n = 0.0, float('inf'), 0
    o = -span
    while o <= span + 1e-9:
        d, n = match_distance(samples, rows, times, o)
        # Require a decent overlap; a handful of points can fit anything.
        if n >= 30 and d < best_d:
            best, best_d, best_n = o, d, n
        o += step
    return best, best_d, best_n


# ------------------------------------------------------------------ matching
def nearest(rows, times, t, max_dt):
    """CSV row closest to t, or None beyond max_dt."""
    i = bisect.bisect_left(times, t)
    cand = [j for j in (i - 1, i) if 0 <= j < len(rows)]
    if not cand:
        return None
    j = min(cand, key=lambda j: abs(times[j] - t))
    return rows[j] if abs(times[j] - t) <= max_dt else None


def window(rows, times, lo, hi):
    """CSV rows with lo <= t < hi."""
    return rows[bisect.bisect_left(times, lo):bisect.bisect_left(times, hi)]


def circular_mean(degs):
    x = sum(math.cos(math.radians(d)) for d in degs)
    y = sum(math.sin(math.radians(d)) for d in degs)
    if abs(x) < 1e-12 and abs(y) < 1e-12:
        return None
    return math.degrees(math.atan2(y, x)) % 360


def aggregate(win, cols):
    """[(name, value)] per column over a window: mean, and min/max where useful.

    Kept in CSV column order, with each min/max beside the mean it belongs to.
    """
    out = []
    for c in cols:
        vals = [v for v in (fnum(r.get(c)) for r in win) if v is not None]
        if not vals:
            continue
        if c == 'gps_valid':                       # a flag, not a measurement
            out.append((c, 1 if max(vals) >= 1 else 0))
            continue
        if c in PASSTHROUGH_COLS:
            continue
        mean = circular_mean(vals) if c in ANGLE_COLS else sum(vals) / len(vals)
        if mean is None:
            continue
        out.append((c, round(mean, 6)))
        if c in EXTREMA_COLS and len(vals) > 1:
            out.append((c + '_min', min(vals)))
            out.append((c + '_max', max(vals)))
    return out


# ------------------------------------------------------------- GPX rendering
def esc(s):
    return (str(s).replace('&', '&amp;').replace('<', '&lt;').replace('>', '&gt;')
            .replace('"', '&quot;'))


def num(v):
    """Decimal text: shortest form that round-trips, never exponent notation.

    Epoch timestamps need 16 significant digits to keep their milliseconds, so
    this cannot be a fixed %g -- repr() gives the shortest exact form instead.
    """
    if isinstance(v, bool):
        return '1' if v else '0'
    if isinstance(v, int):
        return str(v)
    s = repr(float(v))
    if 'e' in s or 'E' in s:
        s = f'{v:.17f}'.rstrip('0').rstrip('.') or '0'
    if s.endswith('.0'):
        s = s[:-2]
    return s


def tag(ns, name, value):
    if value is None or value == '':
        return ''
    v = num(value) if isinstance(value, (int, float)) else esc(value)
    return f'<{ns}:{name}>{v}</{ns}:{name}>'


def meta_tags(ns, obj, prefix=''):
    """Flatten a metadata dict into <ns:key> elements; lists become CSV text."""
    out = []
    for k, v in obj.items():
        name = prefix + k
        if isinstance(v, dict):
            out.append(meta_tags(ns, v, name + '_'))
        elif isinstance(v, (list, tuple)):
            out.append(tag(ns, name, ','.join(str(x) for x in v)))
        elif isinstance(v, bool):
            out.append(tag(ns, name, 'true' if v else 'false'))
        else:
            out.append(tag(ns, name, v))
    return ''.join(out)


def main():
    ap = argparse.ArgumentParser(
        description='Merge a GoPro telemetry CSV into a MotionRecord ride JSON as GPX.')
    ap.add_argument('json_file', help='MotionRecord / Kurvenfokus ride JSON (the source)')
    ap.add_argument('csv_file', help='GoPro telemetry CSV to merge in')
    ap.add_argument('-o', '--out', help='output GPX (default: <json>.merged.gpx)')
    ap.add_argument('--csv-mode', choices=('nearest', 'mean'), default='nearest',
                    help='nearest CSV row (default) or mean over the sample interval')
    ap.add_argument('--max-dt', type=float, default=0.5, metavar='S',
                    help='drop a match further than this from the point (default 0.5 s)')
    ap.add_argument('--time-offset', type=float, metavar='S',
                    help='seconds added to CSV times before matching '
                         '(default: estimated from the two GPS tracks)')
    ap.add_argument('--no-auto-offset', action='store_true',
                    help='assume the two clocks agree instead of estimating')
    ap.add_argument('--offset-span', type=float, default=30.0, metavar='S',
                    help='search +/- this many seconds for the offset (default 30)')
    ap.add_argument('--offset-step', type=float, default=0.5, metavar='S',
                    help='offset search step (default 0.5 s)')
    ap.add_argument('--activity', default='motorcycling', help='GPX track <type>')
    ap.add_argument('--name', help='GPX track <name> (default: ride title)')
    ap.add_argument('-q', '--quiet', action='store_true')
    args = ap.parse_args()

    ride, hardware, samples = load_json(args.json_file)
    rows, times, cols = load_csv(args.csv_file)
    cols = [c for c in cols if c != 'utc']

    if args.time_offset is not None:
        offset = args.time_offset
    elif args.no_auto_offset:
        offset = 0.0
    else:
        offset, d, n = auto_offset(samples, rows, times, args.offset_span, args.offset_step)
        if not args.quiet:
            if n:
                print(f'auto offset  {offset:+.1f} s  (median track separation '
                      f'{d:.1f} m over {n} points)', file=sys.stderr)
            else:
                print('auto offset  no usable overlap -- assuming 0 s', file=sys.stderr)
    # Matching happens in JSON time; shifting the CSV forward by `offset` is the
    # same as looking for each JSON point at t - offset in raw CSV time.
    shift = -offset

    out_path = args.out or os.path.splitext(args.json_file)[0] + '.merged.gpx'
    name = args.name or ride.get('title') or os.path.basename(args.json_file)

    matched = 0
    parts = []
    for i, s in enumerate(samples):
        t = s['_t']
        lat, lon = s.get('gpsLat'), s.get('gpsLon')
        if lat is None or lon is None:
            continue

        # ---- standard GPX fields, from the JSON alone
        ext = []
        tpx = []
        if s.get('gpsSpeed') is not None:
            tpx.append(f'<gpxtpx:speed>{s["gpsSpeed"]:.3f}</gpxtpx:speed>')
        if s.get('gpsTrueCourse') is not None:
            tpx.append(f'<gpxtpx:course>{s["gpsTrueCourse"]:.1f}</gpxtpx:course>')
        if tpx:
            ext.append('<gpxtpx:TrackPointExtension>' + ''.join(tpx) +
                       '</gpxtpx:TrackPointExtension>')

        # ---- every JSON channel, verbatim, under mr:
        for k, v in s.items():
            if k == '_t' or v is None:
                continue
            ext.append(tag('mr', k, v))

        # ---- the CSV channels, under gt:
        if args.csv_mode == 'mean':
            lo = (samples[i - 1]['_t'] + t) / 2 if i else t - 0.5
            hi = (samples[i + 1]['_t'] + t) / 2 if i + 1 < len(samples) else t + 0.5
            win = window(rows, times, lo + shift, hi + shift)
            agg = aggregate(win, cols) if win else []
            if agg:
                matched += 1
                for k, v in agg:
                    ext.append(tag('gt', k, v))
                ext.append(tag('gt', 'csv_samples', len(win)))
                ext.append(tag('gt', 'csv_utc_first', win[0]['utc']))
                ext.append(tag('gt', 'csv_utc_last', win[-1]['utc']))
        else:
            r = nearest(rows, times, t + shift, args.max_dt)
            if r is not None:
                matched += 1
                for c in cols:
                    ext.append(tag('gt', c, r.get(c)))
                ext.append(tag('gt', 'csv_utc', r['utc']))
                ext.append(tag('gt', 'match_dt_s', round(r['_t'] - (t + shift), 3)))

        body = ''
        if s.get('gpsHeight') is not None:
            body += f'<ele>{s["gpsHeight"]:.2f}</ele>'
        body += f'<time>{epoch_to_iso(t)}</time>'
        body += '<extensions>' + ''.join(ext) + '</extensions>'
        parts.append(f'<trkpt lat="{lat:.7f}" lon="{lon:.7f}">{body}</trkpt>')

    if not parts:
        sys.exit(f'{args.json_file}: no positioned samples -- nothing to write')

    meta = ''.join((
        meta_tags('mr', {'ride_' + k: v for k, v in ride.items()}),
        meta_tags('mr', {'hardware_' + k: v for k, v in hardware.items()}),
        tag('gt', 'merge_source_json', os.path.basename(args.json_file)),
        tag('gt', 'merge_source_csv', os.path.basename(args.csv_file)),
        tag('gt', 'merge_mode', args.csv_mode),
        tag('gt', 'merge_time_offset_s', offset),
        tag('gt', 'merge_points_total', len(parts)),
        tag('gt', 'merge_points_with_csv', matched),
    ))

    with open(out_path, 'w') as fh:
        fh.write('<?xml version="1.0" encoding="UTF-8"?>\n'
                 '<gpx version="1.1" creator="merge-ride-telemetry.py"\n'
                 '     xmlns="http://www.topografix.com/GPX/1/1"\n'
                 f'     xmlns:gpxtpx="{XMLNS_TPX}"\n'
                 f'     xmlns:gt="{XMLNS_GT}"\n'
                 f'     xmlns:mr="{XMLNS_MR}">\n'
                 f'<metadata><name>{esc(name)}</name>'
                 f'<time>{epoch_to_iso(samples[0]["_t"])}</time>'
                 f'<extensions>{meta}</extensions></metadata>\n'
                 f'<trk><name>{esc(name)}</name>'
                 f'<type>{esc(args.activity)}</type><trkseg>\n')
        fh.write('\n'.join(parts))
        fh.write('\n</trkseg></trk>\n</gpx>\n')

    if not args.quiet:
        span = samples[-1]['_t'] - samples[0]['_t']
        pct = 100 * matched / len(parts)
        print(f'{os.path.basename(out_path)}  ({len(parts)} points, {span / 60:.1f} min)')
        print(f'  json      {os.path.basename(args.json_file)}  '
              f'{epoch_to_iso(samples[0]["_t"])} .. {epoch_to_iso(samples[-1]["_t"])}')
        print(f'  csv       {os.path.basename(args.csv_file)}  '
              f'{epoch_to_iso(times[0] + offset)} .. {epoch_to_iso(times[-1] + offset)}'
              f'  ({len(rows)} rows, offset {offset:+.1f} s)')
        print(f'  merged    {matched}/{len(parts)} points carry CSV data ({pct:.0f}%), '
              f'mode {args.csv_mode}')
        d, n = match_distance(samples, rows, times, offset)
        if n:
            print(f'  alignment median track separation {d:.1f} m over {n} sampled points')
        print(f'  {os.path.getsize(out_path) / 1e6:.1f} MB')


if __name__ == '__main__':
    main()
