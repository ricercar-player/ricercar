use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use crate::error::{AudioError, Result};
use crate::fmt::Container;

/// Playback devices advertised to the user.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// ALSA device name, or `null` / `file:<path>`
    pub name: String,
    pub description: String,
    pub kind: DeviceKind,
    /// Long name of the sound card (from `/proc/asound/cards`), for `hw:`.
    pub card_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// `hw:` card — direct, exclusive, no mixer in the path.
    Hardware,
    /// Any other ALSA name (plughw, default, pipewire's alsa bridge...).
    Virtual,
    /// Sink discarding data (testing / --device null).
    Null,
    /// Sink writing raw container bytes to a file (testing / bit-exact checks).
    File,
}

impl DeviceKind {
    /// True when we can guarantee no resampling or mixing happens on our side
    /// and the kernel hands samples to the codec untouched.
    pub fn is_bit_perfect(self) -> bool {
        matches!(
            self,
            DeviceKind::Hardware | DeviceKind::Null | DeviceKind::File
        )
    }
}

pub fn classify(name: &str) -> DeviceKind {
    if name == "null" {
        DeviceKind::Null
    } else if name.starts_with("file:") {
        DeviceKind::File
    } else if name.starts_with("hw:") {
        DeviceKind::Hardware
    } else {
        DeviceKind::Virtual
    }
}

/// Card index -> (short name, long name), from `/proc/asound/cards`:
///
/// ```text
///  1 [K7             ]: USB-Audio - FiiO K7
///                       FiiO FiiO K7 at usb-0000:00:14.0-2, high speed
/// ```
pub fn parse_cards(text: &str) -> BTreeMap<u32, (String, String)> {
    let mut out = BTreeMap::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let t = line.trim_start();
        let Some((idx, rest)) = t.split_once(' ') else {
            continue;
        };
        let Ok(idx) = idx.parse::<u32>() else {
            continue;
        };
        let Some((_, head)) = rest.split_once("]: ") else {
            continue;
        };
        let short = head
            .split_once(" - ")
            .map_or(head, |(_, s)| s)
            .trim()
            .to_string();
        let long = match lines.peek() {
            Some(next) if next.starts_with("  ") => {
                let l = next.trim().to_string();
                lines.next();
                l
            }
            _ => short.clone(),
        };
        out.insert(idx, (short, long));
    }
    out
}

/// Playback PCMs from `/proc/asound/pcm` as (card, device, name):
/// `00-03: HDMI 0 : HDMI 0 : playback 1`.
pub fn parse_pcms(text: &str) -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((ids, rest)) = line.split_once(": ") else {
            continue;
        };
        if !rest.contains("playback") {
            continue;
        }
        let Some((card, dev)) = ids.split_once('-') else {
            continue;
        };
        let (Ok(card), Ok(dev)) = (card.trim().parse::<u32>(), dev.trim().parse::<u32>()) else {
            continue;
        };
        let desc = rest.split(" : ").next().unwrap_or("").trim().to_string();
        out.push((card, dev, desc));
    }
    out.sort_by_key(|&(c, d, _)| (c, d));
    out
}

/// Names of ALSA PCMs declared in the configuration (no device is opened).
fn alsa_pcm_hints() -> Option<Vec<(String, Option<String>)>> {
    let hints = alsa::device_name::HintIter::new_str(None, "pcm").ok()?;
    Some(
        hints
            .filter(|h| h.direction != Some(alsa::Direction::Capture))
            .filter_map(|h| {
                let desc = h.desc.map(|d| d.lines().next().unwrap_or("").to_string());
                h.name.map(|n| (n, desc))
            })
            .collect(),
    )
}

/// Build the device list from the raw sources; pure, for testability.
pub fn build_device_list(
    cards: &str,
    pcms: &str,
    hints: Option<&[(String, Option<String>)]>,
) -> Vec<DeviceInfo> {
    let cards = parse_cards(cards);
    let mut out = vec![DeviceInfo {
        name: "null".into(),
        description: "Null sink (discards audio, for tests)".into(),
        kind: DeviceKind::Null,
        card_name: None,
    }];
    for (card, dev, desc) in parse_pcms(pcms) {
        let names = cards.get(&card);
        let description = match names {
            Some((short, _)) if !desc.is_empty() => format!("{short}: {desc}"),
            Some((short, _)) => short.clone(),
            None => desc,
        };
        out.push(DeviceInfo {
            name: format!("hw:{card},{dev}"),
            description,
            kind: DeviceKind::Hardware,
            card_name: names.map(|(_, long)| long.clone()),
        });
    }
    let virtuals = [
        (
            "default",
            "ALSA default (may be resampled by PipeWire/Pulse)",
        ),
        ("pipewire", "PipeWire (mixed/resampled by the sound server)"),
        ("pulse", "PulseAudio (mixed/resampled by the sound server)"),
    ];
    for (name, fallback) in virtuals {
        // Without hints, still offer `default`: it exists on any sane setup.
        let hint = match hints {
            Some(h) => match h.iter().find(|(n, _)| n == name) {
                Some(found) => Some(found.1.clone()),
                None => continue,
            },
            None if name == "default" => None,
            None => continue,
        };
        let description = match hint.flatten() {
            Some(d) if !d.is_empty() => format!("{d} (not bit-perfect)"),
            _ => fallback.to_string(),
        };
        out.push(DeviceInfo {
            name: name.into(),
            description,
            kind: DeviceKind::Virtual,
            card_name: None,
        });
    }
    out
}

/// Enumerate playback devices: `null`, every `hw:` PCM (bit-perfect), then
/// `default`/`pipewire`/`pulse` when ALSA declares them. Never opens a device.
pub fn list_devices() -> Vec<DeviceInfo> {
    let cards = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    let pcms = std::fs::read_to_string("/proc/asound/pcm").unwrap_or_default();
    let hints = alsa_pcm_hints();
    build_device_list(&cards, &pcms, hints.as_deref())
}

/// What a device accepts, for a "device capabilities" panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCaps {
    pub formats: Vec<Container>,
    pub rates: Vec<u32>,
    pub channels: RangeInclusive<u32>,
}

/// Rates worth reporting (the ones music actually comes in).
pub const PROBE_RATES: [u32; 14] = [
    8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000,
    768000,
];

impl DeviceCaps {
    /// Assemble caps from per-format / per-rate acceptance tests.
    pub fn from_tests(
        format_ok: impl Fn(Container) -> bool,
        rate_ok: impl Fn(u32) -> bool,
        channels: RangeInclusive<u32>,
    ) -> DeviceCaps {
        DeviceCaps {
            formats: Container::ALL
                .into_iter()
                .filter(|&c| format_ok(c))
                .collect(),
            rates: PROBE_RATES.into_iter().filter(|&r| rate_ok(r)).collect(),
            channels,
        }
    }

    /// Everything goes (null / file sinks).
    pub fn unrestricted() -> DeviceCaps {
        DeviceCaps::from_tests(|_| true, |_| true, 1..=32)
    }

    pub fn supports_rate(&self, rate: u32) -> bool {
        self.rates.contains(&rate)
    }
}

pub(crate) fn alsa_format(c: Container) -> alsa::pcm::Format {
    use alsa::pcm::Format;
    match c {
        Container::S16 => Format::S16LE,
        Container::S24 => Format::S24LE,
        Container::S24_3 => Format::S243LE,
        Container::S32 => Format::S32LE,
    }
}

/// Containers a hw configuration space accepts.
pub(crate) fn supported_containers(hw: &alsa::pcm::HwParams) -> Vec<Container> {
    Container::ALL
        .into_iter()
        .filter(|&c| hw.test_format(alsa_format(c)).is_ok())
        .collect()
}

/// Query what an ALSA device accepts. Opens the PCM (non-blocking, so a busy
/// device errors out instead of hanging) but never starts it: nothing is
/// played. `null` and `file:` sinks report unrestricted caps.
pub fn probe_device(name: &str) -> Result<DeviceCaps> {
    if matches!(classify(name), DeviceKind::Null | DeviceKind::File) {
        return Ok(DeviceCaps::unrestricted());
    }
    let err = |e: alsa::Error| AudioError::Alsa {
        device: name.to_string(),
        source: e,
    };
    let pcm = alsa::PCM::new(name, alsa::Direction::Playback, true).map_err(err)?;
    let hw = alsa::pcm::HwParams::any(&pcm).map_err(err)?;
    hw.set_access(alsa::pcm::Access::RWInterleaved)
        .map_err(err)?;
    let channels = hw.get_channels_min().map_err(err)?..=hw.get_channels_max().map_err(err)?;
    let formats = supported_containers(&hw);
    Ok(DeviceCaps::from_tests(
        |c| formats.contains(&c),
        |r| hw.test_rate(r).is_ok(),
        channels,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARDS: &str = " 0 [Generic        ]: HDA-Intel - HD-Audio Generic
                      HD-Audio Generic at 0xfcd80000 irq 110
 1 [K7             ]: USB-Audio - FiiO K7
                      FiiO FiiO K7 at usb-0000:00:14.0-2, high speed
";
    const PCMS: &str = "01-00: USB Audio : USB Audio : playback 1 : capture 1
00-03: HDMI 0 : HDMI 0 : playback 1
00-00: Generic Analog : Generic Analog : playback 1 : capture 1
00-02: Generic Alt : Generic Alt : capture 1
";

    #[test]
    fn cards_long_and_short_names() {
        let c = parse_cards(CARDS);
        assert_eq!(c[&0].0, "HD-Audio Generic");
        assert_eq!(c[&1].0, "FiiO K7");
        assert_eq!(c[&1].1, "FiiO FiiO K7 at usb-0000:00:14.0-2, high speed");
    }

    #[test]
    fn device_list_is_ordered_and_classified() {
        let hints: Vec<(String, Option<String>)> = vec![
            ("pulse".into(), Some("PulseAudio Sound Server".into())),
            ("default".into(), Some("Default ALSA Output".into())),
            ("sysdefault:CARD=K7".into(), None),
        ];
        let l = build_device_list(CARDS, PCMS, Some(&hints));
        let names: Vec<&str> = l.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            ["null", "hw:0,0", "hw:0,3", "hw:1,0", "default", "pulse"]
        );
        assert_eq!(l[1].description, "HD-Audio Generic: Generic Analog");
        assert_eq!(
            l[3].card_name.as_deref(),
            Some("FiiO FiiO K7 at usb-0000:00:14.0-2, high speed")
        );
        for d in &l {
            assert_eq!(d.kind, classify(&d.name), "{}", d.name);
        }
        assert!(!l[4].kind.is_bit_perfect());
    }

    #[test]
    fn default_offered_without_hints() {
        let l = build_device_list("", "", None);
        let names: Vec<&str> = l.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["null", "default"]);
    }

    #[test]
    fn live_listing_never_fails() {
        let l = list_devices();
        assert_eq!(l[0].name, "null");
    }

    #[test]
    fn caps_from_tests() {
        let caps = DeviceCaps::from_tests(
            |c| matches!(c, Container::S16 | Container::S32),
            |r| r % 44100 == 0,
            2..=2,
        );
        assert_eq!(caps.formats, [Container::S16, Container::S32]);
        assert_eq!(caps.rates, [44100, 88200, 176400, 352800]);
        assert!(caps.supports_rate(88200) && !caps.supports_rate(96000));
        assert_eq!(probe_device("null").unwrap(), DeviceCaps::unrestricted());
        assert_eq!(
            probe_device("file:/nonexistent").unwrap().formats,
            Container::ALL
        );
    }
}
