//! Speed-correct, cut and write tagged FLAC files.

use crate::audio::Audio;
use crate::meta::{Release, Source, Track};
use crate::resample::{resample, Kernel};
use anyhow::{anyhow, Context, Result};
use flacenc::component::BitRepr;
use flacenc::error::Verify;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Settings {
    /// Library root, e.g. \\192.168.1.138\Media\Music\Audio. Files go to root/Album Artist/Album (Year)/.
    pub root: PathBuf,
    /// 16 or 24.
    pub bits: u32,
    pub embed_cover: bool,
    pub overwrite: bool,
    /// Seconds of lead-in kept before the first track's music (faded in).
    pub lead_in: f64,
    /// Seconds kept after the last track's music (faded out).
    pub tail: f64,
}

/// One output track: database info plus its span in the *recording* (uncorrected seconds).
#[derive(Clone, Debug)]
pub struct Cut {
    pub track: Track,
    pub start: f64,
    pub end: f64,
}

pub struct Report {
    pub files: Vec<PathBuf>,
    pub clipped_samples: usize,
    pub folder: PathBuf,
}

/// Make a string safe as a Windows/Linux file or folder name.
pub fn sanitize(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 32 => '_',
            c => c,
        })
        .collect();
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    let out = out.trim().to_string();
    let reserved = ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "LPT1", "LPT2"];
    if out.is_empty() || reserved.contains(&out.to_uppercase().as_str()) {
        format!("_{out}")
    } else {
        out
    }
}

pub fn album_folder(root: &Path, release: &Release) -> PathBuf {
    let album = match release.year() {
        Some(y) => format!("{} ({})", release.title, y),
        None => release.title.clone(),
    };
    root.join(sanitize(&release.artist)).join(sanitize(&album))
}

pub fn file_name(t: &Track) -> String {
    format!("{:02} - {}.flac", t.number, sanitize(&t.title))
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
}

fn to_ints(planar: &[Vec<f32>], bits: u32, dither: bool, clipped: &mut usize) -> Vec<i32> {
    let max = ((1i64 << (bits - 1)) - 1) as f32;
    let min = -(1i64 << (bits - 1)) as f32;
    let n = planar[0].len();
    let ch = planar.len();
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut out = Vec::with_capacity(n * ch);
    for i in 0..n {
        for c in planar {
            let mut v = c[i] * max;
            if dither {
                v += rng.next() - rng.next(); // TPDF, ±1 LSB
            }
            let r = v.round();
            if r > max || r < min {
                *clipped += 1;
            }
            out.push(r.clamp(min, max) as i32);
        }
    }
    out
}

fn write_flac(path: &Path, interleaved: &[i32], channels: usize, bits: u32, rate: u32) -> Result<()> {
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|e| anyhow!("flac config: {e:?}"))?;
    let source = flacenc::source::MemSource::from_samples(interleaved, channels, bits as usize, rate as usize);
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|e| anyhow!("flac encode: {e:?}"))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream.write(&mut sink).map_err(|e| anyhow!("flac write: {e:?}"))?;
    std::fs::write(path, sink.as_slice()).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Write planar f32 audio as a plain (untagged) FLAC file.
pub fn write_planar(path: &Path, planar: &[Vec<f32>], rate: u32, bits: u32) -> Result<()> {
    let mut clipped = 0;
    let ints = to_ints(planar, bits, bits <= 16, &mut clipped);
    write_flac(path, &ints, planar.len(), bits, rate)
}

fn image_mime(data: &[u8]) -> &'static str {
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else {
        "image/jpeg"
    }
}

#[allow(clippy::too_many_arguments)]
fn tag(
    path: &Path,
    release: &Release,
    t: &Track,
    total: usize,
    factor: f64,
    method: &str,
    cover: Option<&[u8]>,
) -> Result<()> {
    let mut tag = metaflac::Tag::read_from_path(path)?;
    let mut set = |k: &str, v: &str| {
        if !v.trim().is_empty() {
            tag.set_vorbis(k, vec![v.to_string()]);
        }
    };
    set("TITLE", &t.title);
    set("ARTIST", t.artist.as_deref().unwrap_or(&release.artist));
    set("ALBUMARTIST", &release.artist);
    set("ALBUM", &release.title);
    set("TRACKNUMBER", &t.number.to_string());
    set("TRACKTOTAL", &total.to_string());
    set("TOTALTRACKS", &total.to_string());
    if let Some(y) = release.year() {
        set("DATE", y);
    }
    if let Some(y) = &release.original_year {
        set("ORIGINALDATE", y);
        set("ORIGINALYEAR", y);
    }
    if let Some(l) = &release.label {
        set("LABEL", l);
    }
    if let Some(c) = &release.catno {
        set("CATALOGNUMBER", c);
    }
    if let Some(c) = &release.country {
        set("RELEASECOUNTRY", c);
    }
    set("MEDIA", "Vinyl");
    match &release.source {
        Source::Discogs(id) => {
            set("DISCOGS_RELEASE_ID", &id.to_string());
            set("URL_DISCOGS_RELEASE_SITE", &release.url());
        }
        Source::MusicBrainz(id) => {
            set("MUSICBRAINZ_ALBUMID", id);
            if let Some(rg) = &release.mb_release_group {
                set("MUSICBRAINZ_RELEASEGROUPID", rg);
            }
        }
    }
    if let Some(r) = &t.mb_recording_id {
        set("MUSICBRAINZ_TRACKID", r);
    }
    if let Some(r) = &t.mb_track_id {
        set("MUSICBRAINZ_RELEASETRACKID", r);
    }
    set(
        "COMMENT",
        &format!(
            "Vinyl rip, side position {}. Speed corrected ×{:.5} ({}).",
            t.position, factor, method
        ),
    );
    set("ENCODER", "JPVinylRec");
    if let Some(img) = cover {
        tag.add_picture(image_mime(img), metaflac::block::PictureType::CoverFront, img.to_vec());
    }
    tag.save()?;
    Ok(())
}

/// Export the given cuts. `factor` = recorded/true duration (1.0 = no correction).
/// `progress(fraction, message)` is called as tracks are written.
#[allow(clippy::too_many_arguments)]
pub fn export(
    audio: &Audio,
    release: &Release,
    cuts: &[Cut],
    factor: f64,
    method: &str,
    settings: &Settings,
    cover: Option<Vec<u8>>,
    progress: &dyn Fn(f32, String),
) -> Result<Report> {
    if cuts.is_empty() {
        return Err(anyhow!("nothing to export"));
    }
    if settings.bits != 16 && settings.bits != 24 {
        return Err(anyhow!("bit depth must be 16 or 24"));
    }
    let folder = album_folder(&settings.root, release);
    std::fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
    if !settings.overwrite {
        for c in cuts {
            let p = folder.join(file_name(&c.track));
            if p.exists() {
                return Err(anyhow!(
                    "{} already exists. Tick \"Overwrite existing files\" to replace it.",
                    p.display()
                ));
            }
        }
    }
    if let Some(img) = &cover {
        let ext = if image_mime(img) == "image/png" { "png" } else { "jpg" };
        let p = folder.join(format!("cover.{ext}"));
        if settings.overwrite || !p.exists() {
            let _ = std::fs::write(&p, img);
        }
    }

    let rate = audio.rate as f64;
    let kernel = Kernel::new(factor);
    let chans: Vec<&[f32]> = audio.data.iter().map(|c| c.as_slice()).collect();
    let dither = settings.bits < audio.bits.max(17);
    let mut files = Vec::new();
    let mut clipped = 0;
    let total = release.tracks.len();

    for (i, c) in cuts.iter().enumerate() {
        progress(i as f32 / cuts.len() as f32, format!("Writing {} – {}", c.track.position, c.track.title));
        let first = i == 0;
        let last = i + 1 == cuts.len();
        let start = if first { (c.start - settings.lead_in).max(0.0) } else { c.start };
        let end = if last { (c.end + settings.tail).min(audio.duration()) } else { c.end };
        let out_len = (((end - start) * rate) / factor).round().max(1.0) as usize;
        let mut planar = resample(&chans, start * rate, factor, out_len, &kernel);
        if first {
            let n = ((settings.lead_in.min(0.3)) * rate) as usize;
            for ch in &mut planar {
                for (k, s) in ch.iter_mut().take(n).enumerate() {
                    *s *= k as f32 / n as f32;
                }
            }
        }
        if last {
            let n = ((settings.tail.min(1.5)) * rate) as usize;
            for ch in &mut planar {
                let len = ch.len();
                for k in 0..n.min(len) {
                    ch[len - 1 - k] *= k as f32 / n as f32;
                }
            }
        }
        let ints = to_ints(&planar, settings.bits, dither, &mut clipped);
        let path = folder.join(file_name(&c.track));
        write_flac(&path, &ints, planar.len(), settings.bits, audio.rate)?;
        tag(
            &path,
            release,
            &c.track,
            total,
            factor,
            method,
            if settings.embed_cover { cover.as_deref() } else { None },
        )?;
        files.push(path);
    }
    progress(1.0, format!("Done: {} tracks written", files.len()));
    Ok(Report { files, clipped_samples: clipped, folder })
}
