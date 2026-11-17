//! Writes a recording to disk on its own thread, so slow disks never disturb the audio.
//!
//! Two files are written at the same time:
//!   `<base>_multitrack.wav` – one channel per microphone (24-bit, 48 kHz)
//!   `<base>_mix.wav`        – all microphones mixed to one mono track (24-bit, 48 kHz)
//! The WAV headers are refreshed every second, so after a crash or power cut the files still
//! open and contain everything up to the last second. Very long recordings continue in
//! `<base>_part2_…` files before the 4 GB WAV limit is reached.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;

type Wav = hound::WavWriter<BufWriter<File>>;

pub struct WriterPlan {
    pub dir: PathBuf,
    pub base: String,
    pub channels: u16,
    pub sample_rate: u32,
    /// Start a new part after this many frames (default: 1 hour).
    pub max_part_frames: u64,
}

pub enum WriterMsg {
    /// `multi` is interleaved (frame-major, `channels` samples per frame); `mix` is mono.
    Block { multi: Vec<f32>, mix: Vec<f32> },
    Finish(Sender<Result<WrittenFiles, String>>),
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct WrittenFiles {
    pub multitrack: Vec<PathBuf>,
    pub mix: Vec<PathBuf>,
    pub frames: u64,
}

/// Turns a case number into something safe for a Windows file name.
pub fn safe_name(s: &str) -> String {
    let cleaned: String = s
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect();
    let cleaned = cleaned.trim_matches(|c| c == '.' || c == '_').to_string();
    cleaned.chars().take(60).collect()
}

fn spec(channels: u16, sample_rate: u32) -> hound::WavSpec {
    hound::WavSpec { channels, sample_rate, bits_per_sample: 24, sample_format: hound::SampleFormat::Int }
}

fn to_i24(x: f32) -> i32 {
    (x.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32
}

struct Part {
    multi: Wav,
    mix: Wav,
    frames: u64,
}

struct State {
    plan: WriterPlan,
    part_no: u32,
    part: Option<Part>,
    files: WrittenFiles,
    since_flush: u64,
    error: Arc<Mutex<Option<String>>>,
}

impl State {
    fn names(&self) -> (PathBuf, PathBuf) {
        let suffix = if self.part_no > 1 { format!("_part{}", self.part_no) } else { String::new() };
        (
            self.plan.dir.join(format!("{}{}_multitrack.wav", self.plan.base, suffix)),
            self.plan.dir.join(format!("{}{}_mix.wav", self.plan.base, suffix)),
        )
    }

    fn open_part(&mut self) -> Result<(), String> {
        self.part_no += 1;
        let (m, x) = self.names();
        let multi = Wav::create(&m, spec(self.plan.channels, self.plan.sample_rate)).map_err(|e| format!("Could not create {}: {e}", m.display()))?;
        let mix = Wav::create(&x, spec(1, self.plan.sample_rate)).map_err(|e| format!("Could not create {}: {e}", x.display()))?;
        self.files.multitrack.push(m);
        self.files.mix.push(x);
        self.part = Some(Part { multi, mix, frames: 0 });
        Ok(())
    }

    fn close_part(&mut self) -> Result<(), String> {
        if let Some(p) = self.part.take() {
            p.multi.finalize().map_err(|e| format!("Could not finish the multitrack file: {e}"))?;
            p.mix.finalize().map_err(|e| format!("Could not finish the mix file: {e}"))?;
        }
        Ok(())
    }

    fn write(&mut self, multi: &[f32], mix: &[f32]) -> Result<(), String> {
        let ch = self.plan.channels as usize;
        let frames = mix.len();
        let mut done = 0;
        while done < frames {
            if self.part.as_ref().is_none_or(|p| p.frames >= self.plan.max_part_frames) {
                self.close_part()?;
                self.open_part()?;
            }
            let part = self.part.as_mut().expect("part is open");
            let room = (self.plan.max_part_frames - part.frames) as usize;
            let n = room.min(frames - done);
            for f in done..done + n {
                for c in 0..ch {
                    part.multi.write_sample(to_i24(multi[f * ch + c])).map_err(|e| format!("Writing failed (is the disk full?): {e}"))?;
                }
                part.mix.write_sample(to_i24(mix[f])).map_err(|e| format!("Writing failed (is the disk full?): {e}"))?;
            }
            part.frames += n as u64;
            self.files.frames += n as u64;
            self.since_flush += n as u64;
            done += n;
        }
        if self.since_flush >= self.plan.sample_rate as u64 {
            self.since_flush = 0;
            if let Some(p) = self.part.as_mut() {
                // hound's flush also rewrites the header, keeping the files playable after a crash
                p.multi.flush().map_err(|e| format!("Saving failed (is the disk full?): {e}"))?;
                p.mix.flush().map_err(|e| format!("Saving failed (is the disk full?): {e}"))?;
            }
        }
        Ok(())
    }
}

/// Starts the writer thread. Errors while recording are reported through `error` straight away
/// (so the screen can warn) and again when the recording is finished.
pub fn spawn(plan: WriterPlan, error: Arc<Mutex<Option<String>>>) -> Result<(Sender<WriterMsg>, JoinHandle<()>), String> {
    std::fs::create_dir_all(&plan.dir).map_err(|e| format!("Could not create the folder {}: {e}", plan.dir.display()))?;
    let mut st = State { plan, part_no: 0, part: None, files: WrittenFiles::default(), since_flush: 0, error };
    st.open_part()?;
    let (tx, rx): (Sender<WriterMsg>, Receiver<WriterMsg>) = crossbeam_channel::unbounded();
    let handle = std::thread::Builder::new()
        .name("pathosync-writer".into())
        .spawn(move || {
            let mut failed: Option<String> = None;
            for msg in rx {
                match msg {
                    WriterMsg::Block { multi, mix } => {
                        if failed.is_none() {
                            if let Err(e) = st.write(&multi, &mix) {
                                *st.error.lock() = Some(e.clone());
                                failed = Some(e);
                            }
                        }
                    }
                    WriterMsg::Finish(reply) => {
                        let closed = st.close_part();
                        let result = match (failed.take(), closed) {
                            (Some(e), _) | (None, Err(e)) => Err(e),
                            (None, Ok(())) => Ok(st.files.clone()),
                        };
                        let _ = reply.send(result);
                        return;
                    }
                }
            }
            let _ = st.close_part();
        })
        .map_err(|e| format!("Could not start the writer: {e}"))?;
    Ok((tx, handle))
}

pub fn file_name(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names() {
        assert_eq!(safe_name(" S26/1234 "), "S26_1234");
        assert_eq!(safe_name("..\\..\\x"), "x");
        assert_eq!(safe_name(""), "");
    }

    #[test]
    fn writes_both_files_and_splits_parts() {
        let dir = std::env::temp_dir().join(format!("pathosync-writer-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let err = Arc::new(Mutex::new(None));
        let plan = WriterPlan { dir: dir.clone(), base: "T".into(), channels: 3, sample_rate: 48_000, max_part_frames: 1000 };
        let (tx, h) = spawn(plan, err).unwrap();
        let frames = 2500;
        let multi: Vec<f32> = (0..frames * 3).map(|i| ((i % 3) as f32 - 1.0) * 0.5).collect();
        let mix = vec![0.25f32; frames];
        tx.send(WriterMsg::Block { multi, mix }).unwrap();
        let (rtx, rrx) = crossbeam_channel::bounded(1);
        tx.send(WriterMsg::Finish(rtx)).unwrap();
        let files = rrx.recv().unwrap().unwrap();
        h.join().unwrap();
        assert_eq!(files.frames, 2500);
        assert_eq!(files.multitrack.len(), 3);
        assert!(files.mix[1].ends_with("T_part2_mix.wav"));
        let r = hound::WavReader::open(&files.multitrack[0]).unwrap();
        assert_eq!(r.spec().channels, 3);
        assert_eq!(r.duration(), 1000);
        let s: Vec<i32> = r.into_samples().take(3).map(|s| s.unwrap()).collect();
        assert_eq!(s, vec![to_i24(-0.5), 0, to_i24(0.5)]);
        let last = hound::WavReader::open(&files.mix[2]).unwrap();
        assert_eq!(last.duration(), 500);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
