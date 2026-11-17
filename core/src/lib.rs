//! PathoSync audio engine: records several microphones at once, keeps them in sync, gates each
//! one with push-to-talk and saves one mixed file plus one multitrack file.

pub mod align;
pub mod dsp;
pub mod engine;
pub mod gate;
pub mod pipeline;
pub mod writer;

/// Master sample rate of every recording.
pub const SAMPLE_RATE: u32 = 48_000;
/// Processing block: 5 ms.
pub const BLOCK: usize = 240;

pub use engine::{list_input_devices, DeviceInfo, Engine, EngineConfig, EngineStatus, MicConfig, RecordingResult};
