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
gmax clips     <in.360|in.mp4> [--top N] [--metric LIST] | [--ss S [--duration S]]
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

# Or cut one window by hand: 30 s from twenty minutes in.
gmax clips GS010527.360 --ss 1200 --duration 30

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
chroma grid where it would show as fringing along the seams. When the encoder
is 10-bit the whole stitch runs at 10 bits (`yuv444p10le`), so the output is
10-bit in fact and not just in its label; an 8-bit encoder keeps the cheaper
8-bit path. `--preview` writes one JPEG and needs no encoder at all, so it
skips the encoder survey.

`--level` derives a rotation from the clip's mean gravity vector so the horizon
comes out level — over just the rendered segment when `--ss`/`--duration`
select one, since the whole ride's mean can be degrees off for any one stretch. `--heading D` then turns the view about the vertical to choose
what the centre of the frame looks at; the two are independent.

The GPMF telemetry track is copied into the output by default, so `gmax
telemetry` and `gmax analyse` work on a stitched file exactly as on the
original.

The output is then declared spherical, which is what makes it a 360 video
rather than a very wide, very distorted rectangle: YouTube, VLC and every
headset decide whether to offer spherical navigation by looking for Google's
`GSpherical` XMP block, and ffmpeg does not write it. That step needs exiftool;
without it the render still succeeds and says what is missing.

No command will write over its own input: an `-o` (or a derived output) that
resolves to the source file, however it is spelled, is refused before anything
is written. Paths are handed to ffmpeg as `file:` URLs, so a name that starts
with `-` or contains a colon is still just a file.

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

`analyse` and `clips` both take `--gps <GPX>`, which runs the merge above in
memory and writes the repaired positions back onto the camera's own fixes
before ranking. That is not the same as ranking the merged GPX: a GPX carries
lean angle and longitudinal acceleration and nothing else, so ranking one
silently drops cornering and combined load, which have no GPX representation.
Writing back keeps the external receiver's positions *and* the camera's full
channel set. On a ride where the camera's receiver managed 79% usable fixes,
it took every reported moment from 28-of-35 positioned to 35-of-35, and the
measured distance from 15.3 km to 18.9 km.

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
manifest records which moment chose each clip. A stream copy asks nothing of
the video's shape, so an ordinary single-track `.mp4` cuts just as well; each
clip keeps its source's extension.

A stream copy can only begin at a keyframe, so a clip starts at or before the
requested instant and may run a little longer. Trimming to the frame would mean
re-encoding.

#### Cutting a window by hand

`--ss S` cuts one clip starting there instead of ranking anything, running for
`--duration S` or to the end of the video. The names match `gmax convert`,
which takes the same pair. The clip lands in `<stem>_clips/` as
`<stem>_cut_t<start>_<duration>s.360`, with the same sibling telemetry a
ranked clip gets. Each hand cut is added to `<stem>_clips.json` rather than
replacing it, so a run of cuts from one ride ends with one manifest listing
them all, each with the window that was asked for beside the one that was cut.

The ranking options — `--top`, `--metric`, `--pre`, `--post`, `--merge-gap`,
`--min-gap`, `--curve-threshold`, `--window` — cannot shape a window that was
given outright, so they are refused alongside `--ss` rather than silently
ignored. `--gps` and `--max-dop` still apply: they shape the telemetry written
beside the clip.

Nothing is ranked, and nothing is even analysed, which is the other reason to
reach for it: a hand cut works on a ride whose telemetry is too sparse or too
broken to rank. If the telemetry cannot be read at all, the clip is still cut
and only its sibling CSV is skipped, with a warning saying why. A start past the end of the video is refused; a duration
running past it is shortened to it.

#### A clip cannot calibrate itself

Each clip gets a sibling `<clip>.telemetry.csv` holding the *ride's* telemetry,
sliced to that clip's own samples. The clip's embedded GPMF track is still
there and still correct as raw sensor data — but nothing reading the clip alone
can calibrate it. The roll zero needs straight-line riding and the gravity
reference needs coasting, and ten seconds of one corner has neither, so a clip
left to itself fits a mount from that one corner.

The size of that: the 43° left-hander below reads **39.4°** from its own clip
and **43.2°** from the ride, a 3.6° offset near-constant across all 330
samples — the same curve with a different zero. The ride's is the right one,
because the ride has straight-line riding to check it against and reads upright
as zero throughout, while the clip's straight bit reads +3.3°.

Timestamps in the sibling are re-based to start at zero, which is what a
standalone file's timeline means. The viewer prefers a sibling over a video's
embedded track for this reason, and always prints which one it used.

Two bugs fell out of building it, both invisible on a whole recording and only
visible on a clip, because a stream copy keeps the GPMF's *original*
timestamps — an eighteen-minutes-in clip has a timeline starting at 1098 s, not
at 0:

- `t0` is anchored to `times[0]`, but absolute time was computed as
  `t0 + times[i]`, double-counting the offset. Clip UTC was eighteen minutes
  into the future.
- The viewer read a CSV's duration as its last timestamp rather than its span,
  so an eleven-second clip claimed to be eighteen minutes long and its whole
  trace squeezed into the first percent of the scrubber.

### `viewer` — watch it back

Serves a self-contained WebGL 360 player with a lean gauge, g-meter and lean
trace, on `127.0.0.1:8360`. Telemetry comes from an explicit `--telemetry`
file, the video's own GPMF track, or a sibling CSV — the last because a viewer
is usually pointed at a stitched or proxy rendering, which has none of its own.

Frame stepping uses the video's own frame rate, read from its container. The
server answers only requests addressed to it by its loopback name and port, so
a web page cannot reach it by rebinding its own domain to `127.0.0.1`, and
each request is served on its own thread, so a stalled video download never
holds up the page.

### `encoders` — what this machine can actually do

**Which encoders an ffmpeg build exposes says nothing about which ones the
machine can run.** The same binary advertises `hevc_nvenc` with no NVIDIA card
and `h264_qsv` with no Intel GPU, and both fail only once the real encode
starts — an hour into a 25-minute stitch. So every hardware encoder is
verified with a one-frame test encode, fed the way the real encode feeds it:
at 3840×1920, in the encoder's own pixel format and profile (10-bit
`main10` for HEVC). A larger output, like the original MAX's 5376×2688, gets
the chosen encoder re-tested at its real size just before the render, which
catches H.264 hardware and its 4096-pixel width limit.

The verdicts are cached against the ffmpeg binary itself — its version banner,
canonical path, size and modification time — so a different or upgraded
ffmpeg re-probes automatically. A hardware failure is only believed for an
hour, since a busy GPU or a driver not yet loaded is often a passing state;
`--reprobe` still forces a full re-test.

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

#### The zero wanders

Getting the sign right still leaves the question of where zero is. Lean is the
angle between gravity and whichever direction the mount calls up, and that
direction is fitted from the straight-line parts of the ride — where a machine
held up by its own momentum must be upright.

Fitted **once per file**, that zero is right on average and wrong nearly
everywhere. On a 20-minute ride the reading a genuinely upright machine
produces wandered across a 12° band, minute to minute, as the mount crept. A
corner is then reported at whatever end of the band it fell in. One 43°
left-hander sat in a −6° pocket and was indexed, cut and filed as 49° — five
degrees clear of a field it should have tied with. The same corner cut out into
its own clip read 39°, because the clip re-fitted the zero from its own
11 seconds.

So the fit is made again locally: the median lean over the upright samples
within 30 s of each instant, subtracted from the angle. Upright riding then
reads zero *wherever* it occurs rather than on average, a cut clip agrees with
the ride it came from, and the whole-ride residual against an external
reference — `atan(v·ω/g)` from GPS speed and course rate — falls from a
drifting −6°..+5° to a flat −2.8°..+1.6°. The offset removed is kept, in the
`lean_zero_deg` CSV column and in `Telemetry::lean_zero`, so the raw angle can
always be recovered.

A source with too little straight-line riding to fit against — a short clip cut
out of a single corner — gets no correction and a warning saying so. Fitting a
zero to one corner would be worse than not fitting one.

#### GRAV is not vertical

Getting the zero right exposed the next thing. `GRAV` is the camera's *fused
estimate* of which way is down, not a measurement, and it is wrong.

Where the machine is rolling straight, upright, and neither braking nor
driving, the only force acting on it is gravity — so the accelerometer, which
measures force, **is** the vertical. On the reference ride `GRAV` sits a median
8° off it, steadily, from one end of the recording to the other (5.7° taken as
a single rotation; 8.4° on the next recording).

What that costs depends on how a channel uses `GRAV`. Lean and pitch are angles
against a reference fitted from `GRAV` itself, so a fixed tilt cancels: both
sides turn together, and correcting it moves lean by a median 0.05°. The
road-frame acceleration channels have no such reference — they resolve force
against `GRAV` directly — so the tilt lands in them whole. A machine riding
dead straight reported **0.09 g across itself**, 5° of cornering load that was
not there.

So `GRAV` is put back on the accelerometer's vertical before anything reads it:
the rotation taking one onto the other, measured as a component-wise median
over the coasting samples, applied to the whole stream. Straight-line lateral
load falls from 0.092 g to 0.012 g, and total acceleration — a magnitude, which
owes nothing to any frame — comes through bit-identical, which is the check
that the correction is a rotation and nothing more.

Unlike the roll zero this is *not* tracked over time, and not because it was
easier. The roll zero describes where the camera sits on the machine, which a
slipping mount changes. This describes how two sensors inside one camera
disagree, which no amount of mount movement can affect — so the whole recording
is one measurement of it.

The two faults were also pulling in opposite directions, which is what made the
lean channel look worse than it was. Comparing lean against the *uncorrected*
lateral load showed a 10° left/right asymmetry and suggested lean was
over-reading on left-handers. It was not: the asymmetry was in the ruler. With
both corrections in, lean exceeds the load it is measured against by 2.9° on
left turns and 0.2° on right — small, positive, and about the size of the
contact-patch offset that should be there.

#### Checking lean against something that is not lean

Lean is an angle against a calibrated idea of upright. Lateral g is a force
measured against gravity, in the gravity frame, and owes that calibration
nothing. So every reported lean peak is checked against the lateral
acceleration at the same instant, and flagged when the two disagree by more
than 8°.

The tolerance is not tight, and deliberately: a real corner leans a few degrees
further than `atan(a_lat / g)` because the contact patch sits inboard of the
tyre's centreline. What it catches is the gross case — a peak whose angle no
measured load supports, which is what a mis-placed zero or a tilted vertical
looks like from the outside. It earned its keep immediately: it was this check
firing on four of ten lean peaks, after the roll zero was already fixed, that
turned up the `GRAV` tilt above. With both corrections in it is silent on the
reference footage.

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
