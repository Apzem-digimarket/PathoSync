//! Connects the pipeline to real microphones (WASAPI on Windows, CoreAudio on macOS) and runs it.
//!
//! Threads:
//!  * sound-card callbacks (one per device) copy each mic's samples into a lock-free queue;
//!  * the "devices" thread owns the open streams for as long as the engine runs;
//!  * the "master" thread is the master clock: every few ms it runs the pipeline for all the
//!    5 ms blocks that are due, and publishes levels/status for the screen;
//!  * a writer thread per recording does all disk work.
//!
//! Several mics may share one device (e.g. a 4-input audio interface): each mic picks an input
//! channel of its device and one stream feeds all of them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::pipeline::{ChannelSetup, ChannelStatus, Pipeline, Source};
use crate::writer::{self, WriterMsg, WriterPlan};
use crate::{BLOCK, SAMPLE_RATE};

#[derive(Clone, Debug, Serialize)]
pub struct DeviceInfo {
    /// Stable identifier (survives restarts); store this in settings.
    pub id: String,
    pub name: String,
    pub channels: u16,
    pub sample_rate: u32,
    pub is_default: bool,
    /// Set when Windows lists the microphone but won't say how to record from it.
    pub problem: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MicConfig {
    pub name: String,
    pub device_id: String,
    /// Shown if the device can't be found by id (e.g. plugged into another USB port).
    #[serde(default)]
    pub device_name: String,
    /// 0-based input channel of the device (an audio interface has several).
    #[serde(default)]
    pub input_channel: u16,
    #[serde(default)]
    pub gain_db: f32,
    /// Extra delay found by the clap test.
    #[serde(default)]
    pub offset_ms: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EngineConfig {
    pub mics: Vec<MicConfig>,
    pub preroll_ms: u32,
    pub tail_ms: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct MicStatus {
    #[serde(flatten)]
    pub ch: ChannelStatus,
    pub device: String,
    pub error: Option<String>,
    /// "ok", "starting", "no_stream" (Windows sends nothing) or "silent" (Windows sends only
    /// digital silence: muted, volume 0, or microphone access blocked).
    pub signal: &'static str,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct EngineStatus {
    pub mics: Vec<MicStatus>,
    pub recording: bool,
    pub recorded_secs: f64,
    pub writer_error: Option<String>,
    pub clap_test_running: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct TalkSegment {
    pub mic: String,
    pub channel: usize,
    pub start_s: f64,
    pub end_s: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecordingResult {
    pub folder: PathBuf,
    pub mix: Vec<PathBuf>,
    pub multitrack: Vec<PathBuf>,
    pub info: PathBuf,
    pub duration_s: f64,
    pub segments: Vec<TalkSegment>,
    pub warnings: Vec<String>,
}

/// All microphones Windows (or macOS) offers for recording. Microphones that can't be used are
/// still listed, with the reason in `problem`, so they never just vanish from the list.
pub fn list_input_devices() -> Result<Vec<DeviceInfo>, String> {
    let host = cpal::default_host();
    let default_id = host.default_input_device().and_then(|d| d.id().ok()).map(|i| i.to_string());
    let devices = host.input_devices().map_err(|e| format!("Could not list microphones: {e}"))?;
    let mut out = Vec::new();
    for (n, d) in devices.enumerate() {
        let name = d.description().map(|x| x.name().to_string()).unwrap_or_else(|_| d.to_string());
        let id = match d.id() {
            Ok(i) => i.to_string(),
            Err(e) => {
                out.push(DeviceInfo { id: format!("unusable:{n}"), name, channels: 1, sample_rate: SAMPLE_RATE, is_default: false, problem: Some(format!("Windows gives no ID for it ({e})")) });
                continue;
            }
        };
        let is_default = Some(&id) == default_id.as_ref();
        match input_config(&d) {
            Ok(cfg) => out.push(DeviceInfo { is_default, id, name, channels: cfg.channels(), sample_rate: cfg.sample_rate(), problem: None }),
            Err(e) => out.push(DeviceInfo { is_default, id, name, channels: 1, sample_rate: SAMPLE_RATE, problem: Some(e) }),
        }
    }
    Ok(out)
}

/// The format to record a device in: its default format, or else the best format it says it
/// supports (some Bluetooth and jack-detected mics fail the default query).
fn input_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig, String> {
    let default_err = match device.default_input_config() {
        Ok(c) => return Ok(c),
        Err(e) => e.to_string(),
    };
    let ranges: Vec<_> = device.supported_input_configs().map(|r| r.collect()).unwrap_or_default();
    let pick = |r: &cpal::SupportedStreamConfigRange| {
        let rate = SAMPLE_RATE.clamp(r.min_sample_rate(), r.max_sample_rate());
        (*r).with_sample_rate(rate)
    };
    let score = |c: &cpal::SupportedStreamConfig| (c.sample_rate() == SAMPLE_RATE, c.sample_format() == cpal::SampleFormat::F32, c.sample_rate());
    ranges.iter().map(pick).max_by_key(score).ok_or(default_err)
}

fn find_device(host: &cpal::Host, id: &str, name: &str) -> Option<cpal::Device> {
    if let Ok(parsed) = id.parse::<cpal::DeviceId>() {
        if let Some(d) = host.device_by_id(&parsed) {
            return Some(d);
        }
    }
    if name.is_empty() {
        return None;
    }
    host.input_devices().ok()?.find(|d| d.description().map(|x| x.name() == name).unwrap_or(false))
}

/// The queue between a sound-card callback and the master thread.
struct DeviceSource {
    queue: rtrb::Consumer<f32>,
    overflow: Arc<AtomicU64>,
    /// Nanoseconds after engine start of the last callback (0 = none yet).
    last_ns: Arc<AtomicU64>,
}

impl Source for DeviceSource {
    fn drain(&mut self, out: &mut Vec<f32>) {
        self.queue.drain(out);
    }
    fn overflows(&self) -> u64 {
        self.overflow.load(Ordering::Relaxed)
    }
    fn last_delivery_frame(&self) -> Option<f64> {
        match self.last_ns.load(Ordering::Acquire) {
            0 => None,
            ns => Some(ns as f64 * SAMPLE_RATE as f64 / 1e9),
        }
    }
}

struct Route {
    channel: usize,
    queue: rtrb::Producer<f32>,
    overflow: Arc<AtomicU64>,
    /// Nanoseconds after engine start of the last non-zero sample (0 = none yet).
    sound_ns: Arc<AtomicU64>,
}

fn build_stream<T>(device: &cpal::Device, config: cpal::StreamConfig, mut routes: Vec<Route>, last_ns: Arc<AtomicU64>, start: Instant, errors: Arc<Mutex<Option<String>>>) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + Send + 'static,
    f32: cpal::FromSample<T>,
{
    let channels = config.channels as usize;
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _| {
                let frames = data.len() / channels;
                let now_ns = start.elapsed().as_nanos().max(1) as u64;
                for r in routes.iter_mut() {
                    if data.iter().skip(r.channel).step_by(channels).any(|s| cpal::Sample::to_sample::<f32>(*s) != 0.0) {
                        r.sound_ns.store(now_ns, Ordering::Relaxed);
                    }
                    match r.queue.write_chunk_uninit(frames) {
                        Ok(chunk) => {
                            chunk.fill_from_iter(data.iter().skip(r.channel).step_by(channels).map(|s| cpal::Sample::to_sample::<f32>(*s)));
                        }
                        Err(_) => {
                            r.overflow.fetch_add(frames as u64, Ordering::Relaxed);
                        }
                    }
                }
                last_ns.store(now_ns, Ordering::Release);
            },
            move |err: cpal::Error| {
                let msg = match err.kind() {
                    cpal::ErrorKind::Xrun => return,
                    cpal::ErrorKind::DeviceNotAvailable => "Microphone disconnected".to_string(),
                    _ => format!("Microphone error: {err}"),
                };
                *errors.lock() = Some(msg);
            },
            None,
        )
        .map_err(|e| e.to_string())
}

struct Opened {
    mic: usize,
    source: DeviceSource,
    rate: u32,
    device: String,
    probe: Probe,
}

/// Lets the master thread tell "no audio arriving" apart from "audio arriving, all zeros".
#[derive(Clone)]
struct Probe {
    last_ns: Arc<AtomicU64>,
    sound_ns: Arc<AtomicU64>,
}

impl Probe {
    fn signal(&self, now_ns: u64) -> &'static str {
        const GRACE: u64 = 2_500_000_000;
        let last = self.last_ns.load(Ordering::Relaxed);
        let sound = self.sound_ns.load(Ordering::Relaxed);
        if now_ns < GRACE {
            "starting"
        } else if last == 0 || now_ns.saturating_sub(last) > GRACE {
            "no_stream"
        } else if sound == 0 || now_ns.saturating_sub(sound) > GRACE {
            "silent"
        } else {
            "ok"
        }
    }
}

/// Opens every device needed by `mics` on the calling thread. Returns the queues per mic and the
/// streams (which must stay alive, and on the same thread, while recording).
fn open_devices(mics: &[MicConfig], start: Instant, errors: &[Arc<Mutex<Option<String>>>]) -> Result<(Vec<Opened>, Vec<cpal::Stream>), String> {
    let host = cpal::default_host();
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, m) in mics.iter().enumerate() {
        match groups.iter_mut().find(|g| g.0 == m.device_id) {
            Some(g) => g.1.push(i),
            None => groups.push((m.device_id.clone(), vec![i])),
        }
    }
    let mut opened = Vec::new();
    let mut streams = Vec::new();
    for (dev_id, idxs) in groups {
        let first = &mics[idxs[0]];
        let label = |m: &MicConfig| if m.name.is_empty() { "A microphone".to_string() } else { format!("\"{}\"", m.name) };
        let device = find_device(&host, &dev_id, &first.device_name)
            .ok_or_else(|| format!("{}: the microphone \"{}\" is not connected.", label(first), if first.device_name.is_empty() { &dev_id } else { &first.device_name }))?;
        let dev_name = device.description().map(|x| x.name().to_string()).unwrap_or_else(|_| device.to_string());
        let sup = input_config(&device).map_err(|e| format!("{}: Windows won't let PathoSync record from {dev_name} ({e}).", label(first)))?;
        let mut config = sup.config();
        config.buffer_size = cpal::BufferSize::Default;
        let last_ns = Arc::new(AtomicU64::new(0));
        let mut routes = Vec::new();
        for &i in &idxs {
            let m = &mics[i];
            if m.input_channel >= config.channels {
                return Err(format!("{}: {dev_name} has no input {} (it has {}).", label(m), m.input_channel + 1, config.channels));
            }
            let (prod, cons) = rtrb::RingBuffer::new(config.sample_rate as usize * 2);
            let overflow = Arc::new(AtomicU64::new(0));
            let sound_ns = Arc::new(AtomicU64::new(0));
            routes.push(Route { channel: m.input_channel as usize, queue: prod, overflow: overflow.clone(), sound_ns: sound_ns.clone() });
            let probe = Probe { last_ns: last_ns.clone(), sound_ns };
            opened.push(Opened { mic: i, source: DeviceSource { queue: cons, overflow, last_ns: last_ns.clone() }, rate: config.sample_rate, device: dev_name.clone(), probe });
        }
        let errs = errors[idxs[0]].clone();
        use cpal::SampleFormat as F;
        let stream = match sup.sample_format() {
            F::F32 => build_stream::<f32>(&device, config, routes, last_ns, start, errs),
            F::F64 => build_stream::<f64>(&device, config, routes, last_ns, start, errs),
            F::I16 => build_stream::<i16>(&device, config, routes, last_ns, start, errs),
            F::I32 => build_stream::<i32>(&device, config, routes, last_ns, start, errs),
            F::I24 => build_stream::<cpal::I24>(&device, config, routes, last_ns, start, errs),
            F::U16 => build_stream::<u16>(&device, config, routes, last_ns, start, errs),
            F::U8 => build_stream::<u8>(&device, config, routes, last_ns, start, errs),
            F::I8 => build_stream::<i8>(&device, config, routes, last_ns, start, errs),
            other => Err(format!("unsupported sample format {other}")),
        }
        .map_err(|e| format!("{}: could not open {dev_name} ({e}).", label(first)))?;
        stream.play().map_err(|e| format!("{}: could not start {dev_name} ({e}).", label(first)))?;
        streams.push(stream);
    }
    opened.sort_by_key(|o| o.mic);
    Ok((opened, streams))
}

enum Cmd {
    Ptt(usize, bool),
    StartRec(Sender<WriterMsg>),
    StopRec(Sender<Option<(Sender<WriterMsg>, crate::pipeline::Timeline)>>),
    ClapTest(f32),
    Shutdown,
}

struct Shared {
    status: Mutex<EngineStatus>,
    clap_result: Mutex<Option<Result<Vec<u64>, String>>>,
    writer_error: Arc<Mutex<Option<String>>>,
    mic_errors: Vec<Arc<Mutex<Option<String>>>>,
}

struct ActiveRecording {
    folder: PathBuf,
    base: String,
    case: String,
    started: chrono::DateTime<chrono::Local>,
    writer: JoinHandle<()>,
}

pub struct Engine {
    cfg: EngineConfig,
    devices: Vec<String>,
    cmd: Sender<Cmd>,
    shared: Arc<Shared>,
    master: Option<JoinHandle<()>>,
    device_thread: Option<JoinHandle<()>>,
    stop_devices: Option<Sender<()>>,
    rec: Mutex<Option<ActiveRecording>>,
}

impl Engine {
    /// Opens all configured mics and starts the master clock (levels show straight away).
    pub fn start(cfg: EngineConfig) -> Result<Engine, String> {
        if cfg.mics.is_empty() {
            return Err("Choose at least one microphone.".into());
        }
        let start = Instant::now();
        let mic_errors: Vec<_> = cfg.mics.iter().map(|_| Arc::new(Mutex::new(None))).collect();

        // The streams live on their own thread (some audio systems require that).
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let (stop_tx, stop_rx): (Sender<()>, Receiver<()>) = crossbeam_channel::bounded(1);
        let mics = cfg.mics.clone();
        let errs = mic_errors.clone();
        let device_thread = std::thread::Builder::new()
            .name("pathosync-devices".into())
            .spawn(move || match open_devices(&mics, start, &errs) {
                Ok((opened, streams)) => {
                    let _ = ready_tx.send(Ok(opened));
                    let _ = stop_rx.recv();
                    drop(streams);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| e.to_string())?;
        let opened = match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                let _ = device_thread.join();
                return Err(e);
            }
            Err(_) => return Err("The microphones did not respond. Unplug and reconnect them, then try again.".into()),
        };

        let devices: Vec<String> = opened.iter().map(|o| o.device.clone()).collect();
        let probes: Vec<Probe> = opened.iter().map(|o| o.probe.clone()).collect();
        let chans: Vec<(ChannelSetup, Box<dyn Source>)> = opened
            .into_iter()
            .map(|o| {
                let m = &cfg.mics[o.mic];
                let setup = ChannelSetup {
                    name: m.name.clone(),
                    in_rate: o.rate,
                    gain_db: m.gain_db,
                    offset_frames: ((m.offset_ms.clamp(0.0, 500.0) / 1000.0) * SAMPLE_RATE as f32) as u64,
                };
                (setup, Box::new(o.source) as Box<dyn Source>)
            })
            .collect();
        let pipeline = Pipeline::new(SAMPLE_RATE, BLOCK, cfg.preroll_ms, cfg.tail_ms, chans);

        let shared = Arc::new(Shared {
            status: Mutex::new(EngineStatus::default()),
            clap_result: Mutex::new(None),
            writer_error: Arc::new(Mutex::new(None)),
            mic_errors,
        });
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let sh = shared.clone();
        let devs = devices.clone();
        let master = std::thread::Builder::new()
            .name("pathosync-master".into())
            .spawn(move || master_loop(pipeline, cmd_rx, sh, devs, probes, start))
            .map_err(|e| e.to_string())?;

        Ok(Engine { cfg, devices, cmd: cmd_tx, shared, master: Some(master), device_thread: Some(device_thread), stop_devices: Some(stop_tx), rec: Mutex::new(None) })
    }

    pub fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    pub fn ptt(&self, mic: usize, pressed: bool) {
        let _ = self.cmd.send(Cmd::Ptt(mic, pressed));
    }

    pub fn status(&self) -> EngineStatus {
        self.shared.status.lock().clone()
    }

    pub fn is_recording(&self) -> bool {
        self.rec.lock().is_some()
    }

    /// Starts a recording in `<folder>/<case>_<date>_<time>/`.
    pub fn start_recording(&self, folder: &Path, case: &str) -> Result<PathBuf, String> {
        let mut rec = self.rec.lock();
        if rec.is_some() {
            return Err("Already recording.".into());
        }
        let started = chrono::Local::now();
        let stem = match writer::safe_name(case) {
            s if s.is_empty() => "Recording".to_string(),
            s => s,
        };
        let mut base = format!("{stem}_{}", started.format("%Y-%m-%d_%H%M%S"));
        let mut dir = folder.join(&base);
        let mut n = 2;
        while dir.exists() {
            base = format!("{stem}_{}_{n}", started.format("%Y-%m-%d_%H%M%S"));
            dir = folder.join(&base);
            n += 1;
        }
        *self.shared.writer_error.lock() = None;
        let plan = WriterPlan { dir: dir.clone(), base: base.clone(), channels: self.cfg.mics.len() as u16, sample_rate: SAMPLE_RATE, max_part_frames: SAMPLE_RATE as u64 * 3600 };
        let (tx, handle) = writer::spawn(plan, self.shared.writer_error.clone())?;
        let active = ActiveRecording { folder: dir.clone(), base, case: case.trim().to_string(), started, writer: handle };
        // Written now so an interrupted recording can still be identified; replaced when it ends.
        let _ = std::fs::write(active.folder.join(format!("{}_info.json", active.base)), self.info_json(&active, None, None));
        self.cmd.send(Cmd::StartRec(tx)).map_err(|_| "The recorder has stopped. Restart PathoSync.".to_string())?;
        *rec = Some(active);
        Ok(dir)
    }

    pub fn stop_recording(&self) -> Result<RecordingResult, String> {
        let active = self.rec.lock().take().ok_or("Not recording.")?;
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        self.cmd.send(Cmd::StopRec(reply_tx)).map_err(|_| "The recorder has stopped unexpectedly.".to_string())?;
        let (writer_tx, timeline) = reply_rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "The recorder did not respond.".to_string())?.ok_or("Not recording.")?;
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        let _ = writer_tx.send(WriterMsg::Finish(done_tx));
        let files = done_rx.recv_timeout(Duration::from_secs(120)).map_err(|_| "Saving took too long.".to_string())??;

        let mut warnings = Vec::new();
        for (i, &d) in timeline.dropouts.iter().enumerate() {
            if d > 0 {
                warnings.push(format!("{}: the microphone briefly stopped sending sound {d} time(s) during this recording.", self.mic_label(i)));
            }
        }
        if timeline.segments.is_empty() {
            warnings.insert(0, "Nothing was recorded: no microphone was ON AIR, so the files are silent. PathoSync records only while a mic's key (or its \"Hold to talk\" button) is held down - hold it while you speak.".to_string());
        }
        for (i, &peak) in timeline.peaks.iter().enumerate() {
            let held: f64 = timeline.segments.iter().filter(|s| s.channel == i).map(|s| s.end_s - s.start_s).sum();
            if held > 0.0 && peak < 1e-4 {
                warnings.insert(0, format!("{}: was ON AIR for {held:.0} s but picked up no sound, so its part of the recording is silent. Check the microphone (see the message on its tile).", self.mic_label(i)));
            }
        }
        let segments: Vec<TalkSegment> =
            timeline.segments.iter().map(|s| TalkSegment { mic: self.mic_label(s.channel), channel: s.channel + 1, start_s: s.start_s, end_s: s.end_s }).collect();
        let info = active.folder.join(format!("{}_info.json", active.base));
        std::fs::write(&info, self.info_json(&active, Some((&files, &timeline)), Some(&segments))).map_err(|e| format!("Could not save the info file: {e}"))?;
        let _ = active.writer.join(); // already finished: it replied above
        Ok(RecordingResult {
            folder: active.folder,
            mix: files.mix,
            multitrack: files.multitrack,
            info,
            duration_s: timeline.frames as f64 / SAMPLE_RATE as f64,
            segments,
            warnings,
        })
    }

    /// Records `secs` from all mics; clap once during that time. See `clap_test_result`.
    pub fn start_clap_test(&self, secs: f32) {
        *self.shared.clap_result.lock() = None;
        let _ = self.cmd.send(Cmd::ClapTest(secs));
    }

    /// Delay to add to each mic, in ms (relative to the mics' current offsets).
    pub fn clap_test_result(&self) -> Option<Result<Vec<f32>, String>> {
        self.shared.clap_result.lock().take().map(|r| r.map(|v| v.iter().map(|&f| f as f32 * 1000.0 / SAMPLE_RATE as f32).collect()))
    }

    fn mic_label(&self, i: usize) -> String {
        match self.cfg.mics.get(i) {
            Some(m) if !m.name.trim().is_empty() => m.name.trim().to_string(),
            _ => format!("Mic {}", i + 1),
        }
    }

    fn info_json(&self, a: &ActiveRecording, done: Option<(&writer::WrittenFiles, &crate::pipeline::Timeline)>, segments: Option<&[TalkSegment]>) -> String {
        let hms = |s: f64| {
            let ms = (s * 1000.0).round() as u64;
            format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
        };
        let names = |v: &[PathBuf]| v.iter().map(|p| writer::file_name(p)).collect::<Vec<_>>();
        let channels: Vec<_> = self
            .cfg
            .mics
            .iter()
            .enumerate()
            .map(|(i, m)| {
                serde_json::json!({
                    "channel": i + 1,
                    "mic": self.mic_label(i),
                    "device": self.devices.get(i).cloned().unwrap_or_default(),
                    "device_input": m.input_channel + 1,
                    "mix_gain_db": m.gain_db,
                    "sync_offset_ms": m.offset_ms,
                })
            })
            .collect();
        let mut v = serde_json::json!({
            "app": "PathoSync",
            "version": env!("CARGO_PKG_VERSION"),
            "case": a.case,
            "started": a.started.to_rfc3339(),
            "status": if done.is_some() { "complete" } else { "recording (if this stays, the recording was interrupted; the audio files still contain everything up to the interruption)" },
            "sample_rate": SAMPLE_RATE,
            "bit_depth": 24,
            "push_to_talk": { "pre_roll_ms": self.cfg.preroll_ms, "release_tail_ms": self.cfg.tail_ms },
            "multitrack_channels": channels,
        });
        if let (Some((files, tl)), Some(segs)) = (done, segments) {
            v["finished"] = serde_json::json!(chrono::Local::now().to_rfc3339());
            v["duration_seconds"] = serde_json::json!((tl.frames as f64 / SAMPLE_RATE as f64 * 1000.0).round() / 1000.0);
            v["files"] = serde_json::json!({ "mix": names(&files.mix), "multitrack": names(&files.multitrack) });
            v["talk_segments"] = serde_json::json!(segs
                .iter()
                .map(|s| serde_json::json!({ "mic": s.mic, "channel": s.channel, "start": hms(s.start_s), "end": hms(s.end_s), "start_seconds": (s.start_s * 1000.0).round() / 1000.0, "end_seconds": (s.end_s * 1000.0).round() / 1000.0 }))
                .collect::<Vec<_>>());
            let mut dropouts = serde_json::Map::new();
            for (i, d) in tl.dropouts.iter().enumerate() {
                dropouts.insert(self.mic_label(i), serde_json::json!(d));
            }
            v["dropouts"] = serde_json::Value::Object(dropouts);
        }
        serde_json::to_string_pretty(&v).unwrap_or_default()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Never lose a recording because the program is closed.
        if self.is_recording() {
            let _ = self.stop_recording();
        }
        let _ = self.cmd.send(Cmd::Shutdown);
        if let Some(m) = self.master.take() {
            let _ = m.join();
        }
        if let Some(s) = self.stop_devices.take() {
            let _ = s.send(());
        }
        if let Some(d) = self.device_thread.take() {
            let _ = d.join();
        }
    }
}

fn master_loop(mut p: Pipeline, cmds: Receiver<Cmd>, shared: Arc<Shared>, devices: Vec<String>, probes: Vec<Probe>, start: Instant) {
    let rate = SAMPLE_RATE as f64;
    loop {
        for cmd in cmds.try_iter() {
            match cmd {
                Cmd::Ptt(ch, on) => p.ptt(ch, on),
                Cmd::StartRec(tx) => p.start_recording(tx),
                Cmd::StopRec(reply) => {
                    let _ = reply.send(p.stop_recording());
                }
                Cmd::ClapTest(secs) => p.start_sync_test(secs),
                Cmd::Shutdown => return,
            }
        }
        let due = (start.elapsed().as_secs_f64() * rate) as u64;
        if due > p.now() + 2 * SAMPLE_RATE as u64 {
            // The computer was asleep or the program was frozen for over 2 s.
            p.skip_to(due.saturating_sub(BLOCK as u64));
        }
        while p.now() + BLOCK as u64 <= due {
            p.process_block();
        }
        if let Some(r) = p.take_sync_result() {
            *shared.clap_result.lock() = Some(r);
        }
        let now_ns = start.elapsed().as_nanos() as u64;
        let st = EngineStatus {
            mics: p
                .status()
                .into_iter()
                .enumerate()
                .map(|(i, ch)| MicStatus { ch, device: devices.get(i).cloned().unwrap_or_default(), error: shared.mic_errors[i].lock().clone(), signal: probes.get(i).map_or("ok", |pr| pr.signal(now_ns)) })
                .collect(),
            recording: p.is_recording(),
            recorded_secs: p.recorded_frames() as f64 / rate,
            writer_error: shared.writer_error.lock().clone(),
            clap_test_running: p.sync_test_running(),
        };
        *shared.status.lock() = st;
        std::thread::sleep(Duration::from_millis(2));
    }
}
