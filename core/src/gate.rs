//! Push-to-talk gate for one microphone.
//!
//! Button presses and releases are stamped with the master-clock frame at which they happened.
//! The recorder runs `preroll` frames behind real time, so when a button goes down at frame `p`
//! the audio from `p - preroll` onwards can still be let through: the first word is never cut,
//! even if the button is pressed a moment late. After release the mic stays open for `tail`
//! frames so the last word isn't clipped either. Opening and closing use a short fade (no clicks).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Press {
    down: u64,
    up: Option<u64>,
}

pub struct PttGate {
    preroll: u64,
    tail: u64,
    fade_step: f32,
    presses: Vec<Press>,
    gain: f32,
}

impl PttGate {
    /// `fade_frames`: length of the fade in/out (e.g. 5 ms = 240 frames at 48 kHz).
    pub fn new(preroll: u64, tail: u64, fade_frames: u32) -> Self {
        Self { preroll, tail, fade_step: 1.0 / fade_frames.max(1) as f32, presses: Vec::new(), gain: 0.0 }
    }

    pub fn preroll(&self) -> u64 {
        self.preroll
    }

    /// Button went down at master frame `now`. Repeated presses (key auto-repeat) are ignored.
    pub fn press(&mut self, now: u64) {
        if !self.is_held() {
            self.presses.push(Press { down: now, up: None });
        }
    }

    /// Button released at master frame `now`.
    pub fn release(&mut self, now: u64) {
        if let Some(p) = self.presses.last_mut() {
            if p.up.is_none() {
                p.up = Some(now.max(p.down));
            }
        }
    }

    pub fn is_held(&self) -> bool {
        self.presses.last().is_some_and(|p| p.up.is_none())
    }

    /// Whether audio at master frame `f` belongs to a talk segment.
    pub fn is_open_at(&self, f: u64) -> bool {
        self.presses.iter().any(|p| {
            let from = p.down.saturating_sub(self.preroll);
            match p.up {
                None => f >= from,
                Some(up) => f >= from && f < up + self.tail,
            }
        })
    }

    /// Applies the gate (with fades) to `block`, whose first sample is master frame `first_frame`.
    pub fn apply(&mut self, first_frame: u64, block: &mut [f32]) {
        for (i, s) in block.iter_mut().enumerate() {
            let target = if self.is_open_at(first_frame + i as u64) { 1.0 } else { 0.0 };
            if self.gain < target {
                self.gain = (self.gain + self.fade_step).min(1.0);
            } else if self.gain > target {
                self.gain = (self.gain - self.fade_step).max(0.0);
            }
            *s *= self.gain;
        }
    }

    /// Forgets presses that can no longer affect frames at or after `frame`.
    pub fn prune(&mut self, frame: u64) {
        let tail = self.tail;
        self.presses.retain(|p| p.up.is_none_or(|up| up + tail > frame));
    }

    /// Drops all presses (e.g. when the recorder is restarted), keeping a held button held.
    pub fn reset(&mut self, now: u64) {
        let held = self.is_held();
        self.presses.clear();
        self.gain = 0.0;
        if held {
            self.presses.push(Press { down: now, up: None });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preroll_and_tail_extend_the_press() {
        let mut g = PttGate::new(100, 50, 1);
        g.press(1000);
        g.release(2000);
        assert!(!g.is_open_at(899));
        assert!(g.is_open_at(900)); // 100 frames of pre-roll
        assert!(g.is_open_at(2049)); // 50 frames of tail
        assert!(!g.is_open_at(2050));
    }

    #[test]
    fn key_repeat_does_not_restart_the_press() {
        let mut g = PttGate::new(0, 0, 1);
        g.press(10);
        g.press(20);
        g.press(30);
        g.release(40);
        assert!(g.is_open_at(10) && g.is_open_at(39) && !g.is_open_at(40));
    }

    #[test]
    fn gate_mutes_outside_presses_with_fades() {
        let mut g = PttGate::new(0, 0, 4);
        g.press(8);
        g.release(16);
        let mut block = vec![1.0f32; 24];
        g.apply(0, &mut block);
        assert!(block[..8].iter().all(|&s| s == 0.0));
        assert_eq!(&block[8..12], &[0.25, 0.5, 0.75, 1.0]); // fade in
        assert_eq!(block[15], 1.0);
        assert_eq!(&block[16..20], &[0.75, 0.5, 0.25, 0.0]); // fade out
    }
}
