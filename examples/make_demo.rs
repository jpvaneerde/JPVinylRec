//! Demo generator: a synthetic vinyl side with a known speed error, plus a CD-style reference.

use jpvinylrec::resample::{resample, Kernel};

struct Rng(u64);
impl Rng {
    fn f(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct Song {
    notes: Vec<(f64, f64, f64, f32)>,
    hits: Vec<(f64, f32)>,
    len: f64,
}

fn make_song(seed: u64, len: f64) -> Song {
    let mut r = Rng(seed);
    let mut notes = Vec::new();
    let mut t = 0.0;
    while t < len - 0.3 {
        let dur = 0.15 + 0.45 * r.f();
        let k = (r.f() * 36.0).floor();
        notes.push((t, dur, 110.0 * 2f64.powf(k / 12.0), (0.08 + 0.15 * r.f()) as f32));
        t += dur * (0.4 + 0.6 * r.f());
    }
    let beat = 60.0 / (90.0 + 60.0 * r.f());
    let mut hits = Vec::new();
    let mut t = 0.0;
    while t < len - 0.2 {
        if r.f() < 0.75 {
            hits.push((t, (0.15 + 0.2 * r.f()) as f32));
        }
        t += beat;
    }
    Song { notes, hits, len }
}

/// Render a song at any sample rate (the synthesis is a function of time).
fn render(song: &Song, rate: u32, noise_seed: u64) -> Vec<f32> {
    let n = (song.len * rate as f64) as usize;
    let mut out = vec![0.0f32; n];
    for &(s, d, f, a) in &song.notes {
        let i0 = (s * rate as f64) as usize;
        let i1 = (((s + d + 0.15) * rate as f64) as usize).min(n);
        for (i, o) in out.iter_mut().enumerate().take(i1).skip(i0) {
            let dt = i as f64 / rate as f64 - s;
            let env = (dt / 0.005).min(1.0) * (-3.0 * dt).exp() * if dt > d { (1.0 - (dt - d) / 0.15).max(0.0) } else { 1.0 };
            *o += (a as f64 * env * (2.0 * std::f64::consts::PI * f * dt).sin()) as f32;
        }
    }
    let mut r = Rng(noise_seed);
    for &(h, a) in &song.hits {
        let i0 = (h * rate as f64) as usize;
        let i1 = (((h + 0.2) * rate as f64) as usize).min(n);
        for (i, o) in out.iter_mut().enumerate().take(i1).skip(i0) {
            let dt = i as f64 / rate as f64 - h;
            *o += (a as f64 * (2.0 * r.f() - 1.0) * (-30.0 * dt).exp()) as f32;
        }
    }
    out
}

/// Writes a synthetic 3-track vinyl side (turntable 0.87 % slow) and a CD-style reference
/// of track 2 into the given folder. Used for demos/screenshots.
fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&dir).unwrap();
    let rate = 96_000u32;
    let f = 1.0087;
    let songs = [make_song(11, 62.4), make_song(22, 75.7), make_song(33, 48.2)];
    let mut side: Vec<f32> = vec![0.0; 3 * rate as usize];
    for (s, g) in songs.iter().zip([2.1, 1.6, 0.0]) {
        side.extend(render(s, rate, 5));
        side.extend(vec![0.0; (g * rate as f64) as usize]);
    }
    side.extend(vec![0.0; 6 * rate as usize]);
    let out_len = (side.len() as f64 * f) as usize;
    let mut rec = resample(&[&side], 0.0, 1.0 / f, out_len, &Kernel::new(1.0 / f)).remove(0);
    let mut r = Rng(99);
    for s in rec.iter_mut() {
        *s += (0.002 * (2.0 * r.f() - 1.0)) as f32;
        if r.f() < 1e-5 { *s += (0.5 * (2.0 * r.f() - 1.0)) as f32; }
    }
    jpvinylrec::export::write_planar(&dir.join("side-a.flac"), &[rec.clone(), rec], rate, 24).unwrap();
    let reference = render(&songs[1], 44_100, 77);
    jpvinylrec::export::write_planar(&dir.join("cd-track2.flac"), &[reference.clone(), reference], 44_100, 16).unwrap();
    println!("wrote demo files to {}", dir.display());
}
