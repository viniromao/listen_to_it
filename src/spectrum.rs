//! A spectrum-analyser look for the now-playing bar.
//!
//! mpv plays the audio in its own process, so the samples never pass through
//! here and there is nothing to run a real FFT on. The shape of the bars is
//! synthesised instead: smooth noise across frequency and time, a bass-heavy
//! tilt like real music has, and a kick on the low end. Time is the track's
//! playback position, so the bars stop with the song and follow a seek.
//!
//! Their height is real, though: mpv measures how loud the track is (see
//! `crate::player`), so a quiet passage gives short bars and silence none,
//! whatever the volume is set to.

use std::time::Instant;

/// How fast the bars rise when playback starts and sink when it stops, in
/// full heights per second.
const RISE_PER_SEC: f32 = 4.0;
const FALL_PER_SEC: f32 = 2.5;

/// Tempo of the simulated kick drum.
const BPM: f64 = 118.0;

/// RMS levels mapped onto bar height: this loud or louder fills the bars,
/// this quiet or quieter leaves them empty. Mastered music mostly sits
/// between -18 and -8 dBFS.
const LOUD_DB: f64 = -8.0;
const QUIET_DB: f64 = -48.0;

/// How fast the bars follow the measured loudness, per second: quick to jump
/// on a hit, slower to let it ring out.
const LOUDNESS_ATTACK: f32 = 12.0;
const LOUDNESS_RELEASE: f32 = 4.0;

pub struct Spectrum {
    /// 0 while stopped to 1 while playing, eased so the bars grow in when a
    /// track starts and die away when it pauses instead of snapping.
    energy: f32,
    /// Latest loudness mpv reported, 0 to 1. `None` until the first reading,
    /// and for good if mpv can't measure, in which case bars run at full size.
    target_loudness: Option<f32>,
    /// `target_loudness` eased over time, which is what the bars use.
    loudness: f32,
    last: Instant,
}

impl Spectrum {
    pub fn new() -> Self {
        Self { energy: 0.0, target_loudness: None, loudness: 1.0, last: Instant::now() }
    }

    /// Record a loudness reading from mpv, RMS in dBFS.
    pub fn set_level_db(&mut self, db: f64) {
        let l = ((db - QUIET_DB) / (LOUD_DB - QUIET_DB)).clamp(0.0, 1.0);
        // -inf (digital silence) lands on 0; NaN would slip past the clamp.
        self.target_loudness = Some(if l.is_nan() { 0.0 } else { l as f32 });
    }

    /// Advance the rise/fall towards `playing`.
    pub fn step(&mut self, playing: bool) {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f32().min(0.5);
        self.last = now;
        self.energy = if playing {
            (self.energy + RISE_PER_SEC * dt).min(1.0)
        } else {
            (self.energy - FALL_PER_SEC * dt).max(0.0)
        };
        let target = self.target_loudness.unwrap_or(1.0);
        let rate = if target > self.loudness { LOUDNESS_ATTACK } else { LOUDNESS_RELEASE };
        self.loudness += (target - self.loudness) * (rate * dt).min(1.0);
    }

    /// Whether the bars change from one frame to the next.
    pub fn is_moving(&self, playing: bool) -> bool {
        playing || self.energy > 0.0
    }

    /// Height of the bar at `x` (0 = lowest frequency, 1 = highest) at
    /// playback position `t` seconds, from 0 to 1.
    pub fn level(&self, x: f64, t: f64) -> f32 {
        let tilt = 1.0 - 0.5 * x;
        let beat = (t * BPM / 60.0).fract();
        let kick = (-beat * 7.0).exp() * (1.0 - x).powi(2);
        let n = 0.65 * noise(x * 9.0, t * 3.1) + 0.35 * noise(x * 23.0 + 50.0, t * 7.3);
        let level = tilt * (0.15 + 0.75 * n) + 0.4 * kick;
        (level.clamp(0.0, 1.0) as f32) * self.energy * self.loudness
    }
}

impl Default for Spectrum {
    fn default() -> Self {
        Self::new()
    }
}

/// 2D value noise in 0..1, smooth in both directions.
fn noise(x: f64, y: f64) -> f64 {
    let (xi, yi) = (x.floor(), y.floor());
    let (xf, yf) = (smooth(x - xi), smooth(y - yi));
    let (xi, yi) = (xi as i64, yi as i64);
    let top = lerp(hash(xi, yi), hash(xi + 1, yi), xf);
    let bottom = lerp(hash(xi, yi + 1), hash(xi + 1, yi + 1), xf);
    lerp(top, bottom, yf)
}

fn smooth(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn hash(x: i64, y: i64) -> f64 {
    let mut h = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    h ^= h >> 31;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 29;
    (h >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing() -> Spectrum {
        Spectrum { energy: 1.0, ..Spectrum::new() }
    }

    /// Ease the loudness all the way to its target.
    fn settle(s: &mut Spectrum) {
        for _ in 0..50 {
            s.last -= std::time::Duration::from_millis(100);
            s.step(true);
        }
    }

    #[test]
    fn quiet_audio_gives_shorter_bars() {
        let mut loud = playing();
        loud.set_level_db(-10.0);
        settle(&mut loud);
        let mut quiet = playing();
        quiet.set_level_db(-38.0);
        settle(&mut quiet);
        for j in 0..20 {
            let x = j as f64 / 19.0;
            assert!(quiet.level(x, 5.0) < loud.level(x, 5.0) * 0.5);
        }
    }

    #[test]
    fn silence_from_mpv_flattens_the_bars() {
        let mut s = playing();
        s.set_level_db(f64::NEG_INFINITY);
        settle(&mut s);
        assert!(s.level(0.1, 3.0) < 0.001);
    }

    #[test]
    fn without_readings_the_bars_run_at_full_size() {
        let mut s = playing();
        settle(&mut s);
        assert!((0..20).any(|j| s.level(j as f64 / 19.0, 5.0) > 0.3));
    }

    #[test]
    fn levels_stay_in_range() {
        let s = playing();
        for i in 0..200 {
            for j in 0..50 {
                let l = s.level(j as f64 / 49.0, i as f64 * 0.37);
                assert!((0.0..=1.0).contains(&l), "{l}");
            }
        }
    }

    #[test]
    fn the_bars_move_over_time() {
        let s = playing();
        assert_ne!(s.level(0.3, 10.0), s.level(0.3, 10.2));
    }

    #[test]
    fn silence_is_flat() {
        let s = Spectrum::new();
        assert_eq!(s.level(0.2, 12.0), 0.0);
        assert!(!s.is_moving(false));
    }
}
