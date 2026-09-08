#!/usr/bin/env python3
"""
Cut a GoPro .360 into highlight clips at a ride's most interesting moments.

The moments come from `motion-record-analysis` run over a MotionRecord /
Kurvenfokus ride JSON: the top N per metric (lean, accel, decel, speed,
curves), each with an absolute UTC timestamp. Those timestamps are on the
*phone's* clock, so they are mapped onto the video's timeline before cutting.

  ./extract-highlights.py RIDE.json VIDEO.360
  ./extract-highlights.py RIDE.json VIDEO.360 --metric lean,curves --top 5
  ./extract-highlights.py RIDE.json VIDEO.360 --pre 8 --post 6 --dry-run
  ./extract-highlights.py RIDE.json VIDEO.360 --stitch

Clips are cut with `ffmpeg -c copy`: lossless, fast, and every stream survives
-- both HEVC cube-face tracks, both AAC tracks, the 4-channel ambisonic PCM and
the `gpmd` telemetry -- so each clip is still a working .360 that
`gopromax2-stitch.sh` and `gopro-telemetry.py` read like any other.

Two things that are easy to get wrong, and are handled here:

  * The clocks differ. The phone and the camera each keep their own time. The
    offset is measured by correlating the one signal both devices record at
    high dynamic range -- lean angle -- over the whole overlap. Position and
    speed are far worse for this: two GPS receivers disagree by tens of metres
    and a few km/h regardless of alignment, which flattens the minimum. Lean
    gives |r| ~ 0.87 with a peak a few tenths of a second wide.

    The correlation is deliberately sign-agnostic: `gyroLeaningAngle` in the
    JSON and `lean_deg` from gopro-telemetry.py do not currently agree on which
    way is positive, so the peak is sought in |r| and the sign is reported.

  * Horizon levelling must not be computed per clip. gopromax2-stitch.sh
    averages gravity over whatever it is given, and a 10-second highlight is
    usually all corner, so its "upright" reference is wrong -- a 10 s clip of
    one bend levels to roll 13.3 deg where the parent file gives 1.9 deg. With
    `--stitch`, the rotation is measured once from the whole file and passed to
    every clip via `--rotate`.
"""
import argparse, bisect, csv, datetime, json, math, os, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))

# Where to look for the analyser CLI, in order. Override with --analyser or
# $MOTION_ANALYSE.
ANALYSER_CANDIDATES = (
    os.path.expanduser('~/Documents/Projects/jophj/motion-record-analysis/bin/motion-analyse.js'),
    os.path.join(HERE, 'motion-record-analysis/bin/motion-analyse.js'),
    os.path.join(HERE, '../motion-record-analysis/bin/motion-analyse.js'),
)

# Metric id -> (short label, priority). Priority decides which event names a
# clip when several fall inside one.
METRICS = {
    'lean':   ('lean',  0),
    'curves': ('curvy', 1),
    'decel':  ('brake', 2),
    'accel':  ('accel', 3),
    'speed':  ('speed', 4),
}


def die(msg, code=1):
    print(f'{os.path.basename(sys.argv[0])}: {msg}', file=sys.stderr)
    sys.exit(code)


def iso_to_epoch(s):
    return datetime.datetime.fromisoformat(s.replace('Z', '+00:00')).timestamp()


def epoch_to_iso(t):
    dt = datetime.datetime.fromtimestamp(t, datetime.timezone.utc)
    return dt.strftime('%Y-%m-%dT%H:%M:%S.') + f'{dt.microsecond // 1000:03d}Z'


def hms(t):
    """Seconds -> H:MM:SS.mmm, for filenames and tables."""
    neg = '-' if t < 0 else ''
    t = abs(t)
    h, rem = divmod(t, 3600)
    m, s = divmod(rem, 60)
    return f'{neg}{int(h)}:{int(m):02d}:{s:06.3f}'


# ------------------------------------------------------------------ analyser
def find_analyser(explicit):
    for c in (explicit, os.environ.get('MOTION_ANALYSE')):
        if c:
            if not os.path.exists(c):
                die(f'analyser not found: {c}')
            return c
    for c in ANALYSER_CANDIDATES:
        if os.path.exists(c):
            return c
    die('cannot find motion-analyse.js -- pass --analyser PATH or set '
        '$MOTION_ANALYSE')


def run_analyser(analyser, ride_json, top, extra):
    """Return the analyser's --json document."""
    if not shutil.which('node'):
        die('node not found on PATH (motion-record-analysis needs Node >= 18)')
    cmd = ['node', analyser, ride_json, '--json', '--top', str(top)] + extra
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        die(f'analyser exited {p.returncode}:\n{p.stderr.strip()}',
            p.returncode)
    try:
        return json.loads(p.stdout)
    except json.JSONDecodeError as e:
        die(f'analyser did not emit JSON: {e}')


# ------------------------------------------------------- telemetry / cadence
def telemetry_csv(video, regenerate=False):
    """Path to the video's telemetry CSV, generating it if need be."""
    path = f'{video}.telemetry.csv'
    if os.path.exists(path) and not regenerate:
        return path
    gt = os.path.join(HERE, 'gopro-telemetry.py')
    if not os.path.exists(gt):
        die(f'{path} missing and gopro-telemetry.py not next to this script')
    print(f'  extracting telemetry from {os.path.basename(video)} ...',
          file=sys.stderr)
    p = subprocess.run([gt, video, '-o', path], capture_output=True, text=True)
    if p.returncode != 0:
        die(f'gopro-telemetry.py exited {p.returncode}:\n{p.stderr.strip()}')
    return path


def load_camera(path):
    """Return (video_start_epoch, [(epoch, lean_deg)]).

    The CSV's `time_s` is presentation time and `utc` the absolute time of the
    same sample, so their difference is the video's start on the camera clock.
    """
    starts, lean = [], []
    with open(path, newline='') as fh:
        rd = csv.DictReader(fh)
        for col in ('time_s', 'utc', 'lean_deg'):
            if col not in (rd.fieldnames or []):
                die(f'{path}: no "{col}" column')
        for r in rd:
            if not r['utc'] or not r['time_s']:
                continue
            t = iso_to_epoch(r['utc'])
            starts.append(t - float(r['time_s']))
            if r['lean_deg']:
                lean.append((t, float(r['lean_deg'])))
    if not starts:
        die(f'{path}: no usable rows')
    lean.sort()
    # Every row implies the same origin; the median absorbs any odd row.
    starts.sort()
    return starts[len(starts) // 2], lean


def load_phone_lean(ride_json):
    with open(ride_json) as fh:
        doc = json.load(fh)
    out = []
    for s in doc.get('data') or []:
        if s.get('gyroLeaningAngle') is None:
            continue
        t = s.get('gpsTimestamp', s.get('date'))
        if t is None:
            continue
        out.append((float(t), float(s['gyroLeaningAngle'])))
    out.sort()
    return out


# --------------------------------------------------------------- clock align
def pearson(xs, ys):
    n = len(xs)
    mx, my = sum(xs) / n, sum(ys) / n
    sx = math.sqrt(sum((x - mx) ** 2 for x in xs))
    sy = math.sqrt(sum((y - my) ** 2 for y in ys))
    if sx == 0 or sy == 0:
        return 0.0
    return sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / (sx * sy)


def align_clocks(phone_lean, cam_lean, span, step, tol=0.2, min_n=100):
    """Offset (s) to add to a phone timestamp to reach camera time.

    Maximises |Pearson r| between the two lean series. Returns
    (offset, r, n) or (None, None, 0) if there is not enough overlap.
    """
    if not phone_lean or not cam_lean:
        return None, None, 0
    ct = [c[0] for c in cam_lean]

    def cam_at(t):
        i = bisect.bisect_left(ct, t)
        best = None
        for j in (i - 1, i):
            if 0 <= j < len(cam_lean) and abs(cam_lean[j][0] - t) <= tol:
                if best is None or abs(cam_lean[j][0] - t) < abs(cam_lean[best][0] - t):
                    best = j
        return cam_lean[best][1] if best is not None else None

    best = None
    n_steps = int(round(2 * span / step))
    for k in range(n_steps + 1):
        off = -span + k * step
        xs, ys = [], []
        for (t, lean) in phone_lean:
            c = cam_at(t + off)
            if c is None:
                continue
            xs.append(lean)
            ys.append(c)
        if len(xs) < min_n:
            continue
        r = pearson(xs, ys)
        if best is None or abs(r) > abs(best[1]):
            best = (off, r, len(xs))
    return best if best else (None, None, 0)


# -------------------------------------------------------------------- probing
def probe_video(video):
    """Return (duration_s, [tmcd stream indices])."""
    if not shutil.which('ffprobe'):
        die('ffprobe not found on PATH')
    p = subprocess.run(
        ['ffprobe', '-v', 'error', '-show_entries',
         'format=duration:stream=index,codec_tag_string',
         '-of', 'json', video], capture_output=True, text=True)
    if p.returncode != 0:
        die(f'ffprobe failed on {video}:\n{p.stderr.strip()}')
    doc = json.loads(p.stdout)
    dur = float(doc.get('format', {}).get('duration') or 0)
    if dur <= 0:
        die(f'{video}: no duration')
    # The timecode track cannot be remuxed by the mov/mp4 muxer ("codec none"),
    # and carries nothing we need -- drop it by index, which is not stable
    # across files and so is always discovered.
    tmcd = [s['index'] for s in doc.get('streams', [])
            if s.get('codec_tag_string') == 'tmcd']
    return dur, tmcd


# --------------------------------------------------------------------- events
def collect_events(analysis, metrics, top):
    """Flatten the analyser's per-metric rankings into one list."""
    out = []
    for mid in metrics:
        block = analysis['metrics'].get(mid)
        if not block:
            continue
        for e in block['entries'][:top]:
            label, prio = METRICS[mid]
            ev = {
                'metric': mid, 'label': label, 'priority': prio,
                'rank': e['rank'], 'epoch': float(e['epochSeconds']),
                'ride_offset': float(e['offsetSeconds']),
                'timestamp': e['timestamp'],
                'lat': e.get('lat'), 'lon': e.get('lon'),
                'speed_kmh': e.get('speedKmh'),
            }
            # A curve event is a window, not an instant; everything else is a
            # single sample.
            if mid == 'curves':
                ev['span'] = float(e['windowEndOffsetSeconds']) - \
                             float(e['windowStartOffsetSeconds'])
                ev['value'] = e['curveCount']
                ev['value_str'] = f"{e['curveCount']}curves"
                ev['detail'] = (f"{e['curveCount']} curves, peak lean "
                                f"{e['peakLeanDeg']:+.1f} deg")
            else:
                ev['span'] = 0.0
                if mid == 'lean':
                    ev['value'] = abs(e['leanDeg'])
                    ev['value_str'] = f"{abs(e['leanDeg']):.0f}deg"
                    ev['detail'] = f"{e['leanDeg']:+.1f} deg {e['direction']}"
                    if e.get('atSensorLimit'):
                        ev['detail'] += ' (at sensor limit)'
                elif mid == 'speed':
                    ev['value'] = e['speedKmh']
                    ev['value_str'] = f"{e['speedKmh']:.0f}kmh"
                    ev['detail'] = f"{e['speedKmh']:.1f} km/h"
                else:
                    ev['value'] = e['accelMps2']
                    ev['value_str'] = f"{e['accelMps2']:.1f}ms2"
                    ev['detail'] = (f"{e['accelMps2']:.2f} m/s2 "
                                    f"({e['accelG']:.2f} g)")
            out.append(ev)
    return out


def merge(clips, gap):
    """Merge clips that overlap or sit within `gap` seconds of one another.

    Without this, three metrics peaking in the same bend produce three
    near-identical files. The merged clip is named after its highest-priority
    contributor and records the rest.
    """
    if gap < 0 or not clips:
        return clips
    clips.sort(key=lambda c: c['start'])
    out = [clips[0]]
    for c in clips[1:]:
        prev = out[-1]
        if c['start'] <= prev['end'] + gap:
            prev['end'] = max(prev['end'], c['end'])
            prev['events'] += c['events']
        else:
            out.append(c)
    for c in out:
        c['events'].sort(key=lambda e: (e['priority'], e['rank']))
    return out


# ------------------------------------------------------------------ extraction
def cut(video, clip, out_path, tmcd, overwrite):
    """Lossless stream copy of one interval. Returns True if written."""
    if os.path.exists(out_path) and not overwrite:
        print(f'    exists, skipping: {os.path.basename(out_path)}')
        return False
    cmd = ['ffmpeg', '-v', 'error', '-y',
           '-ss', f"{clip['start']:.3f}", '-i', video,
           '-t', f"{clip['end'] - clip['start']:.3f}",
           '-map', '0']
    for i in tmcd:
        cmd += ['-map', f'-0:{i}']
    # `mov`, not `mp4`: the mp4 muxer refuses the first-order ambisonic PCM
    # track ("unsupported channel layout ambisonic 1"). The .360 extension is
    # kept so the rest of the toolchain still recognises the file.
    cmd += ['-c', 'copy', '-copy_unknown',
            '-avoid_negative_ts', 'make_zero',
            '-f', 'mov', out_path]
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        print(f'    FAILED: {p.stderr.strip()}', file=sys.stderr)
        return False
    return True


def stitch(script, clip_path, rotation, extra):
    cmd = [script, '--rotate', rotation] + extra + [clip_path]
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        print(f'    stitch FAILED: {p.stderr.strip()}', file=sys.stderr)
        return False
    return True


def full_file_rotation(script, video):
    p = subprocess.run([script, '--print-rotation', video],
                       capture_output=True, text=True)
    if p.returncode != 0:
        return None
    for line in reversed(p.stdout.strip().splitlines()):
        line = line.strip()
        if line.count(':') == 2:
            return line
    return None


# ------------------------------------------------------------------------ main
def main():
    ap = argparse.ArgumentParser(
        description='Cut highlight clips from a GoPro .360 at a ride JSON\'s '
                    'top moments.',
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('ride_json')
    ap.add_argument('video')
    ap.add_argument('-o', '--outdir', default=None,
                    help='output directory (default: <video basename>_highlights)')
    ap.add_argument('--metric', default='lean,accel,decel,speed,curves',
                    help='metrics to cut, comma separated (default: all)')
    ap.add_argument('--top', type=int, default=10,
                    help='events per metric (default 10)')
    ap.add_argument('--pre', type=float, default=6.0,
                    help='seconds of run-in before the moment (default 6)')
    ap.add_argument('--post', type=float, default=4.0,
                    help='seconds after the moment (default 4)')
    ap.add_argument('--merge-gap', type=float, default=2.0, metavar='S',
                    help='merge clips within S seconds of each other; '
                         '-1 disables (default 2)')
    ap.add_argument('--time-offset', type=float, default=None, metavar='S',
                    help='phone->camera clock offset in seconds, set by hand '
                         '(default: measured from lean angle)')
    ap.add_argument('--no-align', action='store_true',
                    help='assume the clocks agree')
    ap.add_argument('--align-span', type=float, default=30.0, metavar='S',
                    help='search +/- S seconds for the offset (default 30)')
    ap.add_argument('--align-step', type=float, default=0.1, metavar='S',
                    help='offset search step (default 0.1)')
    ap.add_argument('--analyser', default=None,
                    help='path to motion-analyse.js')
    ap.add_argument('--analyser-arg', action='append', default=[],
                    metavar='ARG', help='extra argument for the analyser '
                                        '(repeatable), e.g. --analyser-arg '
                                        '--curve-threshold=20')
    ap.add_argument('--regen-telemetry', action='store_true',
                    help='re-extract the video telemetry CSV even if present')
    ap.add_argument('--stitch', action='store_true',
                    help='also stitch each clip to equirectangular, sharing '
                         'the whole file\'s horizon rotation')
    ap.add_argument('--stitch-arg', action='append', default=[],
                    metavar='ARG', help='extra argument for the stitch script '
                                        '(repeatable)')
    ap.add_argument('--overwrite', action='store_true')
    ap.add_argument('-n', '--dry-run', action='store_true',
                    help='plan the clips and print them, write nothing')
    args = ap.parse_args()

    for p in (args.ride_json, args.video):
        if not os.path.exists(p):
            die(f'no such file: {p}')
    metrics = [m.strip() for m in args.metric.split(',') if m.strip()]
    for m in metrics:
        if m not in METRICS:
            die(f'unknown metric "{m}" (choose from {",".join(METRICS)})')
    if not shutil.which('ffmpeg'):
        die('ffmpeg not found on PATH')

    # ---- moments, from the ride JSON
    analyser = find_analyser(args.analyser)
    analysis = run_analyser(analyser, args.ride_json, args.top,
                            args.analyser_arg)
    ride = analysis['ride']
    events = collect_events(analysis, metrics, args.top)
    if not events:
        die('no events in the analysis', 3)

    # ---- the video's own timeline
    csv_path = telemetry_csv(args.video, args.regen_telemetry)
    video_start, cam_lean = load_camera(csv_path)
    duration, tmcd = probe_video(args.video)

    # ---- put the two clocks on speaking terms
    r = n = None
    if args.time_offset is not None:
        offset, how = args.time_offset, 'set by hand'
    elif args.no_align:
        offset, how = 0.0, 'assumed zero'
    else:
        offset, r, n = align_clocks(load_phone_lean(args.ride_json), cam_lean,
                                    args.align_span, args.align_step)
        if offset is None:
            print('  ! not enough lean overlap to align the clocks; '
                  'assuming zero offset', file=sys.stderr)
            offset, how = 0.0, 'no overlap, assumed zero'
        else:
            how = f'measured from lean angle, r={r:+.3f} over {n} samples'

    def to_video(epoch):
        return epoch + offset - video_start

    print()
    print('=' * 78)
    print(f"RIDE   {ride.get('title') or os.path.basename(args.ride_json)}")
    print('=' * 78)
    print(f"  ride       {ride['start']} .. {ride['end']}")
    print(f'  video      {epoch_to_iso(video_start)} .. '
          f'{epoch_to_iso(video_start + duration)}  ({hms(duration)})')
    print(f'  clock      phone -> camera {offset:+.2f} s  ({how})')
    if r is not None and r < 0:
        print('               note: the two lean signals are sign-inverted '
              'relative to each other;')
        print('               alignment used |r|, which is unaffected.')
    print(f'  window     -{args.pre:g}s / +{args.post:g}s around each moment')

    # ---- map events onto the video, and see what it actually covers
    inside, outside = [], []
    for ev in events:
        ev['video_pts'] = to_video(ev['epoch'])
        start = ev['video_pts'] - args.pre
        end = ev['video_pts'] + ev['span'] + args.post
        # Keep an event whose moment is in frame, even if its padding is not.
        if -0.001 <= ev['video_pts'] <= duration:
            inside.append({'start': max(0.0, start),
                           'end': min(duration, end), 'events': [ev]})
        else:
            outside.append(ev)

    if not inside:
        print()
        print('  No ranked moment falls inside this video.')
        print(f'  The video covers ride {hms(to_video(0) * -1 + 0)}.. '
              f'-- check the file pairing.')
        sys.exit(3)

    clips = merge(inside, args.merge_gap)

    # ---- name and report
    basename = os.path.splitext(os.path.basename(args.video))[0]
    outdir = args.outdir or os.path.join(
        os.path.dirname(os.path.abspath(args.video)), f'{basename}_highlights')
    for c in clips:
        head = c['events'][0]
        c['name'] = (f"{basename}_{head['label']}{head['rank']:02d}_"
                     f"{head['value_str']}_t{int(c['start']):04d}.360")
        c['path'] = os.path.join(outdir, c['name'])

    print()
    print(f'CLIPS  {len(clips)} from {len(inside)} in-video moments '
          f'({len(outside)} outside)')
    print('-' * 78)
    print('  clip start     dur     moment                        detail')
    for c in clips:
        head = c['events'][0]
        extra = (f"  (+{len(c['events']) - 1} more)"
                 if len(c['events']) > 1 else '')
        print(f"  {hms(c['start']):>12}  {c['end'] - c['start']:5.1f}s  "
              f"{head['metric'] + ' #' + str(head['rank']):<12} "
              f"{head['timestamp'][11:23]:<14} {head['detail']}{extra}")
        for e in c['events'][1:]:
            print(f"{'':>21}  also  {e['metric'] + ' #' + str(e['rank']):<12} "
                  f"{e['timestamp'][11:23]:<14} {e['detail']}")

    if outside:
        print()
        print(f'OUTSIDE THIS VIDEO  {len(outside)} moments '
              f'(the video covers only part of the ride)')
        print('-' * 78)
        for ev in sorted(outside, key=lambda e: e['ride_offset']):
            where = 'before' if ev['video_pts'] < 0 else 'after'
            print(f"  {ev['metric'] + ' #' + str(ev['rank']):<12} "
                  f"ride +{hms(ev['ride_offset']):<12} {ev['detail']:<34} "
                  f"{where} the video")

    if args.dry_run:
        print()
        print('  --dry-run: nothing written')
        return

    # ---- cut
    os.makedirs(outdir, exist_ok=True)
    print()
    print(f'WRITING to {outdir}')
    print('-' * 78)
    written = []
    for c in clips:
        print(f"  {c['name']}")
        if cut(args.video, c, c['path'], tmcd, args.overwrite):
            written.append(c)

    # ---- optional stitch, with one rotation for all clips
    if args.stitch and written:
        script = os.path.join(HERE, 'gopromax2-stitch.sh')
        if not os.path.exists(script):
            print('  ! gopromax2-stitch.sh not found, skipping --stitch',
                  file=sys.stderr)
        else:
            rot = full_file_rotation(script, args.video)
            print()
            print('STITCHING')
            print('-' * 78)
            if rot:
                print(f'  horizon rotation {rot} (from the whole file, so '
                      f'every clip shares it)')
            else:
                print('  ! could not read the full file rotation; '
                      'clips would each level themselves, skipping',
                      file=sys.stderr)
            if rot:
                for c in written:
                    print(f"  {os.path.splitext(c['name'])[0]}_equirect.mp4")
                    stitch(script, c['path'], rot, args.stitch_arg)

    # ---- manifest
    manifest = os.path.join(outdir, f'{basename}_highlights.json')
    with open(manifest, 'w') as fh:
        json.dump({
            'ride': ride,
            'source_video': os.path.abspath(args.video),
            'video_start_utc': epoch_to_iso(video_start),
            'video_duration_s': round(duration, 3),
            'clock_offset_s': round(offset, 3),
            'clock_offset_method': how,
            'window': {'pre_s': args.pre, 'post_s': args.post,
                       'merge_gap_s': args.merge_gap},
            'clips': [{
                'file': c['name'],
                'start_s': round(c['start'], 3),
                'end_s': round(c['end'], 3),
                'duration_s': round(c['end'] - c['start'], 3),
                'start_utc': epoch_to_iso(video_start + c['start']),
                'events': [{k: e[k] for k in
                            ('metric', 'rank', 'timestamp', 'ride_offset',
                             'video_pts', 'value', 'detail', 'lat', 'lon',
                             'speed_kmh') if k in e}
                           for e in c['events']],
            } for c in clips],
            'events_outside_video': [{
                'metric': e['metric'], 'rank': e['rank'],
                'timestamp': e['timestamp'], 'ride_offset': e['ride_offset'],
                'detail': e['detail'],
            } for e in sorted(outside, key=lambda e: e['ride_offset'])],
        }, fh, indent=2)

    total = sum(os.path.getsize(c['path']) for c in written
                if os.path.exists(c['path']))
    print()
    print(f'  {len(written)} clips, {total / 1e9:.2f} GB')
    print(f'  manifest {os.path.basename(manifest)}')


if __name__ == '__main__':
    main()
