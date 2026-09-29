//! DAC capabilities: what each `hw:` device accepts, cached between runs,
//! and what that means for the library. Pure helpers here; the settings
//! panel lives in `extras.rs`.

use std::collections::BTreeMap;

use ricercar_audio::{Container, DeviceCaps};
use ricercar_core::RateCount;
use serde::{Deserialize, Serialize};

use crate::text::khz;

/// Rates offered as chips (the ones music comes in).
pub const MUSIC_RATES: [u32; 8] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
];

/// Containers in the order the panel lists them.
pub const CONTAINERS: [(Container, &str); 4] = [
    (Container::S16, "S16"),
    (Container::S24_3, "S24_3LE"),
    (Container::S24, "S24"),
    (Container::S32, "S32"),
];

/// Capabilities as stored in ui-state.json, by device name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedCaps {
    pub rates: Vec<u32>,
    /// Container labels (`S16_LE`, `S24_3LE`…).
    pub formats: Vec<String>,
    pub channels_min: u32,
    pub channels_max: u32,
    /// Unix time of the probe.
    pub probed_at: i64,
}

pub type CapsCache = BTreeMap<String, CachedCaps>;

impl CachedCaps {
    pub fn from_caps(c: &DeviceCaps, now: i64) -> CachedCaps {
        CachedCaps {
            rates: c.rates.clone(),
            formats: c.formats.iter().map(|f| f.label().to_string()).collect(),
            channels_min: *c.channels.start(),
            channels_max: *c.channels.end(),
            probed_at: now,
        }
    }

    pub fn supports_rate(&self, rate: u32) -> bool {
        self.rates.contains(&rate)
    }

    pub fn supports(&self, c: Container) -> bool {
        self.formats.iter().any(|f| f == c.label())
    }
}

/// Rate chips: label and whether the device takes it.
pub fn rate_chips(c: &CachedCaps) -> Vec<(String, bool)> {
    MUSIC_RATES
        .iter()
        .map(|&r| (khz(r), c.supports_rate(r)))
        .collect()
}

pub fn container_chips(c: &CachedCaps) -> Vec<(String, bool)> {
    CONTAINERS
        .iter()
        .map(|&(k, label)| (label.to_string(), c.supports(k)))
        .collect()
}

/// "2" or "1–8".
pub fn channels_label(c: &CachedCaps) -> String {
    if c.channels_min == c.channels_max {
        c.channels_min.to_string()
    } else {
        format!("{}–{}", c.channels_min, c.channels_max)
    }
}

/// Accepted music rates in short form: "44.1–192 kHz" when they follow each
/// other in the usual series, else "44.1, 48, 96 kHz".
pub fn rates_summary(rates: &[u32]) -> String {
    let idx: Vec<usize> = MUSIC_RATES
        .iter()
        .enumerate()
        .filter(|(_, r)| rates.contains(r))
        .map(|(i, _)| i)
        .collect();
    match (idx.first(), idx.last()) {
        (None, _) | (_, None) => String::new(),
        (Some(&a), Some(&b)) if a == b => format!("{} kHz", khz(MUSIC_RATES[a])),
        (Some(&a), Some(&b)) if b - a + 1 == idx.len() => {
            format!("{}–{} kHz", khz(MUSIC_RATES[a]), khz(MUSIC_RATES[b]))
        }
        _ => {
            let list: Vec<String> = idx.iter().map(|&i| khz(MUSIC_RATES[i])).collect();
            format!("{} kHz", list.join(", "))
        }
    }
}

/// Library rates this device cannot play natively: (rate, albums).
pub fn unsupported(hist: &[RateCount], c: &CachedCaps) -> Vec<(u32, u32)> {
    hist.iter()
        .filter(|h| !c.supports_rate(h.rate))
        .map(|h| (h.rate, h.albums))
        .collect()
}

/// The rate in an engine refusal: "device 'hw:1,0' cannot play 352800 Hz
/// bit-perfectly" → 352800.
pub fn refused_rate(msg: &str) -> Option<u32> {
    let head = msg.split(" Hz bit-perfectly").next()?;
    if head.len() == msg.len() {
        return None;
    }
    head.rsplit(' ').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(rates: &[u32]) -> CachedCaps {
        CachedCaps {
            rates: rates.to_vec(),
            formats: vec!["S16_LE".into(), "S32_LE".into()],
            channels_min: 2,
            channels_max: 2,
            probed_at: 0,
        }
    }

    #[test]
    fn labels_and_ranges() {
        let c = caps(&[44_100, 48_000, 88_200, 96_000, 176_400, 192_000]);
        assert_eq!(rates_summary(&c.rates), "44.1–192 kHz");
        assert_eq!(rates_summary(&[44_100, 96_000]), "44.1, 96 kHz");
        assert_eq!(rates_summary(&[48_000]), "48 kHz");
        assert_eq!(rates_summary(&[8_000]), "");
        let chips = rate_chips(&c);
        assert_eq!(chips[0], ("44.1".to_string(), true));
        assert_eq!(chips[6], ("352.8".to_string(), false));
        let names: Vec<(String, bool)> = container_chips(&c);
        assert_eq!(
            names,
            [
                ("S16".to_string(), true),
                ("S24_3LE".to_string(), false),
                ("S24".to_string(), false),
                ("S32".to_string(), true),
            ]
        );
        assert_eq!(channels_label(&c), "2");
        let mut wide = c.clone();
        wide.channels_min = 1;
        wide.channels_max = 8;
        assert_eq!(channels_label(&wide), "1–8");
    }

    #[test]
    fn library_rates_against_the_dac() {
        let hist = [
            RateCount {
                rate: 44_100,
                albums: 300,
                tracks: 3600,
            },
            RateCount {
                rate: 352_800,
                albums: 12,
                tracks: 90,
            },
        ];
        assert_eq!(
            unsupported(&hist, &caps(&[44_100, 96_000])),
            [(352_800, 12)]
        );
        assert!(unsupported(&hist, &caps(&MUSIC_RATES)).is_empty());
    }

    #[test]
    fn refusals_are_recognised() {
        let e = "device 'hw:1,0' cannot play 352800 Hz bit-perfectly";
        assert_eq!(refused_rate(e), Some(352_800));
        assert_eq!(
            refused_rate("device 'hw:1,0' cannot play 6 channels bit-perfectly"),
            None
        );
        assert_eq!(refused_rate("decode error: eof"), None);
    }

    #[test]
    fn null_and_file_sinks_probe_unrestricted() {
        for name in ["null", "file:/nonexistent/out.raw"] {
            let c = CachedCaps::from_caps(&ricercar_audio::probe_device(name).unwrap(), 1);
            assert!(MUSIC_RATES.iter().all(|&r| c.supports_rate(r)));
            assert!(CONTAINERS.iter().all(|&(k, _)| c.supports(k)));
        }
    }
}
