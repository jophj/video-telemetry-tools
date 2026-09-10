# gmax

A GoPro Max 2 toolkit: stitch `.360` footage to equirectangular, extract GPX
and telemetry, rank the interesting moments of a ride, and repair the camera's
GPS from a better source.

One binary, on macOS, Linux and Windows.

```
gmax convert   <in.360> [--level] [--heading D] [-q high|max|av1|prores|h264]
gmax telemetry <in.360> [-o out.csv] [--json]
gmax gpx       <in.360> [-o out.gpx]
gmax merge     <base.360|base.gpx> --with external.gpx [-o merged.gpx]
gmax analyse   <video|csv|gpx> [--json] [--chapters F] [--edl F] [--cues F]
gmax clips     <in.360> [--top N] [--metric LIST]
gmax viewer    <video> [--telemetry F] [--port 8360]
gmax encoders  [--accel gpu|cpu]
```

`convert`, `clips` and `viewer` need ffmpeg on `PATH`. `convert` also uses
exiftool, if it is there, to declare the output spherical. The rest read the
container directly and need nothing installed.

## Quick start

```sh
# What can this machine encode with?
gmax encoders

# Stitch, with the horizon levelled from the camera's own gravity vector.
gmax convert GS010527.360 --level

# Telemetry and a GPX track.
gmax telemetry GS010527.360
gmax gpx GS010527.360

# Where were the interesting bits?
gmax analyse GS010527.360

# Cut them out, losslessly.
gmax clips GS010527.360 --top 5

# Watch it back with the telemetry overlaid.
gmax viewer GS010527_equirect.mp4
```

## What each command does

### `convert` — stitch to equirectangular

A `.360` holds two HEVC tracks, each a horizontal strip of three faces of an
in-camera equi-angular cubemap: track 0 carries left, front and right, track 1
carries down, back and up. The outer face of each strip is stored split in
half with a band of duplicated angle at the fisheye seam, so the filter graph
crops four pieces per split face and cross-fades the middle two.

Output defaults to the native angular resolution — four cube faces across,
3840×1920 for the Max 2. Colour conversion happens *before* the geometric
transform, which keeps the resampling's intermediate precision off the 4:2:0
chroma grid where it would show as fringing along the seams, and the output is
10-bit where the encoder supports it.

`--level` derives a rotation from the clip's mean gravity vector so the horizon
comes out level. `--heading D` then turns the view about the vertical to choose
what the centre of the frame looks at; the two are independent.

The GPMF telemetry track is copied into the output by default, so `gmax
telemetry` and `gmax analyse` work on a stitched file exactly as on the
original.

The output is then declared spherical, which is what makes it a 360 video
rather than a very wide, very distorted rectangle: YouTube, VLC and every
headset decide whether to offer spherical navigation by looking for Google's
`GSpherical` XMP block, and ffmpeg does not write it. That step needs exiftool;
without it the render still succeeds and says what is missing.

### `telemetry` — rider channels as CSV or JSON

Reads the GPMF track directly and derives lean angle, pitch, yaw rate,
lean rate and longitudinal, lateral, vertical and total acceleration in the
vehicle's own frame, alongside position, speed, course and altitude.

The hard part is not the trigonometry but deciding which way is forward and
which way is right, because a camera can be mounted at any angle. That comes
from calibrating against gravity over the straight-line parts of the ride, then
resolving the remaining sign ambiguity against GPS — see
[Lean angle](#lean-angle) below.

Every lateral channel is signed **positive to the right**.

### `gpx` — a GPX 1.1 track

One point per GPS fix, with speed and course in the Garmin
`TrackPointExtension` and lean angle in a `gt:` namespace, since GPX has no
standard home for it. Validates against the official GPX 1.1 schema.

### `merge` — repair the GPS from another source

The camera's receiver is the weak link in an otherwise good telemetry set. On
the reference clip it has no lock for the first thirteen seconds and loses it
for eleven of twenty-five minutes, while a phone in the same tank bag reported
a four-metre fix throughout. Lean, acceleration and orientation come from the
camera's IMU and are unaffected, so the useful operation is to keep everything
it measured well and substitute only the positions it measured badly.

```sh
gmax merge GS010527.360 --with phone.gpx -o merged.gpx
```

Two things are measured, because they are different quantities and fitting
positions alone cannot separate them:

- **The clock offset** between the two devices, from correlating lean angle.
  Both devices measure the same corners with instruments that work, so the
  peak is sharp.
- **The camera's GPS latency** — how far behind its positions run relative to
  the timestamps attached to them. This is the difference between the
  positional fit and the lean fit, and the positions are re-timed by it.

Then each point is decided on measured quality. Both sources are scored on
their own self-consistency — the spread of position-differenced speed against
reported Doppler speed, which needs no external truth — and the better
receiver takes the overlap. `--prefer gopro` or `--prefer external` overrides.

Positions are never invented. Substitution needs external points bracketing
the instant, so nothing is extrapolated past the ends of the external track,
and a point neither source can place is left out rather than filled in. Every
run records its method, offset, latency, residual disagreement and both
quality scores in the GPX metadata, so a repaired track can be audited.

### `analyse` — rank the interesting moments

Seven metrics: maximum lean angle, acceleration, deceleration, speed, the
curviest windows, peak cornering load and peak combined load. The last two need
a vehicle-frame IMU and so are camera-only.

Works on a video, a telemetry CSV or any GPX — including a phone export, which
is how it is checked against the reference implementation.

Sample cadence is **measured at runtime and never assumed**. Every window is
expressed in seconds, every derivative divides by the actual interval between
the two samples involved, and nothing uses a sample count as a proxy for
elapsed time. That is what lets one implementation serve a 1 Hz phone export
and 10 Hz camera telemetry with no configuration.

A consequence worth stating: a reported maximum is the maximum *of the samples
present*. At 1 Hz a corner's true peak lean almost certainly fell between two
samples, so the figure understates it. The report says so.

Ranked moments are separated by `--min-gap` (default 5 s), or a top-ten list is
not ten moments — one hard stop spanning three samples would take three slots.

`--chapters`, `--edl` and `--cues` write ffmpeg chapter markers, a CMX3600 edit
decision list and a cue-sheet CSV from the same set of windows.

### `clips` — cut the moments out

Lossless stream copies keeping both lenses, both audio tracks and the
telemetry, so a clip can be stitched or analysed exactly like the original. A
manifest records which moment chose each clip.

A stream copy can only begin at a keyframe, so a clip starts at or before the
requested instant and may run a little longer. Trimming to the frame would mean
re-encoding.

### `viewer` — watch it back

Serves a self-contained WebGL 360 player with a lean gauge, g-meter and lean
trace, on `127.0.0.1:8360`. Telemetry comes from an explicit `--telemetry`
file, the video's own GPMF track, or a sibling CSV — the last because a viewer
is usually pointed at a stitched or proxy rendering, which has none of its own.

### `encoders` — what this machine can actually do

**Which encoders an ffmpeg build exposes says nothing about which ones the
machine can run.** The same binary advertises `hevc_nvenc` with no NVIDIA card
and `h264_qsv` with no Intel GPU, and both fail only once the real encode
starts — an hour into a 25-minute stitch. So every hardware encoder is
verified with a one-frame test encode, and the verdicts are cached against the
ffmpeg version banner.

A `--quality` preset is therefore a *candidate list*, not a fixed encoder: the
first one that works here wins.

| preset | candidates, in order |
|---|---|
| `high` (default) | HEVC, hardware first: videotoolbox → nvenc → qsv → amf → mf → vaapi → libx265 |
| `max` | libx265 first, then the hardware HEVC encoders |
| `av1` | libsvtav1 → av1_nvenc → av1_qsv → av1_amf → av1_vaapi → libaom-av1 |
| `prores` | prores_ks → prores_videotoolbox |
| `h264` | libx264, then the hardware H.264 encoders |

So the same command line resolves to VideoToolbox on a Mac, NVENC or VAAPI on
Linux, and NVENC, QSV, AMF or Media Foundation on Windows. Every preset has a
software fallback. `-E NAME` forces one, `--enc-quality N` sets the
rate-control value on that encoder's own scale, and `--accel gpu|cpu`
restricts the choice.

## Notes on the format

Established by decoding the reference clip rather than from documentation.

A GoPro Max 2 `.360` is an MP4 with seven streams: two HEVC strips of
3008×960, two stereo AAC tracks, a timecode track, a `gpmd` telemetry track
and a four-channel ambisonic PCM track. **The second video track's index is not
stable** — 5 on one clip, 4 on another, depending on how many audio tracks the
camera wrote — so it has to be discovered.

One GPMF payload spans 1.001 s, from the container's own sample table. A stream
carrying 30 samples per payload therefore runs at **29.97 Hz**, not 30, and
assuming otherwise makes a 25-minute clip's timestamps 1.5 s fast by the end.

| stream | rate | notes |
|---|---|---|
| `GRAV` | 29.97 Hz | gravity as a unit vector, one per video frame. No `ORIN`. |
| `CORI`, `IORI` | 29.97 Hz | orientation quaternions, *relative to the first frame* |
| `ACCL` | 201 Hz | `ORIN=ZXY`, m/s² |
| `GYRO` | 806 Hz | `ORIN=ZXY`, rad/s |
| `GPS9` | 10 Hz | lat, lon, alt, 2D speed, 3D speed, days, seconds, DOP, fix |
| `MAGN`, `MNOR` | 25 / 29.97 Hz | magnetometer and magnetic-north orientation |

`GRAV` is the only *absolute* attitude reference: `CORI` starts at identity, so
it describes rotation since the clip began and cannot say which way is up.
`GRAV` carries no `ORIN` and has its Y and Z axes swapped relative to the
`ORIN=ZXY` IMU streams — verified numerically, in that the first `ACCL` sample
reoriented and swapped is parallel to the first `GRAV` sample at 9.8 m/s².

GPMF strings are **Latin-1**, not UTF-8: `m/s²` is stored as `6d 2f 73 b2`.

### GPS quality

`GPS9`'s fix-type field is not reliable. On the reference clip — firmware
`H24.02.01.30.00` — it reads 0 for all 15 375 samples, including the 55 % of
the ride with a three-metre lock, so gating on `fix >= 2` would reject the
whole recording. `gmax` honours the field only when a recording ever reports a
non-zero value.

Dilution of precision is exact for detecting *loss of lock*: it sits at the
99.99 sentinel through acquisition and drops to 3.31 on the very sample where
the position stops being nonsense. It is useless for judging quality *within* a
lock, where this camera reports a near-constant 3.04 to 3.31 while a tenth of
its position steps imply a speed more than 50 % above its own Doppler speed. So
fixes are additionally checked against that speed, and `merge` compares both
sources' self-consistency rather than trusting either one's self-report.

### Lean angle

The prototype's lean angle was inverted, and its viewer multiplied the channel
by −1 to compensate. The cause is worth recording, because the signal involved
looks convincing.

Signing lean against gyro yaw rate, `dot(GYRO, GRAV)`, gives a strong
correlation — `|r| = 0.94` on real footage. But its *sign convention* is the
opposite of the one being reported: `GRAV` points up and the sensor frame is
right-handed, so a right-hand turn is a **negative** rotation about the up
axis. Forcing lean to agree with it makes lean negative in a right-hand turn:
confidently, consistently backwards.

`gmax` resolves it in two stages, using each signal for what it is good at.
First it establishes that `-dot(GYRO, GRAV)` really is rightward turn rate by
correlating it against GPS course rate, whose sign depends on nothing that was
calibrated — turning right always increases bearing. Then it signs lean against
that turn rate, which is strong and now known to be the right way round. The
decision and its confidence are reported.

Course itself needs a one-second baseline. Over one 10 Hz interval a bike at
65 km/h moves 1.8 m, which is inside the receiver's own error, so the bearing is
mostly noise:

| baseline | 0.1 s | 0.5 s | 1.0 s | 2.0 s | 3.0 s |
|---|---|---|---|---|---|
| r(lean, course rate) | 0.069 | 0.270 | **0.394** | 0.312 | 0.145 |

An independent check falls out of `merge`: the phone's separately measured lean
angle correlates *positively* with the camera's.

### Levelling

`v360`'s rotation convention was established by rendering probe frames and
tracking where the sky went: yaw turns about the world vertical, pitch about
the lateral axis with positive pitch raising content, roll about the view axis,
and its world frame coincides with the `GRAV` frame.

Levelling is then two angles — yaw gravity's meridian round to face away from
the viewer, then pitch it up to the zenith. The rotation order is `pry` rather
than `v360`'s default `ypr`, so yaw is applied *last*, about the
already-levelled vertical; under `ypr` any heading choice moves the meridian
that pitch acts on and destroys the levelling.

## Development

```sh
cargo build --release
cargo test --release
```

Tests that need the reference footage skip cleanly when it is absent, so
`cargo test` passes on a fresh clone and reports what it could not run. Put
material in `sources/` to enable them; the reference CSV is regenerated with

```sh
python3 prototypes/gopro-telemetry.py sources/GS010527.360 \
    -o sources/GS010527.prototype-telemetry.csv
```

`prototypes/` holds the bash, Python and Node tools this replaces, kept for
reference and for parity checks. Their README carries the original
reverse-engineering notes on the `.360` layout.
