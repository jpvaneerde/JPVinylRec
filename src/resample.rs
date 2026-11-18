//! High-quality speed change (pitch and tempo together, exactly like changing the platter
//! speed) with a Kaiser-windowed sinc interpolator. Output sample k is taken from input
//! position `start + k * factor`, so the timing of every cut is exact.

use rayon::prelude::*;

pub struct Kernel {
    /// Half-width of the kernel in input samples.
    half: f64,
    res: f64,
    table: Vec<f32>,
}

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..60 {
        term *= q / (k as f64 * k as f64);
        sum += term;
        if term < 1e-17 * sum {
            break;
        }
    }
    sum
}

impl Kernel {
    /// `factor` > 1 compresses time (speeds up), so the cutoff must drop to avoid aliasing.
    pub fn new(factor: f64) -> Self {
        let zeros = 32.0;
        let beta = 10.0;
        let cutoff = 0.97 * (1.0f64).min(1.0 / factor);
        let half = zeros / cutoff;
        let res = 512.0;
        let len = (half * res).ceil() as usize + 2;
        let i0b = bessel_i0(beta);
        let table = (0..len)
            .map(|k| {
                let x = k as f64 / res;
                if x >= half {
                    return 0.0;
                }
                let sinc = if x == 0.0 {
                    1.0
                } else {
                    let a = std::f64::consts::PI * cutoff * x;
                    a.sin() / a
                };
                let r = x / half;
                let win = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b;
                (cutoff * sinc * win) as f32
            })
            .collect();
        Kernel { half, res, table }
    }

    #[inline]
    fn w(&self, x: f64) -> f32 {
        let p = x.abs() * self.res;
        let i = p as usize;
        if i + 1 >= self.table.len() {
            return 0.0;
        }
        let f = (p - i as f64) as f32;
        self.table[i] * (1.0 - f) + self.table[i + 1] * f
    }
}

/// Resample several channels at once. Output sample k reads input position `start + k*factor`
/// (in input samples). Positions outside the input read as silence.
pub fn resample(channels: &[&[f32]], start: f64, factor: f64, out_len: usize, kernel: &Kernel) -> Vec<Vec<f32>> {
    let nch = channels.len();
    let len_in = channels.first().map(|c| c.len()).unwrap_or(0) as isize;
    let r = kernel.half.ceil() as isize;
    const CHUNK: usize = 16384;
    let chunks: Vec<Vec<Vec<f32>>> = (0..out_len.div_ceil(CHUNK))
        .into_par_iter()
        .map(|ci| {
            let a = ci * CHUNK;
            let b = (a + CHUNK).min(out_len);
            let mut out = vec![Vec::with_capacity(b - a); nch];
            let mut weights: Vec<f32> = Vec::with_capacity((2 * r + 2) as usize);
            for k in a..b {
                let pos = start + k as f64 * factor;
                let base = pos.floor() as isize;
                let lo = base - r + 1;
                let hi = base + r;
                weights.clear();
                let mut wsum = 0.0f32;
                for n in lo..=hi {
                    let w = kernel.w(n as f64 - pos);
                    weights.push(w);
                    wsum += w;
                }
                let norm = if wsum.abs() > 1e-6 { 1.0 / wsum } else { 1.0 };
                // clip the tap range to the available input
                let n0 = lo.max(0);
                let n1 = hi.min(len_in - 1);
                for (c, ch) in channels.iter().enumerate() {
                    let mut acc = 0.0f32;
                    if n1 >= n0 {
                        let ws = &weights[(n0 - lo) as usize..=(n1 - lo) as usize];
                        let xs = &ch[n0 as usize..=n1 as usize];
                        for (w, x) in ws.iter().zip(xs) {
                            acc += w * x;
                        }
                    }
                    out[c].push(acc * norm);
                }
            }
            out
        })
        .collect();
    let mut result = vec![Vec::with_capacity(out_len); nch];
    for chunk in chunks {
        for (c, v) in chunk.into_iter().enumerate() {
            result[c].extend(v);
        }
    }
    result
}
