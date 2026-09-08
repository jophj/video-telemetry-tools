# GoPro MAX 2 `.360` → equirectangular

`gopromax2-stitch.sh` converts GoPro MAX 2 `.360` files into ordinary
equirectangular video that any player or NLE can read, using only stock
`ffmpeg` — no GoPro Player, no custom filter build.

```sh
./gopromax2-stitch.sh --preview ../100GOPRO/GS010526.360   # single JPEG, check it
./gopromax2-stitch.sh ../100GOPRO/GS010526.360             # → GS010526_equirect.mp4
./gopromax2-stitch.sh --help
```

## Why a script is needed

A `.360` is an MP4 whose payload is not viewable video. `GS010526.360` contains:

| stream | contents |
|---|---|
| `0:0` | HEVC 3008×960 — cube faces **left, front, right** |
| `0:4` | HEVC 3008×960 — cube faces **down, back, up** |
| `0:1` | AAC stereo |
| `0:5` | PCM 24-bit, 4-channel first-order ambisonic |
| `0:3` | `gpmd` — GPMF telemetry (IMU, gravity, GPS, exposure…) |
| `0:2` | `tmcd` timecode |

The camera stitches the two fisheyes on-board into an **equi-angular cubemap
(EAC)** and splits the six faces across two video tracks. `udta/GPMF` states
this outright: `PRJT = EACO`, `MINF = MAX2`.

Note the second video track's index is **not stable** — it is `0:4` on
`GS010526` and `0:5` on `GS010527`, depending on how many audio tracks the mode
records. The script always discovers it.

## Frame layout (measured, not documented anywhere)

Each 3008×960 track is a horizontal strip of three 960 px cube faces. The face
size equals the track height. Two of the three faces are **split down their
centre line**, because that centre line is exactly where the two fisheyes meet,
and each half carries 32 px of overlap from its own lens:

```
 x=0        512        1024              1984       2496       3008
  ├──────────┼──────────┼─────────────────┼──────────┼──────────┤
  │ face A   │  face A  │     face B      │ face C   │  face C  │
  │ lens 1   │  lens 2  │  complete, one  │ lens 1   │  lens 2  │
  │ +32 ovl ▶│◀ +32 ovl │      lens       │ +32 ovl ▶│◀ +32 ovl │
  └──────────┴──────────┴─────────────────┴──────────┴──────────┘
   448 + 32   32 + 448          960        448 + 32   32 + 448
```

So the width is `3·F + 2·OVL` = `3·960 + 2·64` = 3008, and the only hard
discontinuities in the frame are at x=512 and x=2496 — the two fisheye seams.
Everything else is continuous, including across the face boundaries at x=1024
and x=1984, which is what proves the strip is a genuine three-face cubemap row.

The script derives `F = height` and `OVL = (width − 3F)/2` from the file, so it
adapts to other capture resolutions. The same arithmetic also fits the original
MAX (4096×1344 → F=1344, OVL=32), though that camera is untested here.

### Reassembly

For each split face, the two panels are cross-faded linearly across their 64 px
of duplicated angle, which removes the seam. The six reconstructed faces are
then laid out as ffmpeg's 3×2 EAC —

```
row 0:  left  front  right
row 1:  down  back   up      (rotated so the strip is continuous)
```

— and handed to `v360=input=eac:output=e`. The exact face order and rotations
were measured by pushing a direction-encoded test sphere through
`v360=e:eac` and decoding where each axis landed, rather than guessed.

### Output resolution

EAC samples a uniform 90° per face, so a 960 px face is 10.667 px/degree and a
full turn is exactly **4 faces = 3840×1920**. That is the default: it is the
source's native angular resolution, so no detail is added or discarded. Use
`--oversample 1.5` (5760×2880) if the result will be graded or reframed
afterwards and you want to keep the resampling loss out of that second pass.

## Horizon levelling

The GPMF `GRAV` stream gives gravity in camera axes, and those axes coincide
with the cube axes (`+Z` = the first track's complete face). The script averages
`GRAV` over the clip and rotates the sphere so gravity lands at the bottom:

```
roll  = asin(−gx)
pitch = atan2(gz, −gy)        # v360's y-p-r convention
```

This is **static** levelling — one rotation for the whole clip. For
`GS010526.360` (camera resting, lens pointing at the ground) it is exactly
right. For a moving ride the mean is a compromise; per-frame levelling would
need the `CORI`/`IORI` quaternions driven into a time-varying `v360`, which this
script does not attempt. `--no-level` keeps the camera-native orientation and
`--rotate Y:P:R` sets it by hand.

With `--join`, the rotation is computed from the first chapter and applied to
all of them, so the orientation does not jump mid-clip.

## Quality

The source is 8-bit 4:2:0 HEVC, full range (`yuvj420p`, bt709). The pipeline:

- converts to **4:4:4** before the geometric transform, so chroma is
  interpolated at full resolution instead of being reprojected at quarter
  resolution and re-upsampled;
- uses **lanczos** resampling in `v360`;
- keeps **full-range** levels (verified: no limited-range rescale) and tags
  `bt709` / `pc` on the output;
- encodes 10-bit to keep the transform's intermediate precision out of the
  8-bit quantisation grid.

| `--quality` | encoder | ~bitrate at 3840×1920 | speed |
|---|---|---|---|
| `high` (default) | `hevc_videotoolbox` q=65, 10-bit | ~50 Mb/s | ~1.6× realtime |
| `max` | `libx265` CRF 16, preset slow, 10-bit | ~35 Mb/s | ~5× slower than realtime |
| `prores` | ProRes 422 HQ | ~800 Mb/s | fast, very large |
| `h264` | `libx264` CRF 18 | — | for maximum compatibility |

`high` is the default because `max` would take hours on the 8.5 GB clips in
`100GOPRO/`; it is the better choice if you have the time and want the smallest
transparent file.

Audio and telemetry are passed through untouched: `--audio stereo` (default)
copies the AAC track, `--audio ambisonic`/`both` copies the 4-channel ambisonic
PCM losslessly and switches the container to `.mov` (MP4 cannot carry it). The
`gpmd` telemetry track survives into the output unless you pass
`--no-telemetry`.

## 360 metadata — needs `exiftool`

Players and YouTube only treat the file as 360 if the projection is declared in
the container. ffmpeg cannot write those boxes, so the script calls `exiftool`
if it is installed and otherwise warns:

```sh
brew install exiftool
```

Without it the video is geometrically correct but will play as a flat 2:1 image.

## Timestamp metadata

Re-encoding loses the capture time twice over, so the script captures it before
encoding and writes it back after. Without that, ffmpeg zeroes the container
dates to `0000:00:00 00:00:00` — it does not carry `creation_time` across unless
asked, and GoPro's `udta` block holding the *local* time, the UTC offset and the
camera identity is proprietary, so ffmpeg cannot copy it at any setting.

What the output now carries, all matching the source:

| field | value | written by |
|---|---|---|
| `creation_time`, track/media dates | `2026-09-08T11:06:53Z` (UTC) | `-map_metadata 0` |
| `Keys:CreationDate` | `2026:09:08 13:06:52+02:00` (local + offset) | exiftool |
| `XMP-exif:DateTimeOriginal`, `XMP:DateCreated` | same | exiftool |
| `tmcd` timecode track | `13:06:53;07` (local wall clock) | `-timecode` |
| `Make` / `Model` | `GoPro` / `MAX2` | exiftool |

`Keys:CreationDate` is the atom Finder, QuickTime and Photos read, and the only
standard place that keeps the UTC offset next to the local time — the timezone
is otherwise unrecoverable from the file, since everything else is UTC.

Two wrinkles worth knowing:

- **`--join` takes its timestamps from the first chapter**, which is when the
  recording actually began. The concat demuxer exposes no global metadata for
  `-map_metadata 0` to copy, so `creation_time` is set explicitly there.
- **The timecode track is written once, onto the final file.** A `tmcd` track in
  the intermediate parts of a `--join` cannot be re-muxed by the concat step's
  `-c copy` — it fails with *"no tag for codec none in stream #3"*.

Even with none of this, absolute time is still recoverable, because the GPMF
telemetry track survives the conversion intact and carries GPS time:
`gopro-telemetry.py` reads 11:06:47 UTC from a stitched file and from the
original `.360` alike. That is UTC only, though, and needs a parser — the
container tags above are what a file browser or NLE will show you.

## Companion files

`.LRV` is a low-resolution proxy — for MAX 2 it is a **dual-fisheye** 1408×704
render, not equirectangular, so it is useful for a quick look but not as a
proxy for the stitched result. `.THM` is a 1408×704 JPEG thumbnail of the same
dual-fisheye framing. Neither is needed for conversion; the script only reads
the `.360`.

For **telemetry**, though, the `.LRV` is as good as the master — its `gpmd`
track is a bit-exact copy. See [LRV and THM as telemetry
sources](#lrv-and-thm-as-telemetry-sources).

Long recordings are chaptered as `GS01nnnn`, `GS02nnnn`, `GS03nnnn`.
`--join` converts them all and concatenates the result. `--ss` and `-t` apply
per chapter.

Note the proxy for an `.MP4` is named with a `GL` prefix, not `GX` —
`GX010525.MP4` pairs with `GL010525.LRV`.

# Ride telemetry — `gopro-telemetry.py`

The `gpmd` track carries far more than metadata. `gopro-telemetry.py` reads it
and writes **speed, lean angle, GPS position, body rates and cornering load** as
CSV and/or GPX — no GoPro software, no network, just `ffmpeg` and Python.

```sh
./gopro-telemetry.py ../100GOPRO/GS010522.360                    # → .telemetry.csv
./gopro-telemetry.py ../100GOPRO/GS010522.360 --gpx ride.gpx     # CSV + GPX
./gopro-telemetry.py ../100GOPRO/GS010522.360 --no-csv --gpx     # GPX only
./gopro-telemetry.py ../100GOPRO/GS010522.360 --gpx --gpx-hz 1   # 1 Hz, small file
```

```
GS010522.360  (1536 s, 46080 samples @ 30 Hz)
  start         2026-09-05 08:41:15 UTC
  gps fix       100% of clip (15374/15374 samples)
  distance      27.11 km   climb 855 m   alt 261-924 m
  speed         max 132.1 km/h   avg moving 65.3 km/h
  lean          max left -33.8°   max right 41.0°
  load (p99.5)  lat 0.64 g   brake -0.44 g   drive 0.41 g
```

Extraction does **not** decode the video — the track is stream-copied, so a
3.4 GB `.360` yields its 9 MB of telemetry in ~0.1 s.

## Channels

Both outputs carry the same channels, on the `GRAV` timebase (30 Hz, or 60 Hz
in the 5.3K MP4 modes).

| channel | unit | meaning |
|---|---|---|
| `utc` | ISO 8601 | absolute UTC time (see below) |
| `time_s` / `video_time_s` | s | offset into the clip, for syncing overlays |
| `lat` `lon` `alt_m` | deg, m | GPS position |
| `speed_kmh` / `speed_3d_kmh` | km/h | GPS ground and 3D speed |
| `course_deg` | deg | course over ground, from the GPS track |
| `lean_deg` | deg | **lean angle**, + = right-hand turn |
| `lean_rate_dps` | °/s | roll rate — how fast it is flicked in |
| `pitch_deg` | deg | nose-up angle (road gradient + brake dive) |
| `pitch_rate_dps` | °/s | pitch rate |
| `yaw_rate_dps` | °/s | turn rate about true vertical |
| `accel_lon_g` | g | + drive, − braking |
| `accel_lat_g` | g | cornering load, + = rightward |
| `accel_vert_g` | g | vertical load beyond 1 g (bumps) |
| `accel_total_g` | g | total specific force |
| `gps_dop` | — | dilution of precision |
| `imu_temp_c` | °C | IMU die temperature (**not** ambient air) |
| `gps_valid` | 0/1 | whether the fix was real (CSV only) |

Accelerations are averaged over ±0.2 s. A shorter window reports road buzz
rather than load on the machine — a bike on tarmac puts multi-g vibration
through the accelerometer, and at ±15 ms the same ride "brakes" at 7 g.

## GPX layout

GPX 1.1. Standard fields carry what the format has names for — `<ele>`,
`<time>`, `<hdop>`, and `gpxtpx:speed`/`gpxtpx:course` in Garmin's
`TrackPointExtension`. Everything else goes in a `gt:` namespace:

```xml
<trkpt lat="44.0830168" lon="11.2832697">
  <ele>819.49</ele>
  <time>2026-09-05T08:57:30.999Z</time>
  <hdop>3.00</hdop>
  <extensions>
    <gpxtpx:TrackPointExtension>
      <gpxtpx:speed>9.029</gpxtpx:speed>
      <gpxtpx:course>279.8</gpxtpx:course>
    </gpxtpx:TrackPointExtension>
    <gt:lean_deg>41.01</gt:lean_deg>
    <gt:lean_rate_dps>-15.7</gt:lean_rate_dps>
    <gt:pitch_deg>3.23</gt:pitch_deg>
    <gt:yaw_rate_dps>38.8</gt:yaw_rate_dps>
    <gt:accel_lat_g>0.658</gt:accel_lat_g>
    <gt:accel_total_g>1.223</gt:accel_total_g>
    <gt:imu_temp_c>45.7</gt:imu_temp_c>
    <gt:video_time_s>975.900</gt:video_time_s>
  </extensions>
</trkpt>
```

That is the deepest-lean point of the ride, and a good illustration of the
peak-accuracy caveat below: `lean_rate_dps` of −15.7 says it is already rolling
*out* of the corner, so the steady-state cross-checks disagree there — 41.0°
from `GRAV` against 35.1° from `accel_total_g` and 31.9° from *vω/g*.

IMU temperature is deliberately *not* written as `gpxtpx:atemp`, which means
air temperature; this sensor reads 36–46 °C because it is inside the camera.

Points are emitted only where the GPS actually had a fix, since a `<trkpt>`
without a position is meaningless — so a clip that never locked on produces an
empty track, and the tool says so. Default cadence is the GPS rate (~10 Hz),
because positions merely repeat at the faster `GRAV` rate; `--gpx-hz` overrides
it in either direction (30 Hz → 34 MB for 25 min, 1 Hz → 1.1 MB).
`--activity` sets the track `<type>`, default `motorcycling`.

## LRV and THM as telemetry sources

**`.LRV` carries the full telemetry.** Its `gpmd` track is not reduced or
resampled — it is byte-for-byte the same track as the master's. Verified by
hashing the extracted stream for every pair on the card:

| proxy | master | `gpmd` sha256 | sizes |
|---|---|---|---|
| `GS010522.LRV` | `GS010522.360` | match | 1.17 GB vs 8.47 GB |
| `GS010523.LRV` | `GS010523.360` | match | 1.17 GB vs 8.39 GB |
| `GS010527.LRV` | `GS010527.360` | match | 1.22 GB vs 8.55 GB |
| `GS020523.LRV` | `GS020523.360` | match | 1.19 GB vs 8.52 GB |
| `GL010525.LRV` | `GX010525.MP4` | match | 0.07 GB vs 4.11 GB |
| …all 11 pairs | | match | |

Both give the same 22.19 MB of GPMF for `GS010522`, 1536 payloads, every stream
at the same rate — and the tool produces a byte-identical CSV from either:

```sh
./gopro-telemetry.py ../100GOPRO/GS010522.LRV --gpx ride.gpx   # just works
```

So an archive of `.LRV` files retains **100% of the ride data** at roughly 1/7
the size, or 1/58 for the MP4 pair. That is the useful part: if disk space
forces a choice, the proxies are not a lossy copy of the telemetry.

It is *not* faster, though — 0.54 s from the LRV against 0.47 s from the
`.360`. `ffmpeg` only touches the moov atom and the interleaved `gpmd` samples,
so the master's extra 7 GB of video is never read either way.

**`.THM` is a single frame only.** It holds no `gpmd` track; its GoPro EXIF
MakerNotes carry one `DEVC` block with `GravityVector`, `CameraOrientation`,
`ImageOrientation`, plus a GPS fix and timestamp — all for the first frame.
The gravity vector matches `GRAV[0]` from the master exactly
(`0.174230, 0.807703, 0.563250`), and the GPS agrees to ~0.7 m, the EXIF
rounding to 1/100 arcsecond.

One sample cannot give a lean angle: the mount calibration needs straight-line
riding to find upright, so there is nothing to reference a single vector
against. What THM *is* good for is indexing a card — one position and time per
clip, for the whole card, without reading a gigabyte:

```sh
exiftool -n -filename -gpslatitude -gpslongitude -gpsdatetime -T ../100GOPRO/*.THM
```

That returns in 0.2 s. Only clips that had a fix **at the first frame** report a
position, so the partial-coverage clips (`GS0x0527`, 46–73% fix) come back
empty even though they do contain GPS later on.

## Timestamps

`GPS9` carries GPS time as days since **2000-01-01** plus seconds-of-day UTC.
Confirmed against the camera's own `CreationDate`: GS030527 reports
11:58:02 UTC against a `CreationDate` of 11:58:08 local-adjusted, a 6 s match.
Chapters line up too — `GS010522` ends 09:06:50 and `GS020522` starts
09:06:52.

Note the camera's `CreateDate` MP4 atom is *not* reliable (it reads 11:06:53
for a clip recorded at 11:58); `CreationDate` and GPS time agree with each
other. Clips whose GPS never got a fix fall back to the container
`creation_time`, and their `GPS9` date fields hold a 2021 placeholder.

## What the camera records

Per `.360`, all from one `DEVC` device named `MAX2`:

| stream | rate | contents |
|---|---|---|
| `GYRO` | 800 Hz | angular rate, rad/s |
| `ACCL` | 200 Hz | acceleration, m/s² |
| `GRAV` | 30 Hz | **gravity direction** in camera frame, unit vector |
| `CORI` / `IORI` | 30 Hz | camera / image orientation quaternions |
| `MAGN` / `MNOR` | 25 / 30 Hz | magnetometer, magnetic north |
| `GPS9` | 10 Hz | lat, lon, alt, 2D+3D speed, date, DOP, fix |
| — | 30 Hz | exposure, ISO, white balance, wind, mic wetness, temperature |

Rates vary by mode — the 5.3K MP4 modes log `GYRO` at 200 Hz and `GRAV` at
60 Hz. The script derives every rate from the file.

## How lean angle is recovered

Lean cannot be taken from the accelerometer. A bike in a steady corner leans
until the resultant of gravity and centrifugal force runs through the tyre
contact patch, so the accelerometer's "down" stays roughly **bike-aligned** and
reads a lean of ~0° no matter how hard you are cornering. That is also why the
lean is not felt from the saddle.

GoPro's `GRAV` stream is a gyro-dominant fusion estimate of *true* gravity, so
it does not suffer this. Measured against the low-passed accelerometer
direction on a mountain road the two diverge by up to **34.8°**, and the
divergence matches the computed lean almost exactly — at t=263.8 s, lean 34.2°
against a divergence of 34.0°. The accelerometer is bike-aligned; `GRAV` knows
which way is down.

Three things are calibrated out, all from the data itself:

- **Mount attitude.** `GRAV` is in the camera's frame, and the camera is
  neither level nor square to the bike. The upright reference is the mean
  gravity direction over *straight-line riding* (>5 m/s, turn rate <2°/s),
  where the bike genuinely is vertical.
- **Which way is sideways.** Lean sweeps gravity along one direction in the
  camera frame; the principal component of the deviations from upright recovers
  that lateral axis. Sign is fixed against the gyro's yaw rate, so positive
  lean means a right-hand turn.
- **Which way is forward.** The remaining axis is forward up to a sign, settled
  by requiring longitudinal acceleration to agree with GPS dv/dt.

Note `GRAV` uses a different axis convention from `ACCL`/`GYRO`: those carry an
`ORIN` tag of `ZXY` and must be permuted, and `GRAV`'s Y and Z are additionally
swapped relative to them. `ACCL` then reads 9.800 m/s² on a stationary clip,
which is how the scaling was confirmed.

## Accuracy

Cross-checked against three estimates that share no inputs with `GRAV` — the
geometric one from GPS speed and gyro yaw rate (tan θ = vω/g), the load-based
one from accelerometer magnitude (cos θ = g/|a|), and lateral load against
tan θ:

| comparison | correlation | bias | median error |
|---|---|---|---|
| whole ride, v > 21.6 km/h, vs *vω/g* | r = 0.965 | — | 2.1° |
| steady corners, vs *vω/g* | r = 0.857 | +0.5° | 2.1° |
| steady corners, vs *g/\|a\|* | r = 0.538 | −0.4° | 4.1° |
| steady corners, tan θ vs lateral g | r = 0.808 | 0.369 vs 0.339 | — |

Bias is under half a degree against both angle checks, so lean is good to a
couple of degrees. The weaker `g/|a|` correlation is that method's own noise —
`|a|` also picks up braking, drive and road vibration — not `GRAV`'s. Peak
`accel_total_g` of 1.332 g matches g/cos(41.0°) = 1.325 g independently.

Near-zero bias also shows the camera rolls with the bike; a helmet mount would
counter-lean and appear here as a systematic offset.

Peaks are the least reliable part: a hairpin apex is not steady-state, and the
methods spread by several degrees there (41.0° / 35.1° / 31.9° at the 32 km/h
apex quoted above). Summary loads are quoted at p99.5 rather than max for the
same reason.

## GPS gaps

When the GPS loses lock the camera keeps emitting 10 Hz samples with the last
known position **frozen** and a DOP sentinel of 99.99 — one clip repeats an
altitude of 100.093 m and a speed of 14.362 km/h for minutes. Unmasked, those
stale points corrupt distance and speed, and clips that never got a fix report
lat/lon of exactly 0 (one produced a "distance" of 8167 km). The script masks
them, leaves the GPS columns empty, reports fix coverage, and never bridges a
gap when integrating distance. Lean and the IMU are unaffected — they work in
clips with no GPS at all.

Raw GPS altitude is too noisy to differentiate directly; climb comes from a 5 s
smoothed profile with a 1 m threshold, without which a ride inside a 90 m
altitude band claimed 864 m of climb. Smoothing and accumulation both stop at a
coverage gap — altitudes either side of a drop-out are unrelated, and bridging
them gave one clip 2850 m of climb inside a 458 m band.

## Telemetry in these recordings

| clip | start (UTC) | fix | distance | max speed | max lean |
|---|---|---|---|---|---|
| `GS010522` | 08:41:15 | 100% | 27.11 km | 132.1 km/h | 41.0° |
| `GS020522` | 09:06:52 | 100% | 5.36 km | 106.8 km/h | 37.2° |
| `GS010523` | 09:40:13 | 100% | 23.80 km | 110.3 km/h | 40.5° |
| `GS020523` | 10:05:50 | 100% | 28.85 km | 162.4 km/h | 43.2° |
| `GS030523` | 10:31:27 | 100% | 10.97 km | 181.4 km/h | 32.5° |
| `GS010527` | 11:06:47 | 55% | 15.25 km | 159.0 km/h | 42.4° |
| `GS020527` | 11:32:24 | 73% | 20.65 km | 155.1 km/h | 31.9° |
| `GS030527` | 11:58:02 | 46% | 3.63 km | 102.1 km/h | 36.2° |
| `GS010526` | 11:05:38 | none | — | — | 0.2° (stationary) |

`max lean` is the deeper of the two directions. Clips with partial fix cover
only the located part of the ride; lean is unaffected by GPS.

Top speeds are sustained, not GPS spikes — p99.9 sits within 3 km/h of max.

# Merging a phone ride log — `merge-ride-telemetry.py`

A MotionRecord / Kurvenfokus export (`*_kurvenfokus.json`) records the same ride
from the rider's phone: 1 Hz GPS plus a calibrated IMU lean angle.
`merge-ride-telemetry.py` folds a camera telemetry CSV into that JSON and writes
a single GPX carrying **both** measurement sets.

```sh
./merge-ride-telemetry.py 66e6c51d_kurvenfokus.json GS010527.telemetry.csv
./merge-ride-telemetry.py ride.json ride.telemetry.csv --csv-mode mean
./merge-ride-telemetry.py ride.json ride.telemetry.csv --time-offset -4.5
```

```
66e6c51d_kurvenfokus.merged.gpx  (3549 points, 59.1 min)
  json      66e6c51d_kurvenfokus.json  11:09:42.529Z .. 12:08:51.218Z
  csv       GS010527.telemetry.csv     11:06:42.612Z .. 11:32:18.579Z  (46080 rows, offset -4.5 s)
  merged    1356/3549 points carry CSV data (38%), mode nearest
  alignment median track separation 42.3 m over 145 sampled points
```

The JSON is the **source**: it alone decides which points exist, and their
position, time and elevation. The CSV is matched onto those points by absolute
UTC. Nothing is overwritten and nothing is dropped — where both devices measured
the same quantity, both numbers survive under their own name.

## Field naming

Phone channels go in an `mr:` namespace, camera channels keep the `gt:` names
`gopro-telemetry.py` already uses, so the redundant pairs sit side by side:

| quantity | phone | camera |
|---|---|---|
| position | `mr:gpsLat` `mr:gpsLon` | `gt:lat` `gt:lon` |
| altitude | `mr:gpsHeight` | `gt:alt_m` |
| speed | `mr:gpsSpeed` (m/s) | `gt:speed_kmh` `gt:speed_3d_kmh` |
| heading | `mr:gpsTrueCourse` | `gt:course_deg` |
| lean | `mr:gyroLeaningAngle` | `gt:lean_deg` |
| acceleration | `mr:accX` `mr:accY` `mr:accZ` (device frame, gravity removed) | `gt:accel_lon_g` `gt:accel_lat_g` `gt:accel_vert_g` |

Standard GPX fields — `<ele>`, `<time>`, `gpxtpx:speed`/`course` — are filled
from the JSON and rounded for readability; the unrounded originals remain under
`mr:`, which is written at full round-trip precision (epoch timestamps keep
their milliseconds). Every CSV column reaches the output, including the ones
with no phone counterpart (`gt:pitch_deg`, `gt:yaw_rate_dps`, `gt:gps_dop`,
`gt:imu_temp_c`, `gt:time_s` for syncing back to the video). Each match also
records where it came from: `gt:csv_utc` and `gt:match_dt_s`. The JSON's `ride`
and `hardware` blocks, and the merge parameters, land in `<metadata>`.

Blank CSV cells are simply not emitted, so a point matched to a row without a
fix carries the camera's IMU channels and no camera GPS.

## Clock alignment

The two devices keep their own clocks, and taking their UTC at face value leaves
the tracks a median **99 m** apart. The script estimates the shift that best
lines the camera's GPS track up with the phone's — a scan over ±30 s minimising
median separation — before matching anything. On these files it finds **−4.5 s**
and separation drops to 42 m:

```
offset   -6.0    -5.0    -4.5    -4.0    -3.0     0.0    +2.0
sep       51.8 m  45.5 m  42.3 m  46.9 m  56.2 m  99.3 m  128.4 m
```

The minimum is unambiguous. The 42 m that remain are phone GPS lag and noise,
not a residual offset — estimated per third of the overlap the best shift wanders
between −2.6 s and −5.0 s, which is the accuracy this method has against a 1 Hz
track. `--time-offset S` sets the shift by hand, `--no-auto-offset` assumes the
clocks agree, `--offset-span` and `--offset-step` control the search.

## Coverage

A clip is shorter than a ride, so most points get no camera data — the script
says how many did. For this pair the CSV covers 11:06–11:32 against the JSON's
11:09–12:08, giving **1356 of 3549 points (38%)**; the other 2193 keep their
phone channels alone. Of the matched points, 718 carry camera GPS as well —
`GS010527` had a fix on only 55% of its rows.

Matches further than `--max-dt` from a point (default 0.5 s) are dropped rather
than stretched.

## Averaging instead of sampling

The CSV runs at 30 Hz against the JSON's 1 Hz, so the default `nearest` mode
throws away 29 of every 30 rows. `--csv-mode mean` instead averages each column
over the interval the point owns (midpoint to midpoint), circularly for
`course_deg`, and adds `_min`/`_max` for lean, speed and the three
accelerations — the peak of a corner is exactly what a single nearest sample
misses. Those points also carry `gt:csv_samples`, `gt:csv_utc_first` and
`gt:csv_utc_last`. It costs about 20% more file size (3.2 → 3.9 MB here).

## What the two devices disagree about

Worth knowing before trusting either column, over the merged points
(1356 for lean, 718 for the GPS channels):

| channel | agreement |
|---|---|
| speed | r = 0.88, median difference 5.0 km/h (max 169.1 phone / 158.5 camera) |
| altitude | phone reads a median **431 m** higher — MSL against the camera's ellipsoidal figure, and the spread of 5–818 m says neither is a clean geoid offset |
| lean | r = 0.20, median difference 13.9° |

The lean disagreement is mostly the merge, not the sensors: lean swings 19.6°
in a second at the 90th percentile, so a ±0.5 s match window and a couple of
seconds of clock uncertainty are worth more than the difference being measured.
The phone also reaches exactly ±55.0°, which looks like a clamp, against the
camera's −35.5…+40.3° over the same stretch. For lean, prefer `gt:lean_deg`
(see *How lean angle is recovered*); the phone's value is kept for comparison.

# Cutting highlight clips — `extract-highlights.py`

`extract-highlights.py` takes the ranked moments that
[`motion-record-analysis`](https://github.com/jophj/motion-record-analysis)
finds in a phone ride log and cuts the matching seconds out of a `.360`.

```sh
./extract-highlights.py 66e6c51d_kurvenfokus.json GS010527.360
./extract-highlights.py ride.json RIDE.360 --metric lean,curves --top 5
./extract-highlights.py ride.json RIDE.360 --pre 8 --post 6 --dry-run
./extract-highlights.py ride.json RIDE.360 --stitch
```

```
  ride       2026-09-08T11:09:42.529Z .. 2026-09-08T12:08:51.218Z
  video      2026-09-08T11:06:47.112Z .. 2026-09-08T11:32:24.648Z  (0:25:37.536)
  clock      phone -> camera +0.80 s  (measured from lean angle, r=-0.869 over 1346 samples)
  window     -6s / +4s around each moment

CLIPS  13 from 29 in-video moments (21 outside)
   0:15:18.916   28.0s  lean #1      11:22:11.227   +55.0 deg right (at sensor limit)  (+2 more)
                       also  lean #5      11:22:29.230   +53.7 deg right
                       also  accel #3     11:22:23.226   6.80 m/s2 (0.69 g)
```

Clips are cut with `ffmpeg -c copy`, so a 13-clip / 1.65 GB pass over an 8.5 GB
file takes **4 seconds** and loses nothing. Every stream survives — both HEVC
cube-face tracks, both AAC tracks, the 4-channel ambisonic PCM and the `gpmd`
telemetry — so a clip is still a working `.360`: `gopro-telemetry.py` reads its
telemetry and `gopromax2-stitch.sh` stitches it like any other file.

Two container details make that work. The `mov` muxer is used rather than `mp4`,
because `mp4` rejects the ambisonic track (`unsupported channel layout
ambisonic 1`); the `.360` extension is kept so the rest of the toolchain still
recognises the file. And the `tmcd` timecode track is dropped, because it cannot
be remuxed (`codec none`) — its stream index is discovered per file, never
assumed, for the same reason the second video track is.

## Aligning the two clocks

The JSON's timestamps are on the phone's clock and the cut has to happen on the
camera's. The offset is measured by correlating **lean angle**, the one quantity
both devices record with high dynamic range:

| method | offset | quality |
|---|---|---|
| lean angle correlation | **+0.80 s** | \|r\| = 0.87, peak a few tenths wide |
| lean corner-peak matching | +0.72 s | 114 corners, sd 1.8 s |
| GPS speed | −3.3 s | shallow, 3.7 km/h residual floor |
| GPS track separation | −4.8 s | shallow, 43 m residual floor |

Position and speed are poor for this and the first two rows are why: two GPS
receivers disagree by tens of metres and a few km/h *regardless* of alignment,
so the minimum is shallow and lands seconds away from the truth. Lean has no
such floor. Note this means `merge-ride-telemetry.py --auto-offset`, which
aligns on position, reports −4.5 s here and is about 5 s wrong — pass
`--time-offset 0.8` to it instead.

The correlation is deliberately **sign-agnostic**, seeking the peak in
`|r|`, because the two lean conventions disagree — see below.

Verified against each clip's own embedded telemetry: every ranked lean moment
lands within **2.3 s** of its predicted position, inside the 1.8 s
corner-to-corner scatter between the two devices and well inside the 6 s of
run-in. Keyframes are 1.001 s apart, so a stream copy costs at most one more
second of pre-roll.

## Levelling clips

Horizon levelling must **not** be left to the individual clip.
`gopromax2-stitch.sh` averages gravity over whatever it is given, and a
highlight is usually all corner, so its "upright" reference is wrong: a 10 s clip
of one bend levels to `roll 13.3°` where the parent file gives `1.9°`. `--stitch`
therefore measures the rotation once from the whole file with
`--print-rotation` and passes it to every clip via `--rotate`. Doing it by hand:

```sh
rot=$(./gopromax2-stitch.sh --print-rotation GS010527.360 | tail -1)
./gopromax2-stitch.sh --rotate "$rot" GS010527_highlights/GS010527_lean01_55deg_t0918.360
```

## Clips, and what falls outside them

Overlapping moments are merged (`--merge-gap`, default 2 s), so the three
metrics that all peak in one bend produce one clip rather than three
near-identical files — 50 ranked moments became 29 in-video ones and 13 clips
here. The clip is named after its highest-priority contributor
(lean, then curves, brake, accel, speed) and a `*_highlights.json` manifest
records every contributor with its timestamp, value and position.

A recording is chaptered, so **one `.360` covers only part of a ride**:
`GS010527` spans 25:37 of a 59:09 ride, and 21 of the 50 ranked moments fall
after it. Those are listed explicitly rather than silently dropped, and the
remaining chapters (`GS020527`, `GS030527`) hold them.

## The lean sign disagreement

`gyroLeaningAngle` in the phone JSON and `lean_deg` from `gopro-telemetry.py`
have **opposite signs**. Checked against GPS course rate, which is independent
of both and unambiguous about which way a turn goes:

| signal | vs GPS course rate | positive lean means |
|---|---|---|
| `mr:gyroLeaningAngle` (phone) | r = **+0.52** | right turn ✓ |
| `gt:lean_deg` (camera) | r = **−0.33** | left turn ✗ |
| `gt:lean_deg` vs camera's own `yaw_rate_dps` | r = +0.95 | self-consistent |

So the phone has it right and `gopro-telemetry.py` is inverted. The last row is
the cause: its calibration fixes the lean sign against the gyro's yaw rate
(*Which way is sideways*, above), and that axis is itself flipped, so the lean
inherits the flip. It is self-consistent and only wrong in absolute terms —
magnitudes, and everything derived from them, are unaffected.

This also revises *What the two devices disagree about*: lean measures r = 0.20
there because the merge used a position-derived offset ~5 s out **and** summed
two inverted signals. Aligned on lean and corrected for sign, the same two
series agree at **|r| = 0.87**. The magnitude gap is real and remains — over
these clips the phone reads 51–55° (clamping at exactly ±55.0°) where the camera
reads 30–42° for the same corners.
