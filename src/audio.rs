//! Decoding audio files (FLAC, WAV, MP3, AAC/M4A, ALAC, Ogg Vorbis) into planar f32.

use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Planar audio in memory. Samples are f32 in [-1, 1].
#[derive(Clone, Debug)]
pub struct Audio {
    pub rate: u32,
    /// Bits per sample of the source file (used to pick the output bit depth).
    pub bits: u32,
    /// One Vec per channel, all the same length.
    pub data: Vec<Vec<f32>>,
}

impl Audio {
    pub fn channels(&self) -> usize {
        self.data.len()
    }
    pub fn frames(&self) -> usize {
        self.data.first().map(|c| c.len()).unwrap_or(0)
    }
    pub fn duration(&self) -> f64 {
        self.frames() as f64 / self.rate as f64
    }
    /// Mono mixdown.
    pub fn mono(&self) -> Vec<f32> {
        let n = self.frames();
        let ch = self.channels().max(1) as f32;
        let mut out = vec![0.0f32; n];
        for c in &self.data {
            for (o, s) in out.iter_mut().zip(c) {
                *o += *s;
            }
        }
        for o in &mut out {
            *o /= ch;
        }
        out
    }
    /// Peak absolute sample value across channels.
    pub fn peak(&self) -> f32 {
        self.data
            .iter()
            .flat_map(|c| c.iter())
            .fold(0.0f32, |m, s| m.max(s.abs()))
    }
    /// Number of samples at (or extremely close to) full scale: a sign of clipping in the recording.
    pub fn clipped_samples(&self) -> usize {
        self.data
            .iter()
            .flat_map(|c| c.iter())
            .filter(|s| s.abs() >= 0.9999)
            .count()
    }
}

pub fn load(path: &Path) -> Result<Audio> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .with_context(|| format!("unsupported or corrupt audio file: {}", path.display()))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("no audio track in {}", path.display()))?;
    let track_id = track.id;
    let params = track.codec_params.clone();
    let rate = params.sample_rate.ok_or_else(|| anyhow!("unknown sample rate"))?;
    let bits = params.bits_per_sample.unwrap_or(16);
    let mut decoder = symphonia::default::get_codecs().make(&params, &DecoderOptions::default())?;

    let mut data: Vec<Vec<f32>> = Vec::new();
    let mut sbuf: Option<SampleBuffer<f32>> = None;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(_)) => continue, // skip a damaged packet
            Err(e) => return Err(e.into()),
        };
        let spec = *decoded.spec();
        let nch = spec.channels.count();
        if data.is_empty() {
            data = vec![Vec::new(); nch];
        }
        let buf = sbuf.get_or_insert_with(|| SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
        if (buf.capacity()) < decoded.capacity() * nch {
            *buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        }
        buf.copy_interleaved_ref(decoded);
        for frame in buf.samples().chunks_exact(nch) {
            for (c, s) in frame.iter().enumerate() {
                data[c].push(*s);
            }
        }
    }
    if data.is_empty() || data[0].is_empty() {
        return Err(anyhow!("{} decoded to no audio", path.display()));
    }
    Ok(Audio { rate, bits, data })
}
