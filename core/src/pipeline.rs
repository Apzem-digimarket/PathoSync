//! The heart of the recorder, free of threads and hardware so it can be tested with simulated mics.
//!
//! Every 5 ms block (`process_block`) the pipeline:
//!  1. takes whatever audio each mic delivered since the last block,
//!  2. resamples it onto the shared 48 kHz master clock, steering each mic's resampler so that its
//!     buffer stays at a fixed level — this cancels each mic's clock drift (the software
//!     "aggregate device"),
//!  3. applies each mic's measured delay (clap test) so that all mics line up,
//!  4. runs the audio through the push-to-talk gates, 0.5 s behind real time (the pre-roll),
//!  5. while recording, sends the multitrack frames and the mono mix to the file writer.

use std::collections::VecDeque;

use crossbeam_channel::Sender;
use serde::Serialize;

use crate::dsp::{db_to_gain, peak_rms, soft_clip, to_db, DriftController, Resampler};
use crate::gate::PttGate;
use crate::writer::WriterMsg;

/// Where a mic's audio comes from (a lock-free queue filled by the sound card callback,
/// or a simulated mic in tests).
pub trait Source: Send {
    /// Appends all audio received since the last call to `out`.
    fn drain(&mut self, out: &mut Vec<f32>);
    /// Frames lost because the queue was full.
    fn overflows(&self) -> u64 {
        0
    }
    /// When the mic last delivered audio, on the master clock (in master frames, fractional).
    /// Sources that can't tell fall back to the block in which the audio was seen.
    fn last_delivery_frame(&self) -> Option<f64> {
        None
    }
}

impl Source for rtrb::Consumer<f32> {
    fn drain(&mut self, out: &mut Vec<f32>) {
        let n = self.slots();
        if n == 0 {
            return;
        }
        if let Ok(chunk) = self.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            out.extend_from_slice(a);
            out.extend_from_slice(b);
            chunk.commit_all();
        }
    }
}

pub struct ChannelSetup {
    pub name: String,
    /// The mic's native sample rate.
    pub in_rate: u32,
    /// Gain applied in the mix (the multitrack file keeps the mic as recorded).
    pub gain_db: f32,
    /// Extra delay for this mic from the clap test, in master frames.
    pub offset_frames: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChannelStatus {
    pub name: String,
    pub peak_db: f32,
    pub rms_db: f32,
    /// Push-to-talk button is held.
    pub held: bool,
    /// Audio from this mic is going into the recording right now.
    pub on_air: bool,
    /// The mic is delivering audio and is locked to the master clock.
    pub locked: bool,
    pub drift_ppm: f64,
    pub noise_floor_db: f32,
    pub dropouts: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Segment {
    pub channel: usize,
    pub start_s: f64,
    pub end_s: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Timeline {
    pub frames: u64,
    pub segments: Vec<Segment>,
    pub dropouts: Vec<u64>,
}

struct Chan {
    setup: ChannelSetup,
    source: Box<dyn Source>,
    rs: Resampler,
    ctl: DriftController,
    tmp: Vec<f32>,
    out: Vec<f32>,
    delay: VecDeque<f32>,
    gate: PttGate,
    gain: f32,
    peak: f32,
    rms: f32,
    underruns: u64,
    resyncs: u64,
    acc_sq: f64,
    acc_n: usize,
    noise_hist: VecDeque<f32>,
    seg_open: Option<u64>,
    segments: Vec<(u64, u64)>,
    dropouts_at_rec_start: u64,
    /// Master frame at which this mic last delivered audio.
    last_delivery: u64,
}

impl Chan {
    fn dropouts(&self) -> u64 {
        self.underruns + self.resyncs + self.source.overflows()
    }
}

struct Recording {
    writer: Sender<WriterMsg>,
    file_frame: u64,
}

struct SyncCapture {
    remaining: usize,
    tracks: Vec<Vec<f32>>,
}

pub struct Pipeline {
    rate: u32,
    block: usize,
    preroll: u64,
    chans: Vec<Chan>,
    now: u64,
    rec: Option<Recording>,
    sync: Option<SyncCapture>,
    sync_result: Option<Result<Vec<u64>, String>>,
}

/// Buffer level each mic is held at. It must comfortably exceed the largest chunk a device
/// delivers at once (USB/wireless receivers: 10–40 ms).
pub const TARGET_LATENCY_SECS: f64 = 0.06;

impl Pipeline {
    pub fn new(rate: u32, block: usize, preroll_ms: u32, tail_ms: u32, chans: Vec<(ChannelSetup, Box<dyn Source>)>) -> Self {
        let preroll = (preroll_ms as u64 * rate as u64) / 1000;
        let tail = (tail_ms as u64 * rate as u64) / 1000;
        let fade = rate / 200; // 5 ms
        let chans = chans
            .into_iter()
            .map(|(setup, source)| {
                let ctl = DriftController::new(setup.in_rate as f64, rate as f64, TARGET_LATENCY_SECS * setup.in_rate as f64);
                let delay_len = (preroll + setup.offset_frames) as usize;
                Chan {
                    gain: db_to_gain(setup.gain_db),
                    ctl,
                    source,
                    rs: Resampler::new(),
                    tmp: Vec::with_capacity(8192),
                    out: vec![0.0; block],
                    delay: VecDeque::from(vec![0.0; delay_len]),
                    gate: PttGate::new(preroll, tail, fade),
                    peak: 0.0,
                    rms: 0.0,
                    underruns: 0,
                    resyncs: 0,
                    acc_sq: 0.0,
                    acc_n: 0,
                    noise_hist: VecDeque::new(),
                    seg_open: None,
                    segments: Vec::new(),
                    dropouts_at_rec_start: 0,
                    last_delivery: 0,
                    setup,
                }
            })
            .collect();
        Self { rate, block, preroll, chans, now: 0, rec: None, sync: None, sync_result: None }
    }

    pub fn now(&self) -> u64 {
        self.now
    }

    pub fn block(&self) -> usize {
        self.block
    }

    pub fn channels(&self) -> usize {
        self.chans.len()
    }

    pub fn is_recording(&self) -> bool {
        self.rec.is_some()
    }

    /// Length of the current recording in frames (0 when not recording).
    pub fn recorded_frames(&self) -> u64 {
        self.rec.as_ref().map_or(0, |r| r.file_frame)
    }

    /// Jumps the master clock forward (after the computer slept) and re-locks every mic.
    pub fn skip_to(&mut self, frame: u64) {
        if frame > self.now {
            self.now = frame;
            self.resync_all();
        }
    }

    pub fn ptt(&mut self, ch: usize, pressed: bool) {
        let now = self.now;
        if let Some(c) = self.chans.get_mut(ch) {
            if pressed {
                c.gate.press(now);
            } else {
                c.gate.release(now);
            }
        }
    }

    /// After the computer was suspended or the program was stalled: start each mic afresh.
    pub fn resync_all(&mut self) {
        for c in &mut self.chans {
            c.ctl.reset_priming();
            c.tmp.clear();
            c.source.drain(&mut c.tmp);
            c.rs.discard_to(0.0);
            c.resyncs += 1;
        }
    }

    pub fn process_block(&mut self) {
        let n = self.block;
        let block_secs = n as f64 / self.rate as f64;
        let o = self.now.saturating_sub(self.preroll); // frame the gates are deciding about
        let recording = self.rec.is_some();
        let file_frame = self.rec.as_ref().map_or(0, |r| r.file_frame);
        let nch = self.chans.len();
        let mut multi = if recording { vec![0.0f32; n * nch] } else { Vec::new() };
        let mut mix = if recording { vec![0.0f32; n] } else { Vec::new() };

        for (ci, c) in self.chans.iter_mut().enumerate() {
            // 1–2: fetch and resample onto the master clock
            c.tmp.clear();
            c.source.drain(&mut c.tmp);
            if !c.tmp.is_empty() {
                c.last_delivery = self.now;
            }
            c.rs.push(&c.tmp);
            let target = c.ctl.target();
            if c.ctl.is_primed() && c.rs.pending() > target * 6.0 {
                // far too much queued (the program was stalled): jump back to the normal level
                c.rs.discard_to(target);
                c.resyncs += 1;
            }
            // Devices deliver audio in chunks (10–40 ms). Audio the mic has already captured but
            // not yet delivered counts as buffered too; otherwise mics with bigger chunks would
            // sit later in time than mics with small chunks.
            let delivered_at = c.source.last_delivery_frame().unwrap_or(c.last_delivery as f64);
            let in_flight = (self.now as f64 - delivered_at) * c.setup.in_rate as f64 / self.rate as f64;
            let fill = c.rs.pending() + in_flight;
            c.out.iter_mut().for_each(|s| *s = 0.0);
            if let Some(ratio) = c.ctl.update(fill, block_secs) {
                let got = c.rs.process(ratio, &mut c.out);
                if got < n {
                    // the mic stopped delivering: wait until its buffer is full again
                    c.underruns += 1;
                    c.ctl.reset_priming();
                }
            }

            // meters and mic-test statistics
            let (p, r) = peak_rms(&c.out);
            c.peak = p;
            c.rms = r;
            c.acc_sq += (r as f64) * (r as f64) * n as f64;
            c.acc_n += n;
            if c.acc_n >= (self.rate / 10) as usize {
                let v = (c.acc_sq / c.acc_n as f64).sqrt() as f32;
                c.noise_hist.push_back(v);
                if c.noise_hist.len() > 300 {
                    c.noise_hist.pop_front();
                }
                c.acc_sq = 0.0;
                c.acc_n = 0;
            }
            if let Some(s) = self.sync.as_mut() {
                s.tracks[ci].extend_from_slice(&c.out);
            }

            // 3–4: alignment delay + pre-roll delay, then the push-to-talk gate
            c.delay.extend(c.out.iter().copied());
            let mut delayed: Vec<f32> = c.delay.drain(..n).collect();
            if recording {
                c.gate.apply(o, &mut delayed);
                for (i, &s) in delayed.iter().enumerate() {
                    let open = c.gate.is_open_at(o + i as u64);
                    track_segment(c, open, file_frame + i as u64);
                    multi[i * nch + ci] = s;
                    mix[i] += s * c.gain;
                }
            }
            c.gate.prune(o);
        }

        if let Some(rec) = self.rec.as_mut() {
            mix.iter_mut().for_each(|s| *s = soft_clip(*s));
            let _ = rec.writer.send(WriterMsg::Block { multi, mix });
            rec.file_frame += n as u64;
        }

        if let Some(s) = self.sync.as_mut() {
            s.remaining = s.remaining.saturating_sub(n);
            if s.remaining == 0 {
                let tracks = std::mem::take(&mut s.tracks);
                self.sync = None;
                self.sync_result = Some(crate::align::clap_offsets(&tracks, self.rate, 200.0));
            }
        }
        self.now += n as u64;
    }

    /// Starts sending audio to `writer`. File frame 0 is the audio from `preroll` before now,
    /// so a button already held when Record is pressed keeps its first word too.
    pub fn start_recording(&mut self, writer: Sender<WriterMsg>) {
        let now = self.now;
        for c in &mut self.chans {
            c.gate.reset(now);
            c.segments.clear();
            c.seg_open = None;
            c.dropouts_at_rec_start = c.dropouts();
        }
        self.rec = Some(Recording { writer, file_frame: 0 });
    }

    /// Flushes the last `preroll` of audio (still waiting in the delay line) and stops sending.
    /// The caller then asks the writer to finish.
    pub fn stop_recording(&mut self) -> Option<(Sender<WriterMsg>, Timeline)> {
        let rec = self.rec.take()?;
        let n = self.preroll as usize;
        let nch = self.chans.len();
        let o = self.now.saturating_sub(self.preroll);
        let mut file_frame = rec.file_frame;
        if n > 0 {
            let mut multi = vec![0.0f32; n * nch];
            let mut mix = vec![0.0f32; n];
            for (ci, c) in self.chans.iter_mut().enumerate() {
                // work on a copy so the live delay line (next recording's pre-roll) stays intact
                let mut pending: Vec<f32> = c.delay.iter().copied().take(n).collect();
                pending.resize(n, 0.0);
                c.gate.apply(o, &mut pending);
                for (i, &s) in pending.iter().enumerate() {
                    let open = c.gate.is_open_at(o + i as u64);
                    track_segment(c, open, file_frame + i as u64);
                    multi[i * nch + ci] = s;
                    mix[i] += s * c.gain;
                }
            }
            mix.iter_mut().for_each(|s| *s = soft_clip(*s));
            let _ = rec.writer.send(WriterMsg::Block { multi, mix });
            file_frame += n as u64;
        }
        let rate = self.rate as f64;
        let mut segments = Vec::new();
        let mut dropouts = Vec::new();
        for (ci, c) in self.chans.iter_mut().enumerate() {
            if let Some(start) = c.seg_open.take() {
                c.segments.push((start, file_frame));
            }
            segments.extend(c.segments.drain(..).map(|(a, b)| Segment { channel: ci, start_s: a as f64 / rate, end_s: b as f64 / rate }));
            dropouts.push(c.dropouts() - c.dropouts_at_rec_start);
        }
        segments.sort_by(|a, b| a.start_s.partial_cmp(&b.start_s).unwrap_or(std::cmp::Ordering::Equal));
        Some((rec.writer, Timeline { frames: file_frame, segments, dropouts }))
    }

    /// Records `secs` of all mics for the clap test; the result appears in `take_sync_result`.
    pub fn start_sync_test(&mut self, secs: f32) {
        let frames = (secs * self.rate as f32) as usize;
        self.sync_result = None;
        self.sync = Some(SyncCapture { remaining: frames, tracks: vec![Vec::with_capacity(frames); self.chans.len()] });
    }

    pub fn sync_test_running(&self) -> bool {
        self.sync.is_some()
    }

    /// Delays (in frames) found by the last clap test, one per mic.
    pub fn take_sync_result(&mut self) -> Option<Result<Vec<u64>, String>> {
        self.sync_result.take()
    }

    pub fn status(&self) -> Vec<ChannelStatus> {
        let o = self.now.saturating_sub(self.preroll);
        self.chans
            .iter()
            .map(|c| {
                let floor = if c.noise_hist.is_empty() {
                    -120.0
                } else {
                    let mut v: Vec<f32> = c.noise_hist.iter().copied().collect();
                    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    to_db(v[v.len() / 10])
                };
                ChannelStatus {
                    name: c.setup.name.clone(),
                    peak_db: to_db(c.peak),
                    rms_db: to_db(c.rms),
                    held: c.gate.is_held(),
                    on_air: self.rec.is_some() && c.gate.is_open_at(o),
                    locked: c.ctl.is_primed(),
                    drift_ppm: c.ctl.drift_ppm(),
                    noise_floor_db: floor,
                    dropouts: c.dropouts(),
                }
            })
            .collect()
    }
}

fn track_segment(c: &mut Chan, open: bool, frame: u64) {
    match (open, c.seg_open) {
        (true, None) => c.seg_open = Some(frame),
        (false, Some(start)) => {
            c.segments.push((start, frame));
            c.seg_open = None;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::WriterMsg;
    use parking_lot::Mutex;
    use std::sync::Arc;

    /// A simulated microphone: its clock runs `ppm` fast/slow and it delivers audio in chunks,
    /// like a USB or wireless receiver does. It "hears" short clicks at given true times.
    struct SimMic {
        rate: f64,
        ppm: f64,
        chunk: usize,
        produced: u64,
        clicks: Vec<f64>,
        queue: Arc<Mutex<Vec<f32>>>,
        delivered: Arc<Mutex<Option<f64>>>,
    }

    struct SimSource(Arc<Mutex<Vec<f32>>>, Arc<Mutex<Option<f64>>>);
    impl Source for SimSource {
        fn drain(&mut self, out: &mut Vec<f32>) {
            out.append(&mut self.0.lock());
        }
        fn last_delivery_frame(&self) -> Option<f64> {
            *self.1.lock()
        }
    }

    impl SimMic {
        fn new(rate: f64, ppm: f64, chunk: usize, clicks: Vec<f64>) -> (Self, SimSource) {
            let q = Arc::new(Mutex::new(Vec::new()));
            let d = Arc::new(Mutex::new(None));
            (Self { rate, ppm, chunk, produced: 0, clicks, queue: q.clone(), delivered: d.clone() }, SimSource(q, d))
        }
        /// Deliver every complete chunk the mic has captured by true time `t`.
        fn advance_to(&mut self, t: f64) {
            let real_rate = self.rate * (1.0 + self.ppm * 1e-6);
            let captured = (t * real_rate) as u64;
            while self.produced + self.chunk as u64 <= captured {
                let mut q = self.queue.lock();
                for k in self.produced..self.produced + self.chunk as u64 {
                    let true_time = k as f64 / real_rate;
                    let hit = self.clicks.iter().any(|&c| true_time >= c && true_time < c + 0.002);
                    q.push(if hit { 0.9 } else { 0.0 });
                }
                self.produced += self.chunk as u64;
                // delivered the moment its last sample was captured (master clock = true time)
                *self.delivered.lock() = Some(self.produced as f64 / real_rate * 48_000.0);
            }
        }
    }

    fn setup(name: &str, rate: u32) -> ChannelSetup {
        ChannelSetup { name: name.into(), in_rate: rate, gain_db: 0.0, offset_frames: 0 }
    }

    /// Runs mics with different clocks for `secs` of simulated time with all buttons held, and
    /// returns where each mic's clicks ended up in the multitrack recording (in seconds).
    fn run(mics: Vec<SimMic>, sources: Vec<SimSource>, secs: f64, rates: &[u32]) -> Vec<Vec<f64>> {
        let chans: Vec<(ChannelSetup, Box<dyn Source>)> =
            sources.into_iter().enumerate().map(|(i, s)| (setup(&format!("Mic {i}"), rates[i]), Box::new(s) as Box<dyn Source>)).collect();
        let mut p = Pipeline::new(48_000, 240, 500, 300, chans);
        let mut mics = mics;
        let (tx, rx) = crossbeam_channel::unbounded();
        for ch in 0..mics.len() {
            p.ptt(ch, true);
        }
        p.start_recording(tx);
        let blocks = (secs * 200.0) as usize;
        for b in 0..blocks {
            let t = (b + 1) as f64 / 200.0;
            for m in mics.iter_mut() {
                m.advance_to(t);
            }
            p.process_block();
        }
        let (_tx, tl) = p.stop_recording().unwrap();
        assert_eq!(tl.dropouts.iter().sum::<u64>(), 0, "no dropouts expected: {:?}", tl.dropouts);
        let nch = mics.len();
        let mut found = vec![Vec::new(); nch];
        let mut frame = 0u64;
        let mut last_hit = vec![-1.0f64; nch];
        for msg in rx.try_iter() {
            if let WriterMsg::Block { multi, .. } = msg {
                for f in 0..multi.len() / nch {
                    for c in 0..nch {
                        let t = (frame + f as u64) as f64 / 48_000.0;
                        if multi[f * nch + c] > 0.45 && t - last_hit[c] > 0.5 {
                            found[c].push(t);
                            last_hit[c] = t;
                        }
                    }
                }
                frame += (multi.len() / nch) as u64;
            }
        }
        found
    }

    #[test]
    fn mics_with_different_clocks_stay_in_sync() {
        // one click every 10 s for 20 minutes; three mics: -150 ppm, +220 ppm, and a 44.1 kHz
        // mic +80 ppm, each delivering audio in different chunk sizes.
        let secs = 1200.0;
        let clicks: Vec<f64> = (1..120).map(|i| i as f64 * 10.0).collect();
        let (m1, s1) = SimMic::new(48_000.0, -150.0, 480, clicks.clone());
        let (m2, s2) = SimMic::new(48_000.0, 220.0, 1024, clicks.clone());
        let (m3, s3) = SimMic::new(44_100.0, 80.0, 441, clicks.clone());
        let found = run(vec![m1, m2, m3], vec![s1, s2, s3], secs, &[48_000, 48_000, 44_100]);
        for f in &found {
            assert!(f.len() >= 115, "clicks found: {}", f.len());
        }
        // without correction mic 1 and 2 would be 0.44 s apart after 20 minutes
        let mut worst: f64 = 0.0;
        let mut latency = Vec::new();
        let debug = std::env::var("PATHOSYNC_DEBUG").is_ok();
        for (k, &t0) in found[0].iter().enumerate().skip(1) {
            let mut row = Vec::new();
            for f in &found[1..] {
                if let Some(t) = f.iter().find(|&&t| (t - t0).abs() < 0.5) {
                    worst = worst.max((t - t0).abs());
                    row.push((t - t0) * 1000.0);
                }
            }
            if debug && (k < 4 || k % 20 == 0) {
                eprintln!("t={:>5.0}s latency={:.2}ms  others vs mic1 (ms): {:?}", clicks[k], (t0 - clicks[k]) * 1000.0, row.iter().map(|v| format!("{v:+.2}")).collect::<Vec<_>>());
            }
            latency.push(t0 - clicks[k]);
        }
        assert!(worst < 0.003, "mics drifted apart by {:.1} ms", worst * 1000.0);
        let (lo, hi) = latency.iter().fold((f64::MAX, f64::MIN), |(a, b), &x| (a.min(x), b.max(x)));
        assert!(hi - lo < 0.004, "latency wandered by {:.1} ms", (hi - lo) * 1000.0);
    }

    #[test]
    fn bluetooth_rate_mics_are_heard_and_in_sync() {
        // Bluetooth headset mics run at 16 kHz or 8 kHz and deliver bursty chunks; check they
        // are recorded (not silent) and line up with a 48 kHz wired mic.
        let clicks: Vec<f64> = (1..12).map(|i| i as f64 * 5.0).collect();
        let (m1, s1) = SimMic::new(48_000.0, 0.0, 480, clicks.clone());
        let (m2, s2) = SimMic::new(16_000.0, 300.0, 480, clicks.clone());
        let (m3, s3) = SimMic::new(8_000.0, -300.0, 60, clicks.clone());
        let found = run(vec![m1, m2, m3], vec![s1, s2, s3], 62.0, &[48_000, 16_000, 8_000]);
        for (c, f) in found.iter().enumerate() {
            assert!(f.len() >= 10, "mic {c}: clicks found {}", f.len());
        }
        for (k, &t0) in found[0].iter().enumerate().skip(2) {
            for f in &found[1..] {
                let t = f.iter().find(|&&t| (t - t0).abs() < 0.5).expect("click missing");
                assert!((t - t0).abs() < 0.003, "click {k}: {:.1} ms apart", (t - t0) * 1000.0);
            }
        }
    }

    #[test]
    fn push_to_talk_keeps_the_preroll_and_mutes_the_rest() {
        let q = Arc::new(Mutex::new(Vec::new()));
        let chans: Vec<(ChannelSetup, Box<dyn Source>)> = vec![(setup("A", 48_000), Box::new(SimSource(q.clone(), Arc::new(Mutex::new(None)))))];
        let mut p = Pipeline::new(48_000, 240, 500, 300, chans);
        let (tx, rx) = crossbeam_channel::unbounded();
        p.start_recording(tx);
        // steady tone, button pressed at 3.0 s and released at 4.0 s
        for b in 0..1200 {
            q.lock().extend(std::iter::repeat_n(0.5f32, 240));
            if b == 600 {
                p.ptt(0, true);
            }
            if b == 800 {
                p.ptt(0, false);
            }
            p.process_block();
        }
        let (_tx, tl) = p.stop_recording().unwrap();
        let mut mix = Vec::new();
        for m in rx.try_iter() {
            if let WriterMsg::Block { mix: x, .. } = m {
                mix.extend(x);
            }
        }
        // file frame 0 = 0.5 s before Record; press at 3.0 s → audio opens 0.5 s earlier
        let at = |s: f64| mix[(s * 48_000.0) as usize];
        assert_eq!(at(2.9), 0.0);
        assert!((at(3.05) - 0.5).abs() < 1e-3, "pre-roll missing");
        assert!((at(4.2) - 0.5).abs() < 1e-3);
        assert!((at(4.75) - 0.5).abs() < 1e-3, "tail missing");
        assert_eq!(at(4.9), 0.0);
        assert_eq!(tl.segments.len(), 1);
        let s = &tl.segments[0];
        assert!((s.start_s - 3.0).abs() < 0.01 && (s.end_s - 4.8).abs() < 0.01, "{s:?}");
    }

    #[test]
    fn end_to_end_saves_correct_files() {
        // two mics, only mic 2 talks (button held 1.0 s → 2.0 s); read the WAV files back
        let qa = Arc::new(Mutex::new(Vec::new()));
        let qb = Arc::new(Mutex::new(Vec::new()));
        let none = || Arc::new(Mutex::new(None));
        let chans: Vec<(ChannelSetup, Box<dyn Source>)> =
            vec![(setup("A", 48_000), Box::new(SimSource(qa.clone(), none()))), (setup("B", 48_000), Box::new(SimSource(qb.clone(), none())))];
        let mut p = Pipeline::new(48_000, 240, 500, 300, chans);
        let dir = std::env::temp_dir().join(format!("pathosync-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let err = Arc::new(Mutex::new(None));
        let plan = crate::writer::WriterPlan { dir: dir.clone(), base: "CASE".into(), channels: 2, sample_rate: 48_000, max_part_frames: 48_000 * 3600 };
        let (tx, h) = crate::writer::spawn(plan, err).unwrap();
        p.start_recording(tx);
        for b in 0..800 {
            qa.lock().extend(std::iter::repeat_n(0.3f32, 240));
            qb.lock().extend(std::iter::repeat_n(-0.4f32, 240));
            if b == 300 { p.ptt(1, true); }
            if b == 500 { p.ptt(1, false); }
            p.process_block();
        }
        let (tx, tl) = p.stop_recording().unwrap();
        let (rtx, rrx) = crossbeam_channel::bounded(1);
        tx.send(WriterMsg::Finish(rtx)).unwrap();
        let files = rrx.recv().unwrap().unwrap();
        h.join().unwrap();
        assert_eq!(files.frames, tl.frames);
        assert_eq!(tl.frames, 800 * 240 + 24_000, "4 s of blocks + the 0.5 s flushed at stop");
        let multi = hound::WavReader::open(&files.multitrack[0]).unwrap();
        assert_eq!((multi.spec().channels, multi.spec().sample_rate, multi.spec().bits_per_sample), (2, 48_000, 24));
        let s: Vec<i32> = multi.into_samples().map(|x| x.unwrap()).collect();
        let at = |sec: f64, ch: usize| s[(sec * 48_000.0) as usize * 2 + ch] as f32 / 8_388_607.0;
        // The file starts 0.5 s before Record, so file time = clock time + 0.5 s:
        // press at 1.5 s → 2.0 s in the file, kept from 1.5 s (pre-roll);
        // release at 2.5 s → 3.0 s in the file, open until 3.3 s (tail).
        assert_eq!(at(2.5, 0), 0.0, "mic A never pressed: silent");
        assert_eq!(at(1.45, 1), 0.0);
        assert!((at(1.55, 1) + 0.4).abs() < 1e-3, "pre-roll missing");
        assert!((at(3.25, 1) + 0.4).abs() < 1e-3, "tail missing");
        assert_eq!(at(3.4, 1), 0.0);
        let mix = hound::WavReader::open(&files.mix[0]).unwrap();
        assert_eq!(mix.spec().channels, 1);
        assert_eq!(mix.duration() as u64, tl.frames);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clap_test_offsets_are_applied() {
        // mic B hears everything 20 ms late; after setting its offset on mic A both line up
        let clicks = vec![2.0, 6.0];
        let (mut a, sa) = SimMic::new(48_000.0, 0.0, 480, clicks.clone());
        let (mut b, sb) = SimMic::new(48_000.0, 0.0, 480, clicks.iter().map(|c| c + 0.020).collect());
        let mut p = Pipeline::new(48_000, 240, 500, 300, vec![(setup("A", 48_000), Box::new(sa)), (setup("B", 48_000), Box::new(sb))]);
        p.start_sync_test(4.0);
        let mut result = None;
        for blk in 0..1000 {
            let t = (blk + 1) as f64 / 200.0;
            a.advance_to(t);
            b.advance_to(t);
            p.process_block();
            if let Some(r) = p.take_sync_result() {
                result = Some(r);
                break;
            }
        }
        let off = result.expect("test finished").expect("clap found");
        assert!((off[0] as f64 / 48.0 - 20.0).abs() < 0.5, "{off:?}");
        assert!(off[1] < 24, "{off:?}");
    }
}
