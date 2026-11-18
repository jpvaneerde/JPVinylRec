//! End-to-end check on a synthetic vinyl side with a known speed error.

use jpvinylrec::analysis::analyse;
use jpvinylrec::audio::Audio;
use jpvinylrec::export::{export, Cut, Settings};
use jpvinylrec::meta::{Release, Source, Track};
use jpvinylrec::resample::{resample, Kernel};
use jpvinylrec::speed::{estimate_from_reference, rec_features};
use jpvinylrec::split::{db_factor, propose, MarkerKind};

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

#[test]
fn full_pipeline() {
    let rate = 48_000u32;
    let true_factor = 1.0087; // turntable ran 0.87 % slow
    let songs = [make_song(11, 62.4), make_song(22, 75.7), make_song(33, 48.2)];
    let gaps_after = [2.1, 1.6, 0.0];
    let lead_in = 3.0;

    // the "true" side, at correct speed
    let mut side: Vec<f32> = vec![0.0; (lead_in * rate as f64) as usize];
    let mut true_starts = Vec::new();
    for (s, g) in songs.iter().zip(gaps_after) {
        true_starts.push(side.len() as f64 / rate as f64);
        side.extend(render(s, rate, 5));
        side.extend(vec![0.0; (g * rate as f64) as usize]);
    }
    let music_end_true = side.len() as f64 / rate as f64;
    side.extend(vec![0.0; 6 * rate as usize]);

    // play it slow: output is longer by true_factor
    let k = Kernel::new(1.0 / true_factor);
    let out_len = (side.len() as f64 * true_factor) as usize;
    let mut rec = resample(&[&side], 0.0, 1.0 / true_factor, out_len, &k).remove(0);
    // surface noise + clicks, plus run-out groove clicks every 1.8 s
    let mut r = Rng(99);
    for s in rec.iter_mut() {
        *s += (0.002 * (2.0 * r.f() - 1.0)) as f32;
        if r.f() < 2e-5 {
            *s += (0.5 * (2.0 * r.f() - 1.0)) as f32;
        }
    }
    let runout_start = ((music_end_true + 0.5) * true_factor * rate as f64) as usize;
    let mut i = runout_start;
    while i + 50 < rec.len() {
        for j in 0..50 {
            rec[i + j] += 0.3 * (1.0 - j as f32 / 50.0);
        }
        i += (1.8 * rate as f64) as usize;
    }
    let recording = Audio { rate, bits: 24, data: vec![rec.clone(), rec] };

    // ---- analysis ----
    let an = analyse(&recording);
    println!("floor {:.1} music {:.1} thr {:.1}", an.noise_floor_db, an.music_level_db, an.threshold_db);
    println!("music {:.2}..{:.2}  gaps: {:?}", an.music_start, an.music_end, an.gaps.iter().map(|g| (g.start, g.end)).collect::<Vec<_>>());
    assert!((an.music_start - lead_in * true_factor).abs() < 0.1, "music start {}", an.music_start);
    assert!((an.music_end - music_end_true * true_factor).abs() < 1.0, "music end {} (run-out clicks must be ignored)", an.music_end);

    // ---- reference method: song 2 as a 44.1 kHz "CD rip" ----
    let reference = Audio { rate: 44_100, bits: 16, data: vec![render(&songs[1], 44_100, 77)] };
    let rf = rec_features(&recording);
    let est = estimate_from_reference(&rf, &reference).expect("reference estimate");
    let err_pct = (est.factor - true_factor) / true_factor * 100.0;
    println!(
        "reference: factor {:.6} (true {true_factor}) err {:+.4}% ±{:.4}%  anchors {}/{} corr {:.2} offset {:.2}s (true {:.2})",
        est.factor,
        err_pct,
        est.stderr * 100.0,
        est.points_used,
        est.points_total,
        est.mean_corr,
        est.offset,
        true_starts[1] * true_factor
    );
    assert!(err_pct.abs() < 0.005, "reference speed error {err_pct}%");
    assert!((est.offset - true_starts[1] * true_factor).abs() < 0.01);

    // ---- database method: durations rounded to whole seconds, including the trailing gap ----
    let db: Vec<f64> = songs.iter().zip(gaps_after).map(|(s, g)| (s.len + g).round()).collect();
    let markers = propose(&db.iter().map(|d| Some(*d)).collect::<Vec<_>>(), &an);
    println!("markers: {:?}", markers.iter().map(|m| (m.t, m.kind)).collect::<Vec<_>>());
    assert_eq!(markers.len(), 4);
    for (i, m) in markers[1..3].iter().enumerate() {
        let truth = true_starts[i + 1] * true_factor;
        assert_eq!(m.kind, MarkerKind::Gap);
        assert!(m.t < truth && truth - m.t < 0.4, "cut {i} at {} vs music start {}", m.t, truth);
    }
    let ts: Vec<f64> = markers.iter().map(|m| m.t).collect();
    let (dbf, dbse, slope) = db_factor(&ts, &db).unwrap();
    println!("database: side-total {:.5} ±{:.5}  regression {:.5}", dbf, dbse, slope);
    assert!(((dbf - true_factor) / true_factor).abs() < 2.0 * dbse);

    // ---- export with the reference factor ----
    let tracks: Vec<Track> = songs
        .iter()
        .enumerate()
        .map(|(i, _)| Track {
            position: format!("A{}", i + 1),
            side: "A".into(),
            number: i + 1,
            title: format!("Song {}: \"test\"?", i + 1),
            artist: None,
            duration: Some(db[i]),
            mb_recording_id: None,
            mb_track_id: None,
        })
        .collect();
    let release = Release {
        source: Source::Discogs(1),
        title: "Synthetic Side".into(),
        artist: "Test Artist".into(),
        original_year: Some("1984".into()),
        release_year: Some("1990".into()),
        label: Some("Label".into()),
        catno: Some("LP 1".into()),
        country: None,
        tracks: tracks.clone(),
        cover_url: None,
        mb_release_group: None,
    };
    let cuts: Vec<Cut> = tracks
        .iter()
        .enumerate()
        .map(|(i, t)| Cut { track: t.clone(), start: ts[i], end: ts[i + 1] })
        .collect();
    let dir = std::env::temp_dir().join("jpvinylrec-test");
    let _ = std::fs::remove_dir_all(&dir);
    let settings = Settings { root: dir.clone(), bits: 24, embed_cover: false, overwrite: true, lead_in: 0.3, tail: 1.0 };
    let t0 = std::time::Instant::now();
    let rep = export(&recording, &release, &cuts, est.factor, "reference", &settings, None, &|_, _| {}).unwrap();
    println!("export took {:?} -> {}", t0.elapsed(), rep.folder.display());
    assert_eq!(rep.files.len(), 3);
    assert!(rep.folder.ends_with("Test Artist/Synthetic Side (1984)"));

    // corrected track 2 must be as long as the original song 2 + its gap (±0.3 s cut tolerance)
    let t2 = jpvinylrec::audio::load(&rep.files[1]).unwrap();
    assert_eq!(t2.rate, rate);
    assert_eq!(t2.bits, 24);
    let expect = true_starts[2] - true_starts[1];
    println!("track 2 length {:.3} s, expected {:.3} s", t2.duration(), expect);
    assert!((t2.duration() - expect).abs() < 0.05);
    let tag = metaflac::Tag::read_from_path(&rep.files[1]).unwrap();
    let title = tag.get_vorbis("TITLE").unwrap().next().unwrap().to_string();
    assert_eq!(title, "Song 2: \"test\"?");
    assert!(rep.files[1].file_name().unwrap().to_str().unwrap() == "02 - Song 2_ _test__.flac");

    // pitch check: the corrected track must line up with the reference at factor ~1
    let rf2 = rec_features(&t2);
    let check = estimate_from_reference(&rf2, &reference).unwrap();
    println!("after correction: residual {:+.4}%", (check.factor - 1.0) * 100.0);
    assert!((check.factor - 1.0).abs() < 0.00005);
}
