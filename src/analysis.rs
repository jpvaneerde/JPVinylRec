//! Level analysis of a side recording: envelopes, noise floor, where the music
//! starts and ends, and the quiet gaps between tracks.

use crate::audio::Audio;

/// Hop of the analysis envelope in seconds.
pub const HOP: f64 = 0.01;

#[derive(Clone, Debug)]
pub struct Gap {
    /// Seconds in the recording.
    pub start: f64,
    pub end: f64,
    /// How far (dB) the quietest point of the gap sits below the threshold.
    pub depth_db: f32,
}

impl Gap {
    pub fn len(&self) -> f64 {
        self.end - self.start
    }
}

#[derive(Clone, Debug)]
pub struct Analysis {
    /// RMS level per hop, dB, median-filtered (removes clicks and pops).
    pub level_db: Vec<f32>,
    /// Peak absolute value per hop (for drawing the waveform).
    pub peaks: Vec<f32>,
    pub noise_floor_db: f32,
    pub music_level_db: f32,
    pub threshold_db: f32,
    pub music_start: f64,
    pub music_end: f64,
    pub gaps: Vec<Gap>,
}

/// Splits the signal into frames of exactly `hop_s` seconds (fractional sample boundaries).
fn frames(mono: &[f32], rate: u32, hop_s: f64) -> impl Iterator<Item = &[f32]> {
    let h = rate as f64 * hop_s;
    let n = (mono.len() as f64 / h).ceil() as usize;
    (0..n).map(move |i| {
        let a = ((i as f64 * h).round() as usize).min(mono.len());
        let b = (((i + 1) as f64 * h).round() as usize).min(mono.len());
        &mono[a..b.max(a)]
    })
}

pub fn rms_db_envelope(mono: &[f32], rate: u32, hop_s: f64) -> Vec<f32> {
    frames(mono, rate, hop_s)
        .map(|c| {
            let e = c.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / c.len().max(1) as f64;
            (10.0 * (e + 1e-12).log10()) as f32
        })
        .collect()
}

pub fn peak_envelope(mono: &[f32], rate: u32, hop_s: f64) -> Vec<f32> {
    frames(mono, rate, hop_s)
        .map(|c| c.iter().fold(0.0f32, |m, s| m.max(s.abs())))
        .collect()
}

/// Sliding median with an odd window; edges use a truncated window.
pub fn median_filter(x: &[f32], win: usize) -> Vec<f32> {
    let half = win / 2;
    let mut buf: Vec<f32> = Vec::with_capacity(win);
    (0..x.len())
        .map(|i| {
            let a = i.saturating_sub(half);
            let b = (i + half + 1).min(x.len());
            buf.clear();
            buf.extend_from_slice(&x[a..b]);
            let m = buf.len() / 2;
            buf.select_nth_unstable_by(m, |p, q| p.total_cmp(q));
            buf[m]
        })
        .collect()
}

fn percentile(x: &[f32], p: f64) -> f32 {
    let mut v: Vec<f32> = x.iter().copied().filter(|v| *v > -110.0).collect();
    if v.is_empty() {
        return -120.0;
    }
    let k = ((v.len() - 1) as f64 * p).round() as usize;
    v.select_nth_unstable_by(k, |a, b| a.total_cmp(b));
    v[k]
}

pub fn analyse(audio: &Audio) -> Analysis {
    let mono = audio.mono();
    let raw = rms_db_envelope(&mono, audio.rate, HOP);
    let peaks = peak_envelope(&mono, audio.rate, HOP);
    // 0.25 s median: clicks/pops are much shorter than that and disappear.
    let level = median_filter(&raw, 25);

    let floor = percentile(&level, 0.03);
    let music = percentile(&level, 0.60);
    let threshold = (floor + (0.3 * (music - floor)).max(6.0)).min(music - 6.0);

    let (start, end) = music_bounds(&level, threshold);
    let gaps = find_gaps(&level, threshold, start, end, 0.35);

    Analysis {
        level_db: level,
        peaks,
        noise_floor_db: floor,
        music_level_db: music,
        threshold_db: threshold,
        music_start: start,
        music_end: end,
        gaps,
    }
}

/// First / last point where the level is above threshold for most of a 2 s window.
/// Requiring "most of the window" ignores the periodic click of the run-out groove.
pub fn music_bounds(level: &[f32], thr: f32) -> (f64, f64) {
    let n = level.len();
    if n == 0 {
        return (0.0, 0.0);
    }
    let w = (2.0 / HOP) as usize;
    let above: Vec<u32> = level.iter().map(|v| (*v > thr) as u32).collect();
    let mut cum = vec![0u32; n + 1];
    for i in 0..n {
        cum[i + 1] = cum[i] + above[i];
    }
    let frac = |a: usize, b: usize| (cum[b] - cum[a]) as f64 / (b - a).max(1) as f64;

    let mut start = 0;
    for i in 0..n {
        if above[i] == 1 && frac(i, (i + w).min(n)) >= 0.6 {
            start = i;
            break;
        }
    }
    let mut end = n - 1;
    for i in (0..n).rev() {
        if above[i] == 1 && frac(i.saturating_sub(w), i + 1) >= 0.6 {
            end = i + 1;
            break;
        }
    }
    if end <= start {
        return (0.0, n as f64 * HOP);
    }
    (start as f64 * HOP, (end as f64 * HOP).min(n as f64 * HOP))
}

pub fn find_gaps(level: &[f32], thr: f32, start: f64, end: f64, min_len: f64) -> Vec<Gap> {
    let a = (start / HOP) as usize;
    let b = ((end / HOP) as usize).min(level.len());
    let mut gaps = Vec::new();
    let mut i = a;
    while i < b {
        if level[i] < thr {
            let s = i;
            let mut mn = level[i];
            while i < b && level[i] < thr {
                mn = mn.min(level[i]);
                i += 1;
            }
            let len = (i - s) as f64 * HOP;
            if len >= min_len {
                gaps.push(Gap { start: s as f64 * HOP, end: i as f64 * HOP, depth_db: thr - mn });
            }
        } else {
            i += 1;
        }
    }
    gaps
}

/// Time of the quietest point (smoothed level) between two times.
pub fn quietest_point(level: &[f32], from: f64, to: f64) -> f64 {
    let a = (from.max(0.0) / HOP) as usize;
    let b = ((to / HOP) as usize).min(level.len());
    if b <= a {
        return from;
    }
    // smooth a little more so a single quiet hop between beats doesn't win
    let mut best = a;
    let mut best_v = f32::MAX;
    for i in a..b {
        let lo = i.saturating_sub(10);
        let hi = (i + 10).min(level.len());
        let v = level[lo..hi].iter().sum::<f32>() / (hi - lo) as f32;
        if v < best_v {
            best_v = v;
            best = i;
        }
    }
    best as f64 * HOP
}
