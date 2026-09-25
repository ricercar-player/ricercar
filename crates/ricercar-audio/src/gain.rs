//! Gain stage: software volume, preamp and per-track ReplayGain, folded into
//! a single multiply. Unity (the default) leaves samples untouched, which is
//! the only bit-perfect setting.

/// Per-track options given with `load_with` / `enqueue_next_with`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TrackOpts {
    /// ReplayGain (or any per-track) adjustment in dB.
    pub gain_db: Option<f32>,
    /// Linear sample peak of the track (1.0 = full scale), used to prevent
    /// the gain from pushing it into clipping.
    pub peak: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GainStage {
    /// Software volume, 0..=100.
    pub volume: u32,
    /// Global preamp in dB (`SetGain`), added to the track gain.
    pub preamp_db: f32,
    pub track: TrackOpts,
    pub muted: bool,
}

impl Default for GainStage {
    fn default() -> Self {
        GainStage {
            volume: 100,
            preamp_db: 0.0,
            track: TrackOpts::default(),
            muted: false,
        }
    }
}

impl GainStage {
    /// dB gain requested (preamp + track), before clipping prevention.
    pub fn requested_db(&self) -> f32 {
        let db = self.preamp_db + self.track.gain_db.unwrap_or(0.0);
        if db.is_finite() { db } else { 0.0 }
    }

    /// Linear gain from the dB stage, capped so that `peak * gain <= 1.0`.
    fn db_factor(&self) -> f64 {
        let db = self.requested_db();
        if db == 0.0 {
            // Exactly unity: an untouched signal cannot clip any more than
            // it already does, so the peak cap does not apply.
            return 1.0;
        }
        let g = 10f64.powf(db as f64 / 20.0);
        match self.track.peak {
            Some(p) if p.is_finite() && p > 0.0 && g * p as f64 > 1.0 => 1.0 / p as f64,
            _ => g,
        }
    }

    /// Linear factor applied to every sample.
    pub fn factor(&self) -> f64 {
        if self.muted {
            return 0.0;
        }
        self.db_factor() * self.volume.min(100) as f64 / 100.0
    }

    /// Effective dB actually applied by the ReplayGain/preamp part.
    pub fn effective_db(&self) -> f32 {
        (20.0 * self.db_factor().log10()) as f32
    }

    pub fn is_unity(&self) -> bool {
        self.factor() == 1.0
    }

    /// Scale LSB-aligned samples of `bits` depth in place, saturating to the
    /// content range (the container cannot carry more).
    pub fn apply(&self, samples: &mut [i32], bits: u8) {
        let f = self.factor();
        if f == 1.0 {
            return;
        }
        if f == 0.0 {
            samples.fill(0);
            return;
        }
        let bits = bits.clamp(2, 32) as u32;
        let max = ((1i64 << (bits - 1)) - 1) as f64;
        let min = -(1i64 << (bits - 1)) as f64;
        for s in samples.iter_mut() {
            // f64 holds any i32 exactly; `as` saturates, clamp enforces depth.
            *s = (*s as f64 * f).round().clamp(min, max) as i32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(volume: u32, db: Option<f32>, peak: Option<f32>) -> GainStage {
        GainStage {
            volume,
            track: TrackOpts { gain_db: db, peak },
            ..GainStage::default()
        }
    }

    #[test]
    fn default_is_unity_and_untouched() {
        let g = GainStage::default();
        assert!(g.is_unity());
        let mut s = [i32::MIN, -1, 0, 1, i32::MAX];
        g.apply(&mut s, 32);
        assert_eq!(s, [i32::MIN, -1, 0, 1, i32::MAX]);
        assert_eq!(g.effective_db(), 0.0);
    }

    #[test]
    fn volume_and_gain_combine_in_one_factor() {
        let g = stage(50, Some(-6.0), None);
        let expect = 0.5 * 10f64.powf(-6.0 / 20.0);
        assert!((g.factor() - expect).abs() < 1e-12);
        let mut s = [10000, -10000];
        g.apply(&mut s, 16);
        let v = (10000.0 * expect).round() as i32;
        assert_eq!(s, [v, -v]);
        assert!(!g.is_unity());
    }

    #[test]
    fn half_gain_is_exact_halving() {
        let g = stage(50, None, None);
        let mut s = [32766, -32766, 2, -2];
        g.apply(&mut s, 16);
        assert_eq!(s, [16383, -16383, 1, -1]);
    }

    #[test]
    fn positive_gain_saturates_to_content_depth() {
        let g = stage(100, Some(12.0), None);
        let mut s = [30000, -30000, 100];
        g.apply(&mut s, 16);
        assert_eq!(s[0], 32767);
        assert_eq!(s[1], -32768);
        assert_eq!(s[2], 398);
        let mut s24 = [0x7F_0000];
        g.apply(&mut s24, 24);
        assert_eq!(s24, [0x7F_FFFF]);
    }

    #[test]
    fn peak_caps_gain() {
        // +6 dB on a track peaking at 0.8 would clip: capped to 1/0.8.
        let g = stage(100, Some(6.0), Some(0.8));
        assert!((g.factor() - 1.25).abs() < 1e-6);
        assert!((g.effective_db() - 1.9382).abs() < 1e-3);
        // -3 dB with a hot peak is left alone.
        let g = stage(100, Some(-3.0), Some(1.2));
        assert!((g.effective_db() + 3.0).abs() < 1e-4);
        // Unity gain is never reduced by the cap.
        assert!(stage(100, Some(0.0), Some(1.3)).is_unity());
    }

    #[test]
    fn preamp_adds_to_track_gain() {
        let g = GainStage {
            preamp_db: -2.0,
            track: TrackOpts {
                gain_db: Some(2.0),
                peak: None,
            },
            ..GainStage::default()
        };
        assert!(g.is_unity());
    }

    #[test]
    fn mute_zeroes() {
        let g = GainStage {
            muted: true,
            ..GainStage::default()
        };
        assert!(!g.is_unity());
        let mut s = [5, -5, i32::MAX];
        g.apply(&mut s, 32);
        assert_eq!(s, [0, 0, 0]);
    }

    #[test]
    fn nonsense_gain_is_ignored() {
        assert!(stage(100, Some(f32::NAN), None).is_unity());
    }
}
