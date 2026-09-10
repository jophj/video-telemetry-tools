//! Turning ranked moments into clip windows, chapters and an edit list.
//!
//! All three outputs need the same thing first: the moments flattened into
//! time-ordered windows, with overlapping ones merged so that two corners four
//! seconds apart become one clip rather than two clips of mostly the same
//! footage.

use super::{Analysis, Entry, MetricId};
use crate::math::fmt_num;

/// One ranked moment, flattened out of its metric.
#[derive(Debug, Clone)]
pub struct Moment {
    pub metric: MetricId,
    pub entry: Entry,
}

impl Moment {
    /// The instant to centre a clip on, in seconds from the start of the video.
    pub fn video_time_s(&self) -> Option<f64> {
        self.entry.video_time_s
    }

    /// A short description, for a chapter title or clip name.
    pub fn label(&self) -> String {
        match self.metric {
            MetricId::Lean => format!(
                "Lean #{} — {:.0}° {}",
                self.entry.rank,
                self.entry.value.abs(),
                if self.entry.direction == Some("left") { "L" } else { "R" }
            ),
            MetricId::Accel => format!("Accel #{} — {:.2} m/s²", self.entry.rank, self.entry.value),
            MetricId::Decel => format!("Brake #{} — {:.2} m/s²", self.entry.rank, self.entry.value),
            MetricId::Speed => format!("Speed #{} — {:.0} km/h", self.entry.rank, self.entry.value),
            MetricId::Curves => format!(
                "Curves #{} — {} in {:.0}s",
                self.entry.rank,
                self.entry.curve_count.unwrap_or(0),
                self.entry.span_s()
            ),
            MetricId::Lateral => {
                format!("Cornering #{} — {:.2} g", self.entry.rank, self.entry.value.abs())
            }
            MetricId::Total => format!("Load #{} — {:.2} g", self.entry.rank, self.entry.value),
        }
    }

    /// A filename-safe token describing the value.
    pub fn value_token(&self) -> String {
        match self.metric {
            MetricId::Lean => format!("{:.0}deg", self.entry.value.abs()),
            MetricId::Accel | MetricId::Decel => format!("{:.1}ms2", self.entry.value),
            MetricId::Speed => format!("{:.0}kmh", self.entry.value),
            MetricId::Curves => format!("{}curves", self.entry.curve_count.unwrap_or(0)),
            MetricId::Lateral | MetricId::Total => format!("{:.2}g", self.entry.value.abs()),
        }
    }

    pub fn slug(&self) -> &'static str {
        match self.metric {
            MetricId::Decel => "brake",
            MetricId::Curves => "curvy",
            other => other.slug(),
        }
    }
}

/// A window of video worth keeping.
#[derive(Debug, Clone)]
pub struct Clip {
    pub start_s: f64,
    pub end_s: f64,
    /// Every moment falling inside, most significant first.
    pub moments: Vec<Moment>,
}

impl Clip {
    pub fn duration_s(&self) -> f64 {
        self.end_s - self.start_s
    }

    /// The moment that names the clip: lowest metric priority, then best rank.
    pub fn head(&self) -> &Moment {
        &self.moments[0]
    }

    pub fn name(&self, stem: &str) -> String {
        let h = self.head();
        format!(
            "{stem}_{}{:02}_{}_t{:04}",
            h.slug(),
            h.entry.rank,
            h.value_token(),
            self.start_s as i64
        )
    }
}

#[derive(Debug, Clone)]
pub struct PlanOptions {
    /// Seconds of run-in before the moment.
    pub pre_s: f64,
    /// Seconds after it.
    pub post_s: f64,
    /// Merge clips that come within this of each other. Negative disables.
    pub merge_gap_s: f64,
    /// Refuse a clip shorter than this.
    pub min_duration_s: f64,
    /// Video length, to clamp against.
    pub duration_s: Option<f64>,
}

impl Default for PlanOptions {
    fn default() -> Self {
        PlanOptions {
            pre_s: 6.0,
            post_s: 4.0,
            merge_gap_s: 2.0,
            // The prototype had no floor, so a moment landing on the last frame
            // produced a zero-length clip and an ffmpeg invocation that wrote
            // an empty file.
            min_duration_s: 0.5,
            duration_s: None,
        }
    }
}

/// What could not be turned into a clip.
#[derive(Debug, Clone)]
pub struct Dropped {
    pub moment: Moment,
    pub reason: String,
}

/// Group the analysis's moments into clip windows.
pub fn plan(analysis: &Analysis, opts: &PlanOptions) -> (Vec<Clip>, Vec<Dropped>) {
    let mut dropped = Vec::new();
    let mut singles: Vec<Clip> = Vec::new();

    for metric in &analysis.metrics {
        for entry in &metric.entries {
            let moment = Moment {
                metric: metric.id,
                entry: entry.clone(),
            };
            let Some(at) = moment.video_time_s() else {
                dropped.push(Dropped {
                    moment,
                    reason: "no video timestamp: this source is not tied to a recording".into(),
                });
                continue;
            };
            // Keep a moment whose instant is in frame even when its padding
            // runs past the ends.
            if let Some(d) = opts.duration_s {
                if at < -0.001 || at > d {
                    dropped.push(Dropped {
                        moment,
                        reason: format!("at {at:.1} s, outside the video's {d:.1} s"),
                    });
                    continue;
                }
            }
            let span = moment.entry.span_s();
            let start = (at - opts.pre_s).max(0.0);
            let mut end = at + span + opts.post_s;
            if let Some(d) = opts.duration_s {
                end = end.min(d);
            }
            singles.push(Clip {
                start_s: start,
                end_s: end,
                moments: vec![moment],
            });
        }
    }

    if singles.is_empty() {
        return (Vec::new(), dropped);
    }

    // Merge overlapping windows.
    singles.sort_by(|a, b| a.start_s.partial_cmp(&b.start_s).unwrap());
    let mut merged: Vec<Clip> = Vec::new();
    for clip in singles {
        match merged.last_mut() {
            Some(prev)
                if opts.merge_gap_s >= 0.0 && clip.start_s <= prev.end_s + opts.merge_gap_s =>
            {
                prev.end_s = prev.end_s.max(clip.end_s);
                prev.moments.extend(clip.moments);
            }
            _ => merged.push(clip),
        }
    }

    // Order each clip's moments so the most significant names it.
    for clip in &mut merged {
        clip.moments.sort_by_key(|m| (m.metric.priority(), m.entry.rank));
    }

    // Drop anything too short to be a clip.
    let mut kept = Vec::new();
    for clip in merged {
        if clip.duration_s() < opts.min_duration_s {
            dropped.push(Dropped {
                moment: clip.moments[0].clone(),
                reason: format!(
                    "clip would be only {:.2} s long, below the {:.2} s minimum",
                    clip.duration_s(),
                    opts.min_duration_s
                ),
            });
            continue;
        }
        kept.push(clip);
    }
    (kept, dropped)
}

/// Write chapter markers in ffmpeg's metadata format.
///
/// `ffmpeg -i video -i chapters.txt -map_metadata 1 -codec copy out.mp4`
/// embeds them, and any player that reads chapters can then jump between the
/// moments.
pub fn write_ffmetadata(clips: &[Clip], title: Option<&str>) -> String {
    let mut out = String::from(";FFMETADATA1\n");
    if let Some(t) = title {
        // ffmetadata uses = ; # \ and newline as its syntax, so they escape.
        out.push_str(&format!("title={}\n", escape_ffmetadata(t)));
    }
    for clip in clips {
        out.push_str("\n[CHAPTER]\nTIMEBASE=1/1000\n");
        out.push_str(&format!("START={}\n", (clip.start_s * 1000.0).round() as i64));
        out.push_str(&format!("END={}\n", (clip.end_s * 1000.0).round() as i64));
        let names: Vec<String> = clip.moments.iter().map(|m| m.label()).collect();
        out.push_str(&format!("title={}\n", escape_ffmetadata(&names.join(", "))));
    }
    out
}

fn escape_ffmetadata(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '=' | ';' | '#' | '\\') {
            out.push('\\');
        }
        if c == '\n' {
            out.push_str("\\\n");
        } else {
            out.push(c);
        }
    }
    out
}

/// Format seconds as a CMX3600 timecode, `hh:mm:ss:ff`.
fn timecode(seconds: f64, fps: f64) -> String {
    let total_frames = (seconds * fps).round().max(0.0) as i64;
    let fps_i = fps.round().max(1.0) as i64;
    let frames = total_frames % fps_i;
    let total_seconds = total_frames / fps_i;
    format!(
        "{:02}:{:02}:{:02}:{:02}",
        total_seconds / 3600,
        (total_seconds / 60) % 60,
        total_seconds % 60,
        frames
    )
}

/// Write a CMX3600 edit decision list.
///
/// The lowest common denominator for handing an edit to another tool: Resolve,
/// Premiere and Avid all import it. Timecodes are whole frames, so a clip's
/// boundaries land on frame edges.
pub fn write_edl(clips: &[Clip], title: &str, reel: &str, fps: f64) -> String {
    let mut out = String::new();
    out.push_str(&format!("TITLE: {title}\n"));
    // 29.97 is a non-drop-frame rate in every tool that matters here.
    out.push_str("FCM: NON-DROP FRAME\n");
    let mut record = 0.0f64;
    // A reel name is conventionally at most eight characters.
    let reel: String = reel.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    let reel = if reel.is_empty() { "AX".to_string() } else { reel };

    for (i, clip) in clips.iter().enumerate() {
        let dur = clip.duration_s();
        out.push_str(&format!(
            "\n{:03}  {:<8} V     C        {} {} {} {}\n",
            i + 1,
            reel,
            timecode(clip.start_s, fps),
            timecode(clip.end_s, fps),
            timecode(record, fps),
            timecode(record + dur, fps),
        ));
        out.push_str(&format!("* FROM CLIP NAME: {title}\n"));
        for m in &clip.moments {
            out.push_str(&format!("* COMMENT: {}\n", m.label()));
        }
        record += dur;
    }
    out
}

/// Write a cue sheet, for anything that will not read an EDL.
pub fn write_cue_csv(clips: &[Clip]) -> String {
    let mut out =
        String::from("start_s,end_s,duration_s,metric,rank,value,unit,detail,lat,lon,speed_kmh\n");
    for clip in clips {
        for m in &clip.moments {
            let e = &m.entry;
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{},{}\n",
                fmt_num(clip.start_s, 3),
                fmt_num(clip.end_s, 3),
                fmt_num(clip.duration_s(), 3),
                m.metric.slug(),
                e.rank,
                fmt_num(e.value, 3),
                m.metric.unit(),
                m.label().replace(',', ";"),
                e.pos.map(|(lat, _)| fmt_num(lat, 7)).unwrap_or_default(),
                e.pos.map(|(_, lon)| fmt_num(lon, 7)).unwrap_or_default(),
                e.speed_kmh().map(|v| fmt_num(v, 1)).unwrap_or_default(),
            ));
        }
    }
    out
}
