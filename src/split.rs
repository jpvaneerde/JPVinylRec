//! Placing track boundaries: quiet gaps in the recording, chosen with the help of
//! database track durations (scaled to the recording, so speed errors don't matter here).

use crate::analysis::{quietest_point, Analysis};
use crate::speed::robust_line;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerKind {
    /// Start of the music (end of lead-in).
    Start,
    /// Boundary snapped to a silent gap.
    Gap,
    /// No gap found near the expected position; placed at the quietest point. Check it!
    NoGap,
    /// Moved by the user.
    Manual,
    /// End of the music (start of run-out).
    End,
}

#[derive(Clone, Debug)]
pub struct Marker {
    /// Seconds in the (uncorrected) recording.
    pub t: f64,
    pub kind: MarkerKind,
}

/// Search radius (seconds) around an expected boundary.
const TOLERANCE: f64 = 10.0;

/// Where to cut inside a gap: shortly before the next track's music begins,
/// so each track starts promptly and the trailing silence stays with the previous track.
fn cut_in_gap(start: f64, end: f64) -> f64 {
    (end - 0.25).max(start + (end - start) * 0.5)
}

/// Propose markers for `n = durations.len()` tracks: [start, cut_1 .. cut_{n-1}, end].
/// `durations` are database lengths in seconds (None = unknown).
pub fn propose(durations: &[Option<f64>], an: &Analysis) -> Vec<Marker> {
    let n = durations.len();
    let start = an.music_start;
    let end = an.music_end;
    let mut markers = vec![Marker { t: start, kind: MarkerKind::Start }];
    if n == 0 {
        markers.push(Marker { t: end, kind: MarkerKind::End });
        return markers;
    }

    if durations.iter().all(|d| d.is_some()) {
        let durs: Vec<f64> = durations.iter().map(|d| d.unwrap()).collect();
        let total: f64 = durs.iter().sum();
        let scale = (end - start) / total.max(1.0);
        let mut cum = 0.0;
        let mut prev = start;
        for d in &durs[..n - 1] {
            cum += d;
            let expected = start + cum * scale;
            let mut best: Option<(f64, f64)> = None; // (score, cut)
            for g in &an.gaps {
                let cut = cut_in_gap(g.start, g.end);
                let dist = (cut - expected).abs();
                if dist > TOLERANCE || cut < prev + 5.0 {
                    continue;
                }
                let score = dist - 1.5 * g.len().min(2.0) - (g.depth_db as f64 / 10.0).min(2.0);
                if best.map(|b| score < b.0).unwrap_or(true) {
                    best = Some((score, cut));
                }
            }
            let m = match best {
                Some((_, cut)) => Marker { t: cut, kind: MarkerKind::Gap },
                None => {
                    let lo = (expected - 4.0).max(prev + 5.0);
                    let t = quietest_point(&an.level_db, lo, (expected + 4.0).max(lo + 0.5));
                    Marker { t, kind: MarkerKind::NoGap }
                }
            };
            prev = m.t;
            markers.push(m);
        }
    } else {
        // No reliable durations: take the n-1 most convincing gaps.
        let mut gs: Vec<_> = an.gaps.iter().collect();
        gs.sort_by(|a, b| {
            let sa = a.len().min(3.0) * (a.depth_db as f64 + 3.0);
            let sb = b.len().min(3.0) * (b.depth_db as f64 + 3.0);
            sb.total_cmp(&sa)
        });
        let mut cuts: Vec<f64> = gs.iter().take(n - 1).map(|g| cut_in_gap(g.start, g.end)).collect();
        cuts.sort_by(|a, b| a.total_cmp(b));
        for c in cuts {
            markers.push(Marker { t: c, kind: MarkerKind::Gap });
        }
        // not enough gaps: spread the rest evenly so the user can drag them
        while markers.len() < n {
            let last = markers.last().unwrap().t;
            markers.push(Marker { t: last + (end - last) / 2.0, kind: MarkerKind::NoGap });
        }
    }
    markers.push(Marker { t: end, kind: MarkerKind::End });
    markers
}

/// Speed factor from database durations.
///
/// Returns (side-total factor, its estimated standard error, regression slope).
/// The side total (music span / sum of database lengths) is the primary value: per-track
/// rounding errors partly cancel in the sum. The regression slope of every boundary against
/// cumulative database time is returned as a cross-check (it disagrees when one database
/// length is plainly wrong).
pub fn db_factor(markers: &[f64], durations: &[f64]) -> Option<(f64, f64, f64)> {
    if markers.len() != durations.len() + 1 || durations.is_empty() {
        return None;
    }
    let mut cum = 0.0;
    let mut pts = vec![(0.0, markers[0])];
    for (i, d) in durations.iter().enumerate() {
        cum += d;
        pts.push((cum, markers[i + 1]));
    }
    let total_factor = (markers[markers.len() - 1] - markers[0]) / cum;
    // whole-second rounding: ±0.29 s rms per track; plus ~1 s uncertainty in where the
    // last track "ends" (fade-out vs database convention)
    let n = durations.len() as f64;
    let se = (n * 0.083 + 1.0).sqrt() / cum;
    let slope = robust_line(&pts).map(|r| r.1).unwrap_or(total_factor);
    Some((total_factor, se, slope))
}

/// Per-track comparison after correction: (corrected length, database length, difference).
pub fn track_deltas(markers: &[f64], durations: &[Option<f64>], factor: f64) -> Vec<(f64, Option<f64>, Option<f64>)> {
    markers
        .windows(2)
        .zip(durations)
        .map(|(w, d)| {
            let len = (w[1] - w[0]) / factor;
            (len, *d, d.map(|d| len - d))
        })
        .collect()
}

/// Parse "4:32", "1:02:03" or "272" into seconds.
pub fn parse_duration(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total = 0.0;
    for part in s.split(':') {
        let v: f64 = part.trim().parse().ok()?;
        total = total * 60.0 + v;
    }
    if total > 0.0 {
        Some(total)
    } else {
        None
    }
}

pub fn fmt_time(t: f64) -> String {
    let neg = t < 0.0;
    let t = t.abs();
    let m = (t / 60.0).floor();
    let s = t - m * 60.0;
    format!("{}{}:{:05.2}", if neg { "-" } else { "" }, m as u64, s)
}
