/// Format of the decoded PCM samples of a track, as they reach our pipeline.
///
/// `bits` is the effective depth of the source content (16, 20, 24, 32...).
/// Samples travel internally as interleaved `i32`, LSB-aligned and
/// sign-extended, i.e. a 24-bit sample keeps its exact original value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u8,
}

impl PcmFormat {
    pub fn describe(&self) -> String {
        format!(
            "{} bit / {} Hz / {} ch",
            self.bits, self.sample_rate, self.channels
        )
    }
}

/// Wire container used on a sink for a given source format (ALSA semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Container {
    /// S16_LE
    S16,
    /// S24_LE: 24 significant bits in the *low* three bytes of a 32-bit LE
    /// word (LSB-aligned, sign-extended into the top byte).
    S24,
    /// S24_3LE: 24 bits packed in 3 bytes.
    S24_3,
    /// S32_LE: 32-bit word, content MSB-aligned.
    S32,
}

impl Container {
    pub const ALL: [Container; 4] = [
        Container::S16,
        Container::S24,
        Container::S24_3,
        Container::S32,
    ];

    pub fn bytes_per_sample(self) -> usize {
        match self {
            Container::S16 => 2,
            Container::S24_3 => 3,
            Container::S24 | Container::S32 => 4,
        }
    }

    /// Width in bits of one sample in the container's memory layout.
    pub fn memory_bits(self) -> u32 {
        match self {
            Container::S16 => 16,
            Container::S24_3 => 24,
            Container::S24 | Container::S32 => 32,
        }
    }

    /// Number of significant bits the container can carry.
    pub fn significant_bits(self) -> u32 {
        match self {
            Container::S16 => 16,
            Container::S24 | Container::S24_3 => 24,
            Container::S32 => 32,
        }
    }

    /// Preferred lossless container for `bits` of content when the sink
    /// accepts everything.
    pub fn for_bits(bits: u8) -> Container {
        negotiate(bits, &Container::ALL).unwrap_or(Container::S32)
    }

    pub fn label(self) -> &'static str {
        match self {
            Container::S16 => "S16_LE",
            Container::S24 => "S24_LE",
            Container::S24_3 => "S24_3LE",
            Container::S32 => "S32_LE",
        }
    }

    pub fn from_label(label: &str) -> Option<Container> {
        Container::ALL.into_iter().find(|c| c.label() == label)
    }
}

/// Pick the first container, in order of preference, that carries
/// `content_bits` losslessly and that the sink supports. Zero-padding into a
/// wider container is still bit-perfect; truncation never happens.
pub fn negotiate(content_bits: u8, supported: &[Container]) -> Option<Container> {
    use Container::*;
    let prefs: &[Container] = match content_bits {
        0..=16 => &[S16, S32, S24_3, S24],
        17..=24 => &[S24, S24_3, S32],
        25..=32 => &[S32],
        _ => &[],
    };
    prefs.iter().copied().find(|c| supported.contains(c))
}

/// Convert interleaved LSB-aligned i32 samples (with `content_bits` effective
/// depth) into the little-endian container bytes and append to `out`.
///
/// Lossless by construction: content is shifted up to the container's
/// significant width (zero-filled LSBs), never truncated or rounded. Callers
/// must have negotiated a container with `significant_bits() >= content_bits`.
pub fn append_container(
    out: &mut Vec<u8>,
    samples: &[i32],
    container: Container,
    content_bits: u8,
) {
    let shift = container
        .significant_bits()
        .saturating_sub(content_bits as u32);
    out.reserve(samples.len() * container.bytes_per_sample());
    match container {
        Container::S16 => {
            for &s in samples {
                out.extend_from_slice(&((s << shift) as i16).to_le_bytes());
            }
        }
        // S24 is LSB-aligned: the 24-bit value is written as a sign-extended
        // 32-bit word, which is exactly what ALSA's S24_LE expects.
        Container::S24 | Container::S32 => {
            for &s in samples {
                out.extend_from_slice(&(s << shift).to_le_bytes());
            }
        }
        Container::S24_3 => {
            for &s in samples {
                out.extend_from_slice(&(s << shift).to_le_bytes()[..3]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Container::*;

    fn enc(samples: &[i32], c: Container, bits: u8) -> Vec<u8> {
        let mut out = Vec::new();
        append_container(&mut out, samples, c, bits);
        out
    }

    #[test]
    fn s24_is_lsb_aligned_and_sign_extended() {
        let out = enc(&[0x7F_FFFF, -0x80_0000, -1, 0x12_3456], S24, 24);
        assert_eq!(
            out,
            [
                0xFF, 0xFF, 0x7F, 0x00, // max
                0x00, 0x00, 0x80, 0xFF, // min
                0xFF, 0xFF, 0xFF, 0xFF, // -1
                0x56, 0x34, 0x12, 0x00,
            ]
        );
    }

    #[test]
    fn s24_pads_16bit_content_into_24_significant_bits() {
        assert_eq!(enc(&[-2], S24, 16), (-2i32 << 8).to_le_bytes());
        assert_eq!(enc(&[0x7FFF], S24, 16), [0x00, 0xFF, 0x7F, 0x00]);
    }

    #[test]
    fn s32_is_msb_aligned() {
        assert_eq!(enc(&[0x12_3456], S32, 24), [0x00, 0x56, 0x34, 0x12]);
        assert_eq!(enc(&[-1], S32, 16), (-1i32 << 16).to_le_bytes());
        assert_eq!(enc(&[i32::MIN], S32, 32), i32::MIN.to_le_bytes());
    }

    #[test]
    fn s16_exact_16bit() {
        assert_eq!(enc(&[32767, -32768], S16, 16), [0xFF, 0x7F, 0x00, 0x80]);
    }

    #[test]
    fn s24_3_packed() {
        assert_eq!(enc(&[0x0000_FFEE], S24_3, 24), [0xEE, 0xFF, 0x00]);
        assert_eq!(enc(&[-0x80_0000], S24_3, 24), [0x00, 0x00, 0x80]);
        assert_eq!(enc(&[0x1234], S24_3, 16), [0x00, 0x34, 0x12]);
    }

    /// Every container must round-trip every content value exactly.
    #[test]
    fn containers_preserve_content_bits() {
        for bits in [16u8, 24] {
            let max = (1i32 << (bits - 1)) - 1;
            let min = -(1i32 << (bits - 1));
            let samples = [min, min + 1, -1, 0, 1, max - 1, max];
            for c in Container::ALL {
                if c.significant_bits() < bits as u32 {
                    continue;
                }
                let out = enc(&samples, c, bits);
                let pad = c.significant_bits() - bits as u32;
                let back: Vec<i32> = out
                    .chunks(c.bytes_per_sample())
                    .map(|b| {
                        let word = match c {
                            S16 => i16::from_le_bytes([b[0], b[1]]) as i32,
                            S24_3 => i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8,
                            S24 | S32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                        };
                        if c == S24 {
                            assert_eq!((word << 8) >> 8, word, "S24 top byte must sign-extend");
                        }
                        assert_eq!(word & ((1i64 << pad) - 1) as i32, 0, "padding must be zero");
                        word >> pad
                    })
                    .collect();
                assert_eq!(back, samples, "{c:?} {bits}");
            }
        }
    }

    #[test]
    fn negotiation_prefers_native_width() {
        assert_eq!(negotiate(16, &Container::ALL), Some(S16));
        assert_eq!(negotiate(24, &Container::ALL), Some(S24));
        assert_eq!(negotiate(20, &Container::ALL), Some(S24));
        assert_eq!(negotiate(32, &Container::ALL), Some(S32));
    }

    #[test]
    fn negotiation_falls_back_losslessly() {
        assert_eq!(negotiate(16, &[S24, S32]), Some(S32));
        assert_eq!(negotiate(16, &[S24_3, S24]), Some(S24_3));
        assert_eq!(negotiate(16, &[S24]), Some(S24));
        assert_eq!(negotiate(24, &[S16, S24_3, S32]), Some(S24_3));
        assert_eq!(negotiate(24, &[S16, S32]), Some(S32));
        assert_eq!(negotiate(32, &[S16, S24, S24_3, S32]), Some(S32));
    }

    #[test]
    fn negotiation_refuses_truncation() {
        assert_eq!(negotiate(24, &[S16]), None);
        assert_eq!(negotiate(32, &[S16, S24, S24_3]), None);
        assert_eq!(negotiate(16, &[]), None);
    }

    #[test]
    fn labels_round_trip() {
        for c in Container::ALL {
            assert_eq!(Container::from_label(c.label()), Some(c));
        }
        assert_eq!(Container::for_bits(24), S24);
    }
}
