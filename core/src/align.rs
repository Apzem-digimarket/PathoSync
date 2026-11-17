//! Clap test: measures the fixed delay of each microphone.
//!
//! Different mics (USB, wireless, audio interface) deliver sound with different built-in delays.
//! During the test everyone stays quiet and one person claps once near all the mics. The clap
//! arrives in every track; comparing where it lands tells how much each track must be delayed
//! so that all of them line up.

const DECIMATE: usize = 8;

fn envelope(x: &[f32]) -> Vec<f32> {
    x.chunks(DECIMATE).map(|c| c.iter().fold(0.0f32, |m, s| m.max(s.abs()))).collect()
}

fn percentile(v: &[f32], p: f32) -> f32 {
    let mut s: Vec<f32> = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    s.get(((s.len() as f32 - 1.0) * p) as usize).copied().unwrap_or(0.0)
}

/// For each track, the delay (in frames) to apply so the clap lines up in all tracks.
/// The track that hears the clap last gets 0. Tracks must be on the same (master) clock.
pub fn clap_offsets(tracks: &[Vec<f32>], sample_rate: u32, max_lag_ms: f32) -> Result<Vec<u64>, String> {
    if tracks.len() < 2 {
        return Err("The clap test needs at least two microphones.".into());
    }
    let envs: Vec<Vec<f32>> = tracks.iter().map(|t| envelope(t)).collect();
    for (i, e) in envs.iter().enumerate() {
        let peak = e.iter().copied().fold(0.0f32, f32::max);
        let floor = percentile(e, 0.5).max(1e-5);
        if peak < 0.02 || peak < floor * 8.0 {
            return Err(format!("Mic {} did not hear a clear clap. Clap louder, closer to all the mics, and keep the room quiet.", i + 1));
        }
    }
    // Work around the loudest moment of the first track.
    let r = &envs[0];
    let peak_at = r.iter().enumerate().fold((0, 0.0f32), |b, (i, &v)| if v > b.1 { (i, v) } else { b }).0;
    let max_lag = ((max_lag_ms / 1000.0) * sample_rate as f32 / DECIMATE as f32) as isize;
    let half = ((0.4 * sample_rate as f32) / DECIMATE as f32) as isize;
    let from = (peak_at as isize - half).max(0) as usize;
    let to = (peak_at + half as usize).min(r.len());

    let mut arrival: Vec<isize> = vec![0];
    for e in envs.iter().skip(1) {
        let mut best = (0isize, f64::MIN);
        for lag in -max_lag..=max_lag {
            let mut acc = 0.0f64;
            for (k, &rv) in r.iter().enumerate().take(to).skip(from) {
                let j = k as isize + lag;
                if j >= 0 && (j as usize) < e.len() {
                    acc += rv as f64 * e[j as usize] as f64;
                }
            }
            if acc > best.1 {
                best = (lag, acc);
            }
        }
        if best.0.abs() == max_lag {
            return Err(format!("The mics are more than {max_lag_ms:.0} ms apart — check that every mic heard the same clap."));
        }
        arrival.push(best.0);
    }
    let latest = *arrival.iter().max().unwrap_or(&0);
    Ok(arrival.iter().map(|&a| ((latest - a) as usize * DECIMATE) as u64).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clap_at(len: usize, at: usize, noise: f32) -> Vec<f32> {
        let mut seed = 12345u32 + at as u32;
        (0..len)
            .map(|i| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let n = ((seed >> 16) as f32 / 32768.0 - 1.0) * noise;
                let d = i as isize - at as isize;
                if (0..400).contains(&d) {
                    n + 0.8 * (-(d as f32) / 80.0).exp() * if d % 2 == 0 { 1.0 } else { -1.0 }
                } else {
                    n
                }
            })
            .collect()
    }

    #[test]
    fn finds_the_delay_of_each_mic() {
        let sr = 48_000;
        let tracks = vec![clap_at(96_000, 40_000, 0.002), clap_at(96_000, 41_200, 0.002), clap_at(96_000, 39_520, 0.002)];
        let off = clap_offsets(&tracks, sr, 150.0).unwrap();
        // mic 2 hears it last (25 ms after mic 1); mic 3 first (10 ms before mic 1)
        let ms: Vec<f32> = off.iter().map(|&o| o as f32 * 1000.0 / sr as f32).collect();
        assert!((ms[0] - 25.0).abs() < 0.5, "{ms:?}");
        assert!(ms[1] < 0.5, "{ms:?}");
        assert!((ms[2] - 35.0).abs() < 0.5, "{ms:?}");
    }

    #[test]
    fn rejects_a_silent_mic() {
        let tracks = vec![clap_at(48_000, 20_000, 0.002), vec![0.0; 48_000]];
        assert!(clap_offsets(&tracks, 48_000, 150.0).is_err());
    }
}
