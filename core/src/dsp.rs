//! Signal helpers: a streaming resampler with an adjustable ratio, the clock-drift controller
//! that steers it, and level metering.

/// Streaming cubic (Catmull-Rom) resampler whose ratio can change on every call.
///
/// `ratio` = input frames consumed per output frame (e.g. 44100/48000 for a 44.1 kHz mic,
/// 1.0001 for a 48 kHz mic whose clock runs 100 ppm fast).
pub struct Resampler {
    buf: Vec<f32>,
    /// Read position in `buf` (fractional). Always >= 1 so one sample of history is available.
    pos: f64,
}

impl Default for Resampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Resampler {
    pub fn new() -> Self {
        Self { buf: vec![0.0], pos: 1.0 }
    }

    pub fn push(&mut self, input: &[f32]) {
        self.buf.extend_from_slice(input);
    }

    /// Input frames waiting to be consumed (the "fill" the drift controller regulates).
    pub fn pending(&self) -> f64 {
        (self.buf.len() as f64 - self.pos).max(0.0)
    }

    /// Throws away pending input so that only `keep` frames remain (used to recover from stalls).
    pub fn discard_to(&mut self, keep: f64) {
        let excess = (self.pending() - keep).floor();
        if excess > 0.0 {
            self.pos += excess;
            self.compact();
        }
    }

    /// Fills `out` and returns how many frames were produced (fewer if input ran out).
    pub fn process(&mut self, ratio: f64, out: &mut [f32]) -> usize {
        let mut n = 0;
        while n < out.len() {
            let i = self.pos.floor() as usize;
            if i + 2 >= self.buf.len() {
                break;
            }
            let t = (self.pos - i as f64) as f32;
            let (y0, y1, y2, y3) = (self.buf[i - 1], self.buf[i], self.buf[i + 1], self.buf[i + 2]);
            // Catmull-Rom spline
            let a = -0.5 * y0 + 1.5 * y1 - 1.5 * y2 + 0.5 * y3;
            let b = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
            let c = -0.5 * y0 + 0.5 * y2;
            out[n] = ((a * t + b) * t + c) * t + y1;
            n += 1;
            self.pos += ratio;
        }
        self.compact();
        n
    }

    fn compact(&mut self) {
        let i = self.pos.floor() as usize;
        if i > 4096 {
            let drop = (i - 1).min(self.buf.len());
            self.buf.drain(..drop);
            self.pos -= drop as f64;
        }
    }
}

/// Keeps one microphone in step with the master clock.
///
/// Every mic delivers audio at its own (slightly wrong) speed. The controller watches how much
/// audio is waiting in that mic's buffer: if the buffer grows the mic is running fast, so the
/// resampler is told to read a little faster, and vice versa. The integral term converges on the
/// mic's real clock error, which is also reported as `drift_ppm`.
pub struct DriftController {
    nominal: f64,
    target: f64,
    ema: f64,
    integral: f64,
    primed: bool,
    correction: f64,
}

impl DriftController {
    // Loop time constant ≈ target / KP ≈ 3 s; integral time KP/KI ≈ 12 s; fill smoothing ≈ 0.5 s.
    const KP: f64 = 0.02;
    const KI: f64 = 0.001_6;
    const EMA: f64 = 0.01;
    const MAX_CORRECTION: f64 = 0.004; // ±4000 ppm, far beyond any real clock error

    /// `in_rate`/`out_rate` are the mic's and master sample rates; `target_frames` is the
    /// buffer level (in mic frames) to hold — it sets the constant latency of every channel.
    pub fn new(in_rate: f64, out_rate: f64, target_frames: f64) -> Self {
        Self { nominal: in_rate / out_rate, target: target_frames, ema: target_frames, integral: 0.0, primed: false, correction: 0.0 }
    }

    pub fn target(&self) -> f64 {
        self.target
    }

    pub fn is_primed(&self) -> bool {
        self.primed
    }

    /// Call once per block with the current fill; returns the ratio to use for this block,
    /// or `None` while the buffer is still filling up for the first time.
    pub fn update(&mut self, fill: f64, block_secs: f64) -> Option<f64> {
        if !self.primed {
            if fill < self.target {
                return None;
            }
            self.primed = true;
            self.ema = fill;
        }
        self.ema += (fill - self.ema) * Self::EMA;
        let err = (self.ema - self.target) / self.target;
        self.integral = (self.integral + err * block_secs).clamp(-20.0, 20.0);
        self.correction = (Self::KP * err + Self::KI * self.integral).clamp(-Self::MAX_CORRECTION, Self::MAX_CORRECTION);
        Some(self.nominal * (1.0 + self.correction))
    }

    /// Estimated clock error of the mic relative to the master clock, in parts per million.
    pub fn drift_ppm(&self) -> f64 {
        Self::KI * self.integral * 1e6
    }

    pub fn reset_priming(&mut self) {
        self.primed = false;
    }
}

/// Peak and RMS of a block, as linear values.
pub fn peak_rms(block: &[f32]) -> (f32, f32) {
    if block.is_empty() {
        return (0.0, 0.0);
    }
    let mut peak = 0.0f32;
    let mut sum = 0.0f64;
    for &s in block {
        peak = peak.max(s.abs());
        sum += (s as f64) * (s as f64);
    }
    (peak, (sum / block.len() as f64).sqrt() as f32)
}

pub fn to_db(x: f32) -> f32 {
    if x <= 1e-6 {
        -120.0
    } else {
        20.0 * x.log10()
    }
}

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Gentle limiter for the mixed file: transparent below -2 dBFS, never exceeds full scale.
pub fn soft_clip(x: f32) -> f32 {
    const KNEE: f32 = 0.8;
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        x.signum() * (KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_passes_signal_at_unity() {
        let mut r = Resampler::new();
        let input: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.01).sin()).collect();
        r.push(&input);
        let mut out = vec![0.0; 990];
        let n = r.process(1.0, &mut out);
        assert_eq!(n, 990);
        for i in 0..980 {
            assert!((out[i] - input[i]).abs() < 1e-5, "i={i}");
        }
    }

    #[test]
    fn resampler_converts_44k1_to_48k() {
        let mut r = Resampler::new();
        let f = 1000.0;
        let input: Vec<f32> = (0..44100).map(|i| (2.0 * std::f32::consts::PI * f * i as f32 / 44100.0).sin()).collect();
        r.push(&input);
        let mut out = vec![0.0; 48000];
        let n = r.process(44100.0 / 48000.0, &mut out);
        assert!(n > 47_990, "produced {n}");
        // compare with an ideal 1 kHz sine at 48 kHz
        let mut err = 0.0f32;
        for (i, &v) in out.iter().enumerate().take(47_000).skip(100) {
            let t = (i as f64 * 44100.0 / 48000.0 / 44100.0) as f32;
            err = err.max((v - (2.0 * std::f32::consts::PI * f * t).sin()).abs());
        }
        assert!(err < 0.01, "max error {err}");
    }

    #[test]
    fn soft_clip_is_bounded_and_transparent() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert!(soft_clip(5.0) <= 1.0 && soft_clip(-5.0) >= -1.0);
    }
}
