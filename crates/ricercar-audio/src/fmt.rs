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

/// Wire container used on a sink for a given source format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// S16_LE
    S16,
    /// S24_LE (24 significant bits in a 32-bit word)
    S24,
    /// S24_3LE (packed)
    S24_3,
    /// S32_LE
    S32,
}

impl Container {
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

    /// Pick the lossless container that carries exactly `bits` of content.
    pub fn for_bits(bits: u8) -> Container {
        match bits {
            0..=16 => Container::S16,
            17..=24 => Container::S24,
            _ => Container::S32,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Container::S16 => "S16_LE",
            Container::S24 => "S24_LE",
            Container::S24_3 => "S24_3LE",
            Container::S32 => "S32_LE",
        }
    }
}

/// Convert interleaved LSB-aligned i32 samples (with `content_bits` effective
/// depth) into the little-endian container bytes and append to `out`.
///
/// Lossless by construction: samples are only left-shifted to align with the
/// container's most significant bits, never truncated or rounded.
pub fn append_container(
    out: &mut Vec<u8>,
    samples: &[i32],
    container: Container,
    content_bits: u8,
) {
    let shift = container.memory_bits().saturating_sub(content_bits as u32);
    match container {
        Container::S16 => {
            out.reserve(samples.len() * 2);
            for &s in samples {
                out.extend_from_slice(&((s << shift) as i16).to_le_bytes());
            }
        }
        Container::S24 => {
            out.reserve(samples.len() * 4);
            for &s in samples {
                out.extend_from_slice(&(s << shift).to_le_bytes());
            }
        }
        Container::S24_3 => {
            out.reserve(samples.len() * 3);
            for &s in samples {
                let b = (s << shift).to_le_bytes();
                out.extend_from_slice(&b[..3]);
            }
        }
        Container::S32 => {
            out.reserve(samples.len() * 4);
            for &s in samples {
                out.extend_from_slice(&(s << shift).to_le_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s24_left_aligns_24bit_content_by_8() {
        let samples = [0x0080_0000i32, -1, 0x7F_FFFF];
        let mut out = Vec::new();
        append_container(&mut out, &samples, Container::S24, 24);
        let expected: Vec<u8> = [0x0080_0000i32 << 8, -1i32 << 8, 0x7F_FFFFi32 << 8]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn s16_exact_16bit() {
        let mut out = Vec::new();
        append_container(&mut out, &[32767, -32768], Container::S16, 16);
        assert_eq!(out, [0xFF, 0x7F, 0x00, 0x80]);
    }

    #[test]
    fn s24_3_packed() {
        let mut out = Vec::new();
        append_container(&mut out, &[0x0000_FFEE], Container::S24_3, 24);
        assert_eq!(out, [0xEE, 0xFF, 0x00]);
    }

    #[test]
    fn container_choice() {
        assert_eq!(Container::for_bits(16), Container::S16);
        assert_eq!(Container::for_bits(24), Container::S24);
        assert_eq!(Container::for_bits(32), Container::S32);
    }
}
