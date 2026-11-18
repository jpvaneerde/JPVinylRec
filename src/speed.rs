//! Speed-factor estimation.
//!
//! Convention: `factor = recorded duration / true duration`.
//! factor > 1  -> the turntable ran slow (recording too long, pitch flat) -> speed up by `factor`.
//! factor < 1  -> the turntable ran fast.
//!
//! The reference method aligns the *energy envelope* of a digital copy (CD rip, stream
//! download) against the recording at many anchor points and fits a straight line
//! `t_rec = offset + factor * t_ref` through them. Envelopes are insensitive to the small
//! pitch difference, EQ/mastering differences and polarity, and many anchors spread over
//! minutes make the slope accurate to a few thousandths of a percent.

use crate::audio::Audio;
use anyhow::{anyhow, Result};
use rustfft::{num_complex::Complex, FftPlanner};

const FINE_HOP: f64 = 0.002;
const COARSE_DECIM: usize = 5; // 10 ms

#[derive(Clone, Debug)]
pub struct Estimate {
    pub factor: f64,
    /// Standard error of the factor (same units).
    pub stderr: f64,
    pub points_used: usize,
    pub points_total: usize,
    /// Where in the recording (seconds) the reference's t=0 lands.
    pub offset: f64,
    /// Mean normalised correlation of the used anchors (0..1).
    pub mean_corr: f32,
}

impl Estimate {
    pub fn percent(&self) -> f64 {
        (self.factor - 1.0) * 100.0
    }
}

/// Human-readable description of a speed factor.
pub fn describe(factor: f64) -> String {
    let pct = (factor - 1.0) * 100.0;
    let cents = 1200.0 * factor.log2();
    if pct.abs() < 0.0005 {
        return "spot on".to_string();
    }
    let dir = if factor > 1.0 { "slow" } else { "fast" };
    format!(
        "turntable ran {:.3}% {} (pitch {:+.1} cents); correction speeds audio by ×{:.5}",
        pct.abs(),
        dir,
        -cents,
        factor
    )
}

/// Log-energy envelope with the local mean removed (2 ms hop).
fn feature(mono: &[f32], rate: u32) -> Vec<f32> {
    // Exact (fractional) hop boundaries: at 44.1 kHz a 2 ms hop is 88.2 samples, and
    // rounding it would bias the speed estimate by 0.23 %.
    let h = rate as f64 * FINE_HOP;
    let n = (mono.len() as f64 / h).floor() as usize;
    let e: Vec<f64> = (0..n)
        .map(|i| {
            let a = (i as f64 * h).round() as usize;
            let b = (((i + 1) as f64 * h).round() as usize).min(mono.len());
            mono[a..b].iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / (b - a).max(1) as f64
        })
        .collect();
    let mut f: Vec<f64> = (0..n)
        .map(|i| {
            let v = e[i] + if i + 1 < n { e[i + 1] } else { e[i] };
            (v / 2.0 + 1e-10).ln()
        })
        .collect();
    // remove a 0.25 s moving average (level / EQ independence)
    let w = (0.25 / FINE_HOP) as usize;
    let mut cum = vec![0.0f64; n + 1];
    for i in 0..n {
        cum[i + 1] = cum[i] + f[i];
    }
    for i in 0..n {
        let a = i.saturating_sub(w / 2);
        let b = (i + w / 2 + 1).min(n);
        f[i] -= (cum[b] - cum[a]) / (b - a) as f64;
    }
    f.into_iter().map(|v| v as f32).collect()
}

fn decimate(x: &[f32], k: usize) -> Vec<f32> {
    x.chunks(k).map(|c| c.iter().sum::<f32>() / c.len() as f32).collect()
}

/// Zero-mean, unit-norm copy. Returns None if the window is (nearly) flat.
fn normalise(t: &[f32]) -> Option<Vec<f32>> {
    let m = t.iter().sum::<f32>() / t.len() as f32;
    let v: Vec<f32> = t.iter().map(|x| x - m).collect();
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n < 1e-3 * (t.len() as f32).sqrt() {
        return None;
    }
    Some(v.into_iter().map(|x| x / n).collect())
}

/// Normalised cross-correlation of a (normalised) template at one lag.
fn ncc_at(sig: &[f32], tpl: &[f32], lag: isize) -> f32 {
    if lag < 0 || lag as usize + tpl.len() > sig.len() {
        return -1.0;
    }
    let s = &sig[lag as usize..lag as usize + tpl.len()];
    let mut dot = 0.0f32;
    let mut sum = 0.0f32;
    let mut sq = 0.0f32;
    for (a, b) in s.iter().zip(tpl) {
        dot += a * b;
        sum += a;
        sq += a * a;
    }
    let l = tpl.len() as f32;
    let var = sq - sum * sum / l;
    if var <= 1e-9 {
        return -1.0;
    }
    dot / var.sqrt()
}

/// Best lag in [lo, hi], refined to sub-hop precision with a parabola.
fn search(sig: &[f32], tpl: &[f32], lo: isize, hi: isize) -> Option<(f64, f32)> {
    let lo = lo.max(0);
    let hi = hi.min(sig.len() as isize - tpl.len() as isize);
    if hi <= lo + 1 {
        return None;
    }
    let vals: Vec<f32> = (lo..=hi).map(|l| ncc_at(sig, tpl, l)).collect();
    let (bi, bv) = vals
        .iter()
        .enumerate()
        .fold((0usize, f32::MIN), |acc, (i, v)| if *v > acc.1 { (i, *v) } else { acc });
    let mut pos = bi as f64;
    if bi > 0 && bi + 1 < vals.len() {
        let (a, b, c) = (vals[bi - 1] as f64, vals[bi] as f64, vals[bi + 1] as f64);
        let den = a - 2.0 * b + c;
        if den.abs() > 1e-12 {
            pos += (0.5 * (a - c) / den).clamp(-0.5, 0.5);
        }
    } else {
        return None; // peak at the edge of the search range: not trustworthy
    }
    Some((lo as f64 + pos, bv))
}

/// FFT normalised cross-correlation of template against the whole signal; returns best lag.
fn locate_fft(sig: &[f32], tpl: &[f32]) -> Option<(usize, f32)> {
    let n = sig.len();
    let l = tpl.len();
    if l >= n {
        return None;
    }
    let size = (n + l).next_power_of_two();
    let mut planner = FftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(size);
    let inv = planner.plan_fft_inverse(size);
    let mut a: Vec<Complex<f32>> = (0..size)
        .map(|i| Complex::new(if i < n { sig[i] } else { 0.0 }, 0.0))
        .collect();
    let mut b: Vec<Complex<f32>> = (0..size)
        .map(|i| Complex::new(if i < l { tpl[i] } else { 0.0 }, 0.0))
        .collect();
    fwd.process(&mut a);
    fwd.process(&mut b);
    for (x, y) in a.iter_mut().zip(&b) {
        *x *= y.conj();
    }
    inv.process(&mut a);
    let mut cs = vec![0.0f64; n + 1];
    let mut cq = vec![0.0f64; n + 1];
    for i in 0..n {
        cs[i + 1] = cs[i] + sig[i] as f64;
        cq[i + 1] = cq[i] + (sig[i] as f64) * (sig[i] as f64);
    }
    let mut best = (0usize, f32::MIN);
    for lag in 0..=(n - l) {
        let s = cs[lag + l] - cs[lag];
        let q = cq[lag + l] - cq[lag];
        let var = q - s * s / l as f64;
        if var <= 1e-9 {
            continue;
        }
        let v = (a[lag].re as f64 / size as f64 / var.sqrt()) as f32;
        if v > best.1 {
            best = (lag, v);
        }
    }
    Some(best)
}

fn interp(x: &[f32], pos: f64) -> f32 {
    if pos < 0.0 {
        return x.first().copied().unwrap_or(0.0);
    }
    let i = pos.floor() as usize;
    if i + 1 >= x.len() {
        return x.last().copied().unwrap_or(0.0);
    }
    let fr = (pos - i as f64) as f32;
    x[i] * (1.0 - fr) + x[i + 1] * fr
}

/// Least-squares line y = a + b x with iterative outlier rejection.
/// Returns (intercept, slope, stderr of slope, points used).
pub fn robust_line(pts: &[(f64, f64)]) -> Option<(f64, f64, f64, usize)> {
    let mut use_: Vec<bool> = vec![true; pts.len()];
    let mut result = None;
    for _ in 0..4 {
        let sel: Vec<(f64, f64)> = pts.iter().zip(&use_).filter(|(_, u)| **u).map(|(p, _)| *p).collect();
        if sel.len() < 2 {
            return result;
        }
        let n = sel.len() as f64;
        let mx = sel.iter().map(|p| p.0).sum::<f64>() / n;
        let my = sel.iter().map(|p| p.1).sum::<f64>() / n;
        let sxx = sel.iter().map(|p| (p.0 - mx).powi(2)).sum::<f64>();
        if sxx <= 0.0 {
            return result;
        }
        let sxy = sel.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum::<f64>();
        let b = sxy / sxx;
        let a = my - b * mx;
        let res: Vec<f64> = sel.iter().map(|p| p.1 - (a + b * p.0)).collect();
        let dof = (n - 2.0).max(1.0);
        let sigma2 = res.iter().map(|r| r * r).sum::<f64>() / dof;
        let se = if sel.len() > 2 { (sigma2 / sxx).sqrt() } else { f64::NAN };
        result = Some((a, b, se, sel.len()));
        // MAD-based rejection
        let mut absr: Vec<f64> = pts.iter().map(|p| (p.1 - (a + b * p.0)).abs()).collect();
        let mut tmp = absr.clone();
        tmp.sort_by(|x, y| x.total_cmp(y));
        let mad = tmp[tmp.len() / 2].max(1e-4);
        let lim = 4.0 * 1.4826 * mad;
        let mut changed = false;
        for (i, r) in absr.iter_mut().enumerate() {
            let keep = *r <= lim;
            if keep != use_[i] {
                use_[i] = keep;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    result
}

/// Pre-computed envelope features for the recording (reuse across several references).
pub struct RecFeatures {
    fine: Vec<f32>,
    coarse: Vec<f32>,
}

pub fn rec_features(rec: &Audio) -> RecFeatures {
    let fine = feature(&rec.mono(), rec.rate);
    let coarse = decimate(&fine, COARSE_DECIM);
    RecFeatures { fine, coarse }
}

/// Estimate the speed factor by aligning a digital reference copy against the recording.
/// The reference can be one track or the whole side; it is searched for anywhere in the recording.
pub fn estimate_from_reference(recf: &RecFeatures, reference: &Audio) -> Result<Estimate> {
    let ref_fine = feature(&reference.mono(), reference.rate);
    let ref_coarse = decimate(&ref_fine, COARSE_DECIM);
    let ch = FINE_HOP * COARSE_DECIM as f64; // coarse hop seconds
    let ref_len_s = ref_fine.len() as f64 * FINE_HOP;
    if ref_len_s < 20.0 {
        return Err(anyhow!("reference is too short ({ref_len_s:.0} s); use at least 20 s, ideally a full track"));
    }

    // ---- 1. find where the reference sits in the recording ----
    let loc_len = (8.0 / ch) as usize;
    let mut best_tpl: Option<(usize, Vec<f32>, f32)> = None;
    for k in 1..9 {
        let start = (ref_coarse.len() - loc_len) * k / 10;
        let w = &ref_coarse[start..start + loc_len];
        let var = {
            let m = w.iter().sum::<f32>() / w.len() as f32;
            w.iter().map(|x| (x - m).powi(2)).sum::<f32>()
        };
        if let Some(t) = normalise(w) {
            if best_tpl.as_ref().map(|b| var > b.2).unwrap_or(true) {
                best_tpl = Some((start, t, var));
            }
        }
    }
    let (loc_start, loc_tpl, _) = best_tpl.ok_or_else(|| anyhow!("reference appears to be silent"))?;
    let (lag, corr) = locate_fft(&recf.coarse, &loc_tpl).ok_or_else(|| anyhow!("recording shorter than reference window"))?;
    if corr < 0.45 {
        return Err(anyhow!(
            "could not find the reference in the recording (best correlation {corr:.2}). Is it the same song?"
        ));
    }
    let t0_ref = loc_start as f64 * ch;
    let t0_rec = lag as f64 * ch;

    // ---- 2. coarse anchors -> first slope estimate ----
    let win = (5.0 / ch) as usize;
    let step = (3.0 / ch) as usize;
    let mut pts = Vec::new();
    let mut s = 0usize;
    while s + win <= ref_coarse.len() {
        if let Some(tpl) = normalise(&ref_coarse[s..s + win]) {
            let t_ref = s as f64 * ch;
            let pred = t0_rec + (t_ref - t0_ref);
            let radius = 0.03 * (t_ref - t0_ref).abs() + 1.0;
            if let Some((l, c)) = search(
                &recf.coarse,
                &tpl,
                ((pred - radius) / ch) as isize,
                ((pred + radius) / ch) as isize,
            ) {
                if c > 0.5 {
                    pts.push((t_ref, l * ch));
                }
            }
        }
        s += step;
    }
    let (mut off, mut factor, _, _) =
        robust_line(&pts).ok_or_else(|| anyhow!("too few matching points between reference and recording"))?;

    // ---- 3. fine anchors on time-warped templates, iterated ----
    let fwin = (3.0 / FINE_HOP) as usize;
    let fstep = (2.0 / FINE_HOP) as usize;
    let mut last = None;
    for _ in 0..3 {
        let mut pts = Vec::new();
        let mut corrs = Vec::new();
        let mut total = 0;
        let mut s = 0usize;
        while (s as f64 * FINE_HOP) + 3.0 * 1.05 <= ref_len_s {
            let t_ref = s as f64 * FINE_HOP;
            // template sampled on the recording's time grid
            let warped: Vec<f32> = (0..fwin)
                .map(|j| interp(&ref_fine, (t_ref + j as f64 * FINE_HOP / factor) / FINE_HOP))
                .collect();
            s += fstep;
            let Some(tpl) = normalise(&warped) else { continue };
            total += 1;
            let pred = (off + factor * t_ref) / FINE_HOP;
            if let Some((l, c)) = search(&recf.fine, &tpl, pred as isize - 25, pred as isize + 25) {
                if c > 0.6 {
                    pts.push((t_ref, l * FINE_HOP));
                    corrs.push(c);
                }
            }
        }
        let Some((a, b, se, used)) = robust_line(&pts) else { break };
        off = a;
        factor = b;
        let mean_corr = corrs.iter().sum::<f32>() / corrs.len().max(1) as f32;
        last = Some(Estimate { factor, stderr: se, points_used: used, points_total: total, offset: off, mean_corr });
    }
    let est = last.ok_or_else(|| anyhow!("fine alignment failed"))?;
    if est.points_used < 5 {
        return Err(anyhow!("only {} anchor points matched; result unreliable", est.points_used));
    }
    Ok(est)
}

/// Combine several estimates by inverse-variance weighting.
pub fn combine(ests: &[Estimate]) -> Option<(f64, f64)> {
    let mut wsum = 0.0;
    let mut fsum = 0.0;
    for e in ests {
        let se = if e.stderr.is_finite() && e.stderr > 0.0 { e.stderr } else { 1e-4 };
        let w = 1.0 / (se * se);
        wsum += w;
        fsum += w * e.factor;
    }
    if wsum == 0.0 {
        return None;
    }
    Some((fsum / wsum, (1.0 / wsum).sqrt()))
}
