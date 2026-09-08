#!/usr/bin/env bash
#
# gopromax2-stitch.sh -- convert GoPro MAX 2 (.360) footage to standard
#                        equirectangular video using only stock ffmpeg.
#
# A .360 file is not a normal video: it holds TWO HEVC tracks, each carrying
# three faces of an in-camera-stitched equi-angular cubemap (EAC).  Players and
# NLEs cannot read it.  This script reassembles the six cube faces, blends the
# two fisheye overlap zones, and reprojects to equirectangular at the source's
# native angular resolution, so nothing is thrown away.
#
# See README.md for the reverse-engineered frame layout.
#
set -euo pipefail

# ---------------------------------------------------------------- defaults ---
QUALITY=high            # high | max | prores | h264
AUDIO=stereo            # stereo | ambisonic | both | none
LEVEL=1                 # horizon-level from the GPMF gravity vector
YAW=0; PITCH=; ROLL=    # manual orientation override (degrees)
OUT_W=; OUT_H=          # default: native angular resolution
OVERSAMPLE=1            # multiply native output resolution
INTERP=lanczos
TELEMETRY=1             # copy the GPMF telemetry track through
START=; DURATION=
PREVIEW=0               # write a single JPEG instead of a video
PRINT_ROT=0             # print the computed yaw:pitch:roll and exit
JOIN=0                  # concatenate GS02…/GS03… chapters of the same clip
OUTPUT=
VT_Q=65                 # hevc_videotoolbox quality (0-100); ~50 Mb/s at 3840x1920
X265_CRF=16
JOBS=

self=$(basename "$0")

die()  { printf '%s: error: %s\n' "$self" "$*" >&2; exit 1; }
note() { printf '%s: %s\n' "$self" "$*" >&2; }

usage() {
  cat >&2 <<EOF
Usage: $self [options] INPUT.360 [OUTPUT]

Reprojects GoPro MAX 2 .360 footage to equirectangular video.

Output geometry
  -s, --size WxH        output resolution (default: native angular res,
                        i.e. 4 x cube-face size, e.g. 3840x1920)
  -O, --oversample N    render N x native resolution (e.g. 1.5) to minimise
                        resampling loss when grading/reframing afterwards
      --interp NAME     v360 interpolation: near, line, cubic, lanczos,
                        spline16 (default: $INTERP)

Orientation
      --level           level the horizon using the GPMF gravity vector (default)
      --no-level        keep the camera-native orientation
      --rotate Y:P:R    explicit yaw:pitch:roll in degrees (implies --no-level)
      --yaw DEG         extra yaw applied after levelling (default 0)
      --print-rotation  print the computed yaw:pitch:roll and exit, so every
                        chapter of one recording can share it via --rotate

Encoding
  -q, --quality PRESET  high    hevc_videotoolbox 10-bit, hardware  (default)
                        max     libx265 CRF $X265_CRF 10-bit, preset slow (slow)
                        prores  ProRes 422 HQ, for NLE round-tripping
                        h264    libx264 CRF 18, maximum compatibility
      --vt-quality N    hevc_videotoolbox quality 0-100 (default $VT_Q)
      --crf N           libx265/libx264 CRF (default $X265_CRF / 18)
  -j, --jobs N          ffmpeg filter threads (default: auto)

Audio / metadata
  -a, --audio MODE      stereo (default) | ambisonic | both | none
                        ambisonic keeps the 4-channel first-order ambisonic
                        PCM track and forces a .mov container
      --no-telemetry    drop the GPMF telemetry track

                        Capture time is preserved automatically: UTC in
                        creation_time, local time plus UTC offset in
                        Keys:CreationDate and XMP, and the local wall clock in
                        a tmcd timecode track.  The local time and offset need
                        exiftool; without it only UTC survives.
Miscellaneous
      --ss TIME         start offset
  -t, --duration TIME   stop after TIME
      --preview         write a single equirectangular JPEG and exit
      --join            also process GS02…/GS03… chapters of the same clip
                        and concatenate them into one output
  -h, --help            this help

Examples
  $self GS010526.360
  $self --preview GS010526.360
  $self -q max -a both --join GS010527.360 ride.mov
EOF
  exit 1
}

# ------------------------------------------------------------------ parsing --
while [ $# -gt 0 ]; do
  case $1 in
    -s|--size)       IFS=x read -r OUT_W OUT_H <<<"${2:-}"; shift 2;;
    -O|--oversample) OVERSAMPLE=${2:-1}; shift 2;;
    --interp)        INTERP=${2:-}; shift 2;;
    --level)         LEVEL=1; shift;;
    --no-level)      LEVEL=0; shift;;
    --rotate)        IFS=: read -r YAW PITCH ROLL <<<"${2:-}"; LEVEL=0; shift 2;;
    --yaw)           YAW=${2:-0}; shift 2;;
    -q|--quality)    QUALITY=${2:-}; shift 2;;
    --vt-quality)    VT_Q=${2:-}; shift 2;;
    --crf)           X265_CRF=${2:-}; shift 2;;
    -j|--jobs)       JOBS=${2:-}; shift 2;;
    -a|--audio)      AUDIO=${2:-}; shift 2;;
    --no-telemetry)  TELEMETRY=0; shift;;
    --ss)            START=${2:-}; shift 2;;
    -t|--duration)   DURATION=${2:-}; shift 2;;
    --preview)       PREVIEW=1; shift;;
    --print-rotation) PRINT_ROT=1; shift;;
    --join)          JOIN=1; shift;;
    -h|--help)       usage;;
    -*)              die "unknown option: $1";;
    *)               if [ -z "${INPUT:-}" ]; then INPUT=$1; else OUTPUT=$1; fi; shift;;
  esac
done
[ -n "${INPUT:-}" ] || usage
[ -f "$INPUT" ]     || die "no such file: $INPUT"

command -v ffmpeg  >/dev/null || die "ffmpeg not found (brew install ffmpeg)"
command -v ffprobe >/dev/null || die "ffprobe not found"
command -v python3 >/dev/null || { [ "$LEVEL" = 1 ] && die "python3 needed for --level"; }

case $AUDIO in stereo|ambisonic|both|none) ;; *) die "bad --audio: $AUDIO";; esac
case $QUALITY in high|max|prores|h264) ;; *) die "bad --quality: $QUALITY";; esac

# ---------------------------------------------------------------- probing ----
# The two EAC tracks are the only video streams; their index differs between
# capture modes (0+5 for plain video, 0+4 when there is no second audio track),
# so never hard-code them.
probe_streams() {
  local f=$1 idx
  # ffprobe lists each stream twice on .360 files (once via the track-reference
  # stream group), so filter to numeric values and de-duplicate.
  VID_IDX=()
  for idx in $(ffprobe -v error -select_streams v -show_entries stream=index \
                 -of csv=p=0 "$f" | sed 's/,.*$//' | grep -E '^[0-9]+$' | sort -n -u); do
    VID_IDX[${#VID_IDX[@]}]=$idx
  done
  VID_W=$(ffprobe -v error -select_streams v:0 -show_entries stream=width \
            -of csv=p=0 "$f" | sed 's/,.*$//' | grep -E '^[0-9]+$' | head -1)
  VID_H=$(ffprobe -v error -select_streams v:0 -show_entries stream=height \
            -of csv=p=0 "$f" | sed 's/,.*$//' | grep -E '^[0-9]+$' | head -1)
  [ ${#VID_IDX[@]} -ge 2 ] || die "$f: expected 2 video tracks, found ${#VID_IDX[@]} -- not a .360 file?"
  [ -n "$VID_W" ] && [ -n "$VID_H" ] || die "$f: could not read track dimensions"

  # Frame layout: [ half-face+overlap | full face | half-face+overlap ]
  #   face size F = track height
  #   total overlap OVL = (W - 3F) / 2   (half of it each side of the centre line)
  FACE=$VID_H
  OVL=$(( (VID_W - 3*FACE) / 2 ))
  [ $(( 3*FACE + 2*OVL )) -eq "$VID_W" ] || \
    die "$f: ${VID_W}x${VID_H} does not match the GoPro EAC layout (3*H + 2*overlap)"
  [ "$OVL" -gt 0 ] && [ $((OVL % 2)) -eq 0 ] || die "$f: implausible overlap width $OVL"
  SEG=$(( FACE/2 - OVL/2 ))     # face pixels stored before the overlap band
  PANEL=$(( FACE/2 + OVL/2 ))   # stored width of one half of a split face

  AAC_IDX=$(ffprobe -v error -select_streams a -show_entries stream=index,codec_name \
              -of csv=p=0 "$f" | awk -F, '$1~/^[0-9]+$/ && $2=="aac"{print $1; exit}')
  AMB_IDX=$(ffprobe -v error -select_streams a -show_entries stream=index,channels \
              -of csv=p=0 "$f" | awk -F, '$1~/^[0-9]+$/ && $2+0>=4{print $1; exit}')
  GPMD_IDX=$(ffprobe -v error -show_entries stream=index,codec_tag_string \
              -of csv=p=0 "$f" | awk -F, '$1~/^[0-9]+$/ && $2=="gpmd"{print $1; exit}')
  FPS=$(ffprobe -v error -select_streams v:0 -show_entries stream=r_frame_rate \
          -of csv=p=0 "$f" | sed 's/,.*$//' | grep -E '^[0-9]+/[0-9]+$' | head -1 \
        | awk -F/ '{printf "%.3f", $1/$2}')
}

probe_streams "$INPUT"

if [ -z "$OUT_W" ]; then
  # native angular resolution: the EAC samples 90 deg per face, so a full
  # 360 deg turn is exactly 4 faces wide.  No detail gained or lost.
  OUT_W=$(( FACE * 4 )); OUT_H=$(( FACE * 2 ))
fi
if [ "$OVERSAMPLE" != 1 ]; then
  OUT_W=$(python3 -c "print(int(round($OUT_W*$OVERSAMPLE/2))*2)")
  OUT_H=$(python3 -c "print(int(round($OUT_H*$OVERSAMPLE/2))*2)")
fi

# --------------------------------------------------- horizon levelling -------
# GPMF carries GRAV, the gravity direction in camera axes, which coincide with
# the cube axes (+Z = first track's centre face).  Rotating the sphere so that
# gravity lands on -Y puts the horizon where it belongs.
compute_level() {
  local f=$1 tmp
  [ -n "$GPMD_IDX" ] || { note "no GPMF track; skipping horizon levelling"; return 1; }
  tmp=$(mktemp -t gpmd) || return 1
  ffmpeg -v error -y -noautorotate -i "$f" -map "0:$GPMD_IDX" -c copy -f data "$tmp" \
    || { rm -f "$tmp"; return 1; }
  local res
  res=$(python3 - "$tmp" <<'PY'
import struct, sys, math
d = open(sys.argv[1], 'rb').read()
grav = []
def walk(off, end, ctx):
    while off + 8 <= end:
        key = d[off:off+4]; t = d[off+4]; ss = d[off+5]
        rep = struct.unpack('>H', d[off+6:off+8])[0]
        ln = ss*rep; pad = (4 - ln % 4) % 4; pl = d[off+8:off+8+ln]
        if t == 0:
            walk(off+8, off+8+ln, {})
        elif key == b'SCAL':
            ctx['scal'] = struct.unpack('>%dh' % (ln//2), pl)
        elif key == b'GRAV' and ln >= 6:
            s = ctx.get('scal', (1,))[0] or 1
            for i in range(ln//6):
                grav.append(tuple(v/s for v in struct.unpack('>3h', pl[i*6:i*6+6])))
        off += 8 + ln + pad
        if ln == 0 and ss == 0 and rep == 0:
            break
walk(0, len(d), {})
if not grav:
    sys.exit(1)
n = len(grav)
gx, gy, gz = (sum(g[i] for g in grav)/n for i in range(3))
L = math.hypot(math.hypot(gx, gy), gz)
if L < 0.5:
    sys.exit(1)
gx, gy, gz = gx/L, gy/L, gz/L
# want R(yaw)R(pitch)R(roll) . (0,1,0) == -gravity  (v360's ypr convention)
roll = math.degrees(math.asin(max(-1.0, min(1.0, -gx))))
pitch = math.degrees(math.atan2(gz, -gy))
print('%.4f %.4f %d' % (pitch, roll, n))
PY
  ) || { rm -f "$tmp"; note "could not read GRAV; skipping horizon levelling"; return 1; }
  rm -f "$tmp"
  read -r PITCH ROLL NSAMP <<<"$res"
  note "horizon levelling from $NSAMP gravity samples: pitch=$PITCH roll=$ROLL"
  return 0
}

if [ "$LEVEL" = 1 ]; then
  compute_level "$INPUT" || { PITCH=0; ROLL=0; }
fi
PITCH=${PITCH:-0}; ROLL=${ROLL:-0}; YAW=${YAW:-0}

if [ "$PRINT_ROT" = 1 ]; then
  printf '%s:%s:%s\n' "$YAW" "$PITCH" "$ROLL"
  exit 0
fi

# ------------------------------------------------------- source timestamps ---
# Re-encoding loses the capture time twice over.  ffmpeg does not carry
# container `creation_time` across unless asked (-map_metadata 0), and the GoPro
# `udta` block holding the *local* time, the UTC offset and the camera identity
# is proprietary, so ffmpeg cannot copy it at any setting.  Capture both up
# front, then re-apply after encoding.  The GPMF telemetry track survives on its
# own and still carries GPS time, but that is UTC only and needs a parser.
read_times() {                 # $1=source
  local f=$1 cd tz
  SRC_CT=$(ffprobe -v error -show_entries format_tags=creation_time \
             -of default=nw=1:nk=1 "$f" 2>/dev/null | head -1)
  SRC_TC=$(ffprobe -v error -select_streams d -show_entries stream_tags=timecode \
             -of default=nw=1:nk=1 "$f" 2>/dev/null | head -1)
  SRC_LOCAL=; SRC_MODEL=
  if command -v exiftool >/dev/null; then
    cd=$(exiftool -q -s3 -GoPro:CreationDate "$f" 2>/dev/null | head -1)
    tz=$(exiftool -q -s3 -GoPro:TimeZone     "$f" 2>/dev/null | head -1)
    SRC_MODEL=$(exiftool -q -s3 -GoPro:Model "$f" 2>/dev/null | head -1)
    [ -n "$cd" ] && SRC_LOCAL="$cd${tz}"
  fi
}

# ------------------------------------------------------------ filter graph ---
# Rebuild one 960x960 (FACE x FACE) cube face out of a split face.  The face is
# stored as two panels; each carries OVL/2 extra pixels past the fisheye centre
# line, so the two panels overlap by OVL pixels of duplicated angle.  Cross-fade
# them linearly to hide the stitch.
split_face() {                 # $1=track $2=first split slot $3=base x $4=label
  local t=$1 s=$2 b=$3 o=$4
  printf '[t%s_%s]crop=%d:%d:%d:0[%sa];'   "$t" "$((s+0))" "$SEG"  "$FACE" "$b"                    "$o"
  printf '[t%s_%s]crop=%d:%d:%d:0[%soA];'  "$t" "$((s+1))" "$OVL"  "$FACE" "$((b+SEG))"            "$o"
  printf '[t%s_%s]crop=%d:%d:%d:0[%soB];'  "$t" "$((s+2))" "$OVL"  "$FACE" "$((b+PANEL))"          "$o"
  printf "[%soA][%soB]blend=all_expr='A*(1-(X+0.5)/%d)+B*((X+0.5)/%d)'[%so];" "$o" "$o" "$OVL" "$OVL" "$o"
  printf '[t%s_%s]crop=%d:%d:%d:0[%sb];'   "$t" "$((s+3))" "$SEG"  "$FACE" "$((b+PANEL+OVL))"      "$o"
  printf '[%sa][%so][%sb]hstack=3[%s];' "$o" "$o" "$o" "$o"
}

build_graph() {
  local mid=$(( 2*PANEL ))                 # x offset of the complete centre face
  local right=$(( 2*PANEL + FACE ))        # x offset of the second split face
  # 4:4:4 keeps chroma at full resolution through the geometric transform
  printf '[0:%s]format=yuv444p,split=9[t0_0][t0_1][t0_2][t0_3][t0_4][t0_5][t0_6][t0_7][t0_8];' "${VID_IDX[0]}"
  printf '[0:%s]format=yuv444p,split=9[t1_0][t1_1][t1_2][t1_3][t1_4][t1_5][t1_6][t1_7][t1_8];' "${VID_IDX[1]}"
  split_face 0 0 0       FL                # track 0 = cubemap strip [left front right]
  printf '[t0_4]crop=%d:%d:%d:0[FF];' "$FACE" "$FACE" "$mid"
  split_face 0 5 "$right" FR
  split_face 1 0 0       FD                # track 1 = cubemap strip [down back up]
  printf '[t1_4]crop=%d:%d:%d:0[FB];' "$FACE" "$FACE" "$mid"
  split_face 1 5 "$right" FU
  printf '[FL][FF][FR]hstack=3[row0];'
  printf '[FD][FB][FU]hstack=3[row1];'
  printf '[row0][row1]vstack=2[eac];'      # ffmpeg's 3x2 EAC face order
  printf '[eac]v360=input=eac:output=e:w=%d:h=%d:interp=%s:yaw=%s:pitch=%s:roll=%s[vout]' \
         "$OUT_W" "$OUT_H" "$INTERP" "$YAW" "$PITCH" "$ROLL"
}

# --------------------------------------------------------------- encoding ----
video_opts() {
  case $QUALITY in
    high)   printf -- '-c:v hevc_videotoolbox -q:v %s -pix_fmt p010le -profile:v main10 -tag:v hvc1' "$VT_Q";;
    max)    printf -- '-c:v libx265 -crf %s -preset slow -pix_fmt yuv420p10le -tag:v hvc1 -x265-params log-level=error' "$X265_CRF";;
    prores) printf -- '-c:v prores_ks -profile:v 3 -vendor apl0 -pix_fmt yuv422p10le';;
    h264)   printf -- '-c:v libx264 -crf 18 -preset slow -pix_fmt yuv420p';;
  esac
}

# Container: ProRes and 4-channel ambisonic PCM are only valid in .mov
container_ext() {
  case $QUALITY in prores) echo mov; return;; esac
  case $AUDIO in ambisonic|both) echo mov; return;; esac
  echo mp4
}

convert_one() {               # $1=input  $2=output
  local in=$1 out=$2 graph maps=() aopts=() mopts=()
  probe_streams "$in"
  read_times "$in"
  graph=$(build_graph)

  # -map_metadata 0 restores creation_time and the track/media dates, which
  # ffmpeg otherwise zeroes to 0000:00:00.  -timecode re-creates the tmcd track
  # carrying the local wall-clock start, which NLEs use to sync.
  # EMIT_TC is 0 for the intermediate parts of a --join: a tmcd track cannot be
  # re-muxed by the concat step's -c copy ("no tag for codec none"), so the
  # timecode is written once, onto the final file.
  mopts+=(-map_metadata 0)
  [ -n "${SRC_TC:-}" ] && [ "${EMIT_TC:-1}" = 1 ] && mopts+=(-timecode "$SRC_TC")

  maps+=(-map '[vout]')
  case $AUDIO in
    stereo)    [ -n "$AAC_IDX" ] && { maps+=(-map "0:$AAC_IDX"); aopts+=(-c:a copy); };;
    ambisonic) [ -n "$AMB_IDX" ] && { maps+=(-map "0:$AMB_IDX"); aopts+=(-c:a copy); } || note "no ambisonic track";;
    both)      [ -n "$AAC_IDX" ] && maps+=(-map "0:$AAC_IDX")
               [ -n "$AMB_IDX" ] && maps+=(-map "0:$AMB_IDX")
               aopts+=(-c:a copy);;
    none)      aopts+=(-an);;
  esac
  if [ "$TELEMETRY" = 1 ] && [ -n "$GPMD_IDX" ]; then
    maps+=(-map "0:$GPMD_IDX" -c:d copy -copy_unknown)
  fi

  ffmpeg -hide_banner -noautorotate \
    ${JOBS:+-filter_threads "$JOBS"} \
    ${START:+-ss "$START"} ${DURATION:+-t "$DURATION"} \
    -i "$in" \
    -filter_complex "$graph" \
    "${maps[@]}" \
    "${mopts[@]}" \
    $(video_opts) \
    -color_range pc -colorspace bt709 -color_primaries bt709 -color_trc bt709 \
    "${aopts[@]}" \
    -movflags +faststart \
    -y "$out"
}

# 360 players and YouTube need the projection declared in the container, and
# the capture time has to be written back (see read_times).  One exiftool pass
# does both, because each pass rewrites the whole file.
tag_output() {
  local out=$1 args=()
  if ! command -v exiftool >/dev/null; then
    note "exiftool not found -- output is NOT tagged as 360,"
    note "  and the local capture time cannot be restored."
    note "  install it with:  brew install exiftool   then re-run, or tag later."
    return
  fi
  args+=(-XMP-GSpherical:Spherical=true
         -XMP-GSpherical:Stitched=true
         -XMP-GSpherical:ProjectionType=equirectangular
         -XMP-GSpherical:StitchingSoftware="gopromax2-stitch.sh (ffmpeg)")
  if [ -n "${SRC_LOCAL:-}" ]; then
    # Keys:CreationDate is the atom Finder, QuickTime and Photos read, and the
    # only standard place that keeps the UTC offset alongside the local time.
    args+=("-QuickTime:CreationDate=$SRC_LOCAL"
           "-XMP-exif:DateTimeOriginal=$SRC_LOCAL"
           "-XMP-photoshop:DateCreated=$SRC_LOCAL")
  fi
  [ -n "${SRC_MODEL:-}" ] && args+=(-QuickTime:Make=GoPro "-QuickTime:Model=$SRC_MODEL")
  if exiftool -q -overwrite_original "${args[@]}" "$out"; then
    note "tagged as equirectangular 360${SRC_LOCAL:+, capture time $SRC_LOCAL}"
  else
    note "exiftool could not tag $out"
  fi
}

# ------------------------------------------------------------------- main ----
ext=$(container_ext)
base=${INPUT%.*}

# ProRes and 4-channel ambisonic PCM cannot be muxed into MP4.
if [ "$ext" = mov ] && [ -n "$OUTPUT" ] && [ "$PREVIEW" = 0 ]; then
  case $OUTPUT in
    *.mp4|*.MP4)
      note "$QUALITY/$AUDIO requires a .mov container; writing ${OUTPUT%.*}.mov"
      OUTPUT="${OUTPUT%.*}.mov";;
  esac
fi

if [ "$PREVIEW" = 1 ]; then
  out=${OUTPUT:-${base}_equirect.jpg}
  probe_streams "$INPUT"
  ffmpeg -hide_banner -loglevel error -noautorotate ${START:+-ss "$START"} -i "$INPUT" \
    -filter_complex "$(build_graph)" -map '[vout]' -frames:v 1 -q:v 2 -y "$out"
  note "wrote $out  (${OUT_W}x${OUT_H})"
  exit 0
fi

# chapter list: GoPro splits long recordings into GS01xxxx, GS02xxxx, ...
chapters=("$INPUT")
if [ "$JOIN" = 1 ]; then
  dir=$(dirname "$INPUT"); f=$(basename "$INPUT")
  if [[ $f =~ ^(G[A-Z])([0-9]{2})([0-9]{4})\.(360|MP4)$ ]]; then
    pre=${BASH_REMATCH[1]}; clip=${BASH_REMATCH[3]}; sfx=${BASH_REMATCH[4]}
    chapters=()
    for n in $(seq -w 1 99); do
      c="$dir/${pre}${n}${clip}.${sfx}"
      [ -f "$c" ] || continue
      if ffprobe -v error -select_streams v:0 -show_entries stream=index \
           -of csv=p=0 "$c" >/dev/null 2>&1; then
        chapters+=("$c")
      else
        note "skipping unreadable chapter $(basename "$c") (incomplete recording?)"
      fi
    done
    note "joining ${#chapters[@]} chapter(s): ${chapters[*]##*/}"
  else
    note "filename is not a GoPro chapter name; --join ignored"
  fi
fi

if [ ${#chapters[@]} -le 1 ]; then
  out=${OUTPUT:-${base}_equirect.$ext}
  convert_one "$INPUT" "$out"
  tag_output "$out"
  note "done: $out  (${OUT_W}x${OUT_H} equirectangular, $FPS fps)"
else
  out=${OUTPUT:-${base}_equirect.$ext}
  tmpdir=$(mktemp -d -t max2join)
  trap 'rm -rf "$tmpdir"' EXIT
  list="$tmpdir/list.txt"; : >"$list"
  EMIT_TC=0
  i=0
  for c in "${chapters[@]}"; do
    i=$((i+1))
    part="$tmpdir/part$i.$ext"
    note "[$i/${#chapters[@]}] $(basename "$c")"
    convert_one "$c" "$part"
    printf "file '%s'\n" "$part" >>"$list"
  done
  # Timestamps come from the first chapter -- that is when the recording began.
  EMIT_TC=1
  read_times "${chapters[0]}"
  # The concat demuxer exposes no global metadata, so -map_metadata 0 has
  # nothing to copy here; set creation_time from the first chapter explicitly.
  cmopts=(-map_metadata 0)
  [ -n "${SRC_CT:-}" ] && cmopts+=(-metadata "creation_time=$SRC_CT")
  [ -n "${SRC_TC:-}" ] && cmopts+=(-timecode "$SRC_TC")
  ffmpeg -hide_banner -loglevel error -f concat -safe 0 -i "$list" \
    -map 0 -c copy -copy_unknown "${cmopts[@]}" \
    -movflags +faststart -y "$out"
  tag_output "$out"
  note "done: $out  (${OUT_W}x${OUT_H} equirectangular, $FPS fps, ${#chapters[@]} chapters)"
fi
