//! Turns the finished WAV recording into MP3 files that play on any computer or phone:
//! one MP3 of the mix, and one MP3 per microphone (MP3 can't hold 3–4 separate channels).
//!
//! The WAV files stay the crash-safe format while recording; this runs once, after Stop.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use mp3lame_encoder::{Bitrate, Builder, Encoder, FlushNoGap, Mode, MonoPcm, Quality};

use crate::writer;

const CHUNK_FRAMES: usize = 48_000;

struct Mp3File {
    enc: Encoder,
    out: BufWriter<File>,
    buf: Vec<u8>,
    path: PathBuf,
}

impl Mp3File {
    fn create(path: PathBuf, sample_rate: u32) -> Result<Self, String> {
        let fail = |e: &dyn std::fmt::Display| format!("MP3 encoder: {e}");
        let mut b = Builder::new().ok_or("MP3 encoder could not start")?;
        b.set_num_channels(1).map_err(|e| fail(&e))?;
        b.set_sample_rate(sample_rate).map_err(|e| fail(&e))?;
        b.set_mode(Mode::Mono).map_err(|e| fail(&e))?;
        b.set_brate(Bitrate::Kbps128).map_err(|e| fail(&e))?;
        b.set_quality(Quality::Good).map_err(|e| fail(&e))?;
        let enc = b.build().map_err(|e| fail(&e))?;
        let out = BufWriter::new(File::create(&path).map_err(|e| format!("Could not create {}: {e}", writer::file_name(&path)))?);
        Ok(Self { enc, out, buf: Vec::new(), path })
    }

    fn push(&mut self, samples: &[f32]) -> Result<(), String> {
        self.buf.clear();
        self.buf.reserve(mp3lame_encoder::max_required_buffer_size(samples.len()));
        self.enc.encode_to_vec(MonoPcm(samples), &mut self.buf).map_err(|e| format!("MP3 encoder: {e}"))?;
        self.out.write_all(&self.buf).map_err(|e| format!("Could not write {}: {e}", writer::file_name(&self.path)))
    }

    fn finish(mut self) -> Result<PathBuf, String> {
        self.buf.clear();
        self.buf.reserve(7200);
        self.enc.flush_to_vec::<FlushNoGap>(&mut self.buf).map_err(|e| format!("MP3 encoder: {e}"))?;
        self.out.write_all(&self.buf).and_then(|_| self.out.flush()).map_err(|e| format!("Could not write {}: {e}", writer::file_name(&self.path)))?;
        Ok(self.path)
    }
}

/// Reads WAV parts one after the other and hands `CHUNK_FRAMES` interleaved frames at a time,
/// as floats in -1..1, to `each`.
fn read_parts(parts: &[PathBuf], mut each: impl FnMut(&[f32], u16) -> Result<(), String>) -> Result<(), String> {
    let mut chunk = Vec::new();
    for part in parts {
        let mut rd = hound::WavReader::open(part).map_err(|e| format!("Could not read {}: {e}", writer::file_name(part)))?;
        let ch = rd.spec().channels;
        let scale = match rd.spec().sample_format {
            hound::SampleFormat::Int => 1.0 / (1u32 << (rd.spec().bits_per_sample - 1)) as f32,
            hound::SampleFormat::Float => 1.0,
        };
        let float = rd.spec().sample_format == hound::SampleFormat::Float;
        let per_chunk = CHUNK_FRAMES * ch as usize;
        chunk.clear();
        if float {
            for s in rd.samples::<f32>() {
                chunk.push(s.map_err(|e| e.to_string())?);
                if chunk.len() == per_chunk {
                    each(&chunk, ch)?;
                    chunk.clear();
                }
            }
        } else {
            for s in rd.samples::<i32>() {
                chunk.push(s.map_err(|e| e.to_string())? as f32 * scale);
                if chunk.len() == per_chunk {
                    each(&chunk, ch)?;
                    chunk.clear();
                }
            }
        }
        if !chunk.is_empty() {
            each(&chunk, ch)?;
        }
    }
    Ok(())
}

/// Writes `<base>_mix.mp3` and `<base>_<n>-<mic>.mp3` for each microphone. Returns the files,
/// mix first.
pub fn convert(dir: &Path, base: &str, mix: &[PathBuf], multitrack: &[PathBuf], mic_labels: &[String], sample_rate: u32) -> Result<Vec<PathBuf>, String> {
    let mut done = Vec::new();

    let mut mix_mp3 = Mp3File::create(dir.join(format!("{base}_mix.mp3")), sample_rate)?;
    read_parts(mix, |chunk, _| mix_mp3.push(chunk))?;
    done.push(mix_mp3.finish()?);

    let mut per_mic: Vec<Mp3File> = Vec::new();
    for (i, label) in mic_labels.iter().enumerate() {
        let name = match writer::safe_name(label) {
            s if s.is_empty() => format!("Mic{}", i + 1),
            s => s,
        };
        per_mic.push(Mp3File::create(dir.join(format!("{base}_{}-{name}.mp3", i + 1)), sample_rate)?);
    }
    let mut mono = Vec::with_capacity(CHUNK_FRAMES);
    read_parts(multitrack, |chunk, ch| {
        let ch = ch as usize;
        for (c, f) in per_mic.iter_mut().enumerate().take(ch) {
            mono.clear();
            mono.extend(chunk.iter().skip(c).step_by(ch).copied());
            f.push(&mono)?;
        }
        Ok(())
    })?;
    for f in per_mic {
        done.push(f.finish()?);
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(path: &Path, channels: u16, secs: f32, freq: &[f32]) {
        let spec = hound::WavSpec { channels, sample_rate: 48_000, bits_per_sample: 24, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for n in 0..(secs * 48_000.0) as usize {
            for f in freq.iter().take(channels as usize) {
                let v = (2.0 * std::f32::consts::PI * f * n as f32 / 48_000.0).sin() * 0.5;
                w.write_sample((v * 8_388_607.0) as i32).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    #[test]
    fn makes_a_mix_mp3_and_one_per_mic() {
        let dir = std::env::temp_dir().join(format!("pathosync-mp3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mix = vec![dir.join("a_mix.wav"), dir.join("a_mix_part2.wav")];
        wav(&mix[0], 1, 2.0, &[440.0]);
        wav(&mix[1], 1, 1.0, &[440.0]);
        let multi = vec![dir.join("a_multitrack.wav")];
        wav(&multi[0], 2, 3.0, &[440.0, 880.0]);
        let out = convert(&dir, "a", &mix, &multi, &["Dr Rao".into(), "".into()], 48_000).unwrap();
        let names: Vec<_> = out.iter().map(|p| writer::file_name(p)).collect();
        assert_eq!(names, vec!["a_mix.mp3", "a_1-Dr_Rao.mp3", "a_2-Mic2.mp3"]);
        for p in &out {
            let bytes = std::fs::read(p).unwrap();
            // 3 s at 128 kbit/s ≈ 48 kB; every MP3 frame starts with 11 set bits
            assert!((40_000..60_000).contains(&bytes.len()), "{}: {} bytes", writer::file_name(p), bytes.len());
            assert!(bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0, "not an MP3 frame header");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
