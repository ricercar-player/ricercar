/// Playback devices advertised to the user.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// ALSA device name, or `null` / `file:<path>`
    pub name: String,
    pub description: String,
    pub kind: DeviceKind,
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

/// Enumerate ALSA playback devices by parsing `/proc/asound/cards` and
/// `/proc/asound/pcm` (no libasound needed, deterministic output).
pub fn list_devices() -> Vec<DeviceInfo> {
    let mut out = vec![DeviceInfo {
        name: "null".into(),
        description: "Null sink (discards audio, for tests)".into(),
        kind: DeviceKind::Null,
    }];
    if let Ok(pcm) = std::fs::read_to_string("/proc/asound/pcm") {
        for line in pcm.lines() {
            // "00-00: Generic Analog : HD-Audio Generic : playback 1 : capture 1"
            let Some((ids, rest)) = line.split_once(": ") else {
                continue;
            };
            if !rest.contains("playback") {
                continue;
            }
            let desc = rest.split(" : ").next().unwrap_or("").trim().to_string();
            let bytes = ids.as_bytes();
            if bytes.len() < 5 || bytes[2] != b'-' {
                continue;
            }
            let (Ok(card), Ok(dev)) = (ids[0..2].parse::<u32>(), ids[3..5].parse::<u32>()) else {
                continue;
            };
            out.push(DeviceInfo {
                name: format!("hw:{card},{dev}"),
                description: desc,
                kind: DeviceKind::Hardware,
            });
        }
    }
    out.push(DeviceInfo {
        name: "default".into(),
        description: "ALSA default (may be resampled by PipeWire/Pulse)".into(),
        kind: DeviceKind::Virtual,
    });
    out
}
