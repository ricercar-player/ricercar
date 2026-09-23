use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::TimeBase;

use crate::error::{AudioError, Result};
use crate::fmt::PcmFormat;

/// A decoded track ready (or nearly ready) to be pumped into a sink.
pub struct TrackSource {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    pub uri: String,
    pub duration_ms: Option<u64>,
    /// Known once the first packet has been decoded.
    pub format: Option<PcmFormat>,
    /// Decoded, not-yet-consumed interleaved i32 samples.
    pub pending: VecDeque<i32>,
    pub end_of_stream: bool,
    scratch: Vec<i32>,
    pub seekable: bool,
}

/// Intrinsic sample width (in bits) of the decoded buffer. For lossy codecs
/// that decode to float, the content is scaled into the full i32 range; we
/// keep 24 bits of it, like every other lossy player.
fn intrinsic_bits(b: &GenericAudioBufferRef) -> u8 {
    use GenericAudioBufferRef as G;
    match b {
        G::U8(_) | G::S8(_) => 8,
        G::U16(_) | G::S16(_) => 16,
        G::U24(_) | G::S24(_) => 24,
        G::F32(_) | G::F64(_) => 32,
        G::U32(_) | G::S32(_) => 32,
    }
}

/// Non-seekable read-ahead ring: a reader thread prefetches from a slow source
/// (HTTP) so the audio thread is not blocked by network hiccups.
pub struct Prefetch {
    rx: crossbeam_channel::Receiver<io::Result<Vec<u8>>>,
    buf: VecDeque<u8>,
}

impl Prefetch {
    fn start(reader: Box<dyn Read + Send + 'static>) -> Prefetch {
        let (tx, rx) = crossbeam_channel::unbounded::<io::Result<Vec<u8>>>();
        std::thread::Builder::new()
            .name("ricercar-prefetch".into())
            .spawn(move || {
                let mut reader = reader;
                let mut chunk = vec![0u8; 256 * 1024];
                loop {
                    match reader.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(Ok(chunk[..n].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(io::Error::new(e.kind(), e.to_string())));
                            break;
                        }
                    }
                }
            })
            .ok();
        Prefetch { rx, buf: VecDeque::new() }
    }
}

impl Read for Prefetch {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.buf.is_empty() {
            match self.rx.recv() {
                Ok(Ok(chunk)) => self.buf.extend(chunk),
                Ok(Err(e)) => return Err(e),
                Err(_) => return Ok(0), // producer gone => EOF
            }
        }
        let n = std::cmp::min(out.len(), self.buf.len());
        for i in 0..n {
            out[i] = self.buf.pop_front().unwrap();
        }
        Ok(n)
    }
}

impl Seek for Prefetch {
    fn seek(&mut self, _p: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "not seekable"))
    }
}

impl MediaSource for Prefetch {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

fn path_from_uri(uri: &str) -> Option<String> {
    if let Some(rest) = uri.strip_prefix("file://") {
        Some(percent_decode(rest))
    } else if uri.contains("://") {
        None
    } else {
        Some(uri.to_string())
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn open_http(uri: &str) -> Result<Box<dyn MediaSource + Send + 'static>> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(30))
        .build();
    let resp = agent
        .get(uri)
        .call()
        .map_err(|e| AudioError::UnsupportedSource(format!("http fetch {uri}: {e}")))?;
    let reader = Box::new(resp.into_reader());
    Ok(Box::new(Prefetch::start(reader)))
}

fn extension_of(uri: &str) -> Option<String> {
    let path = uri.rsplit_once(['?', '#']).map_or(uri, |a| a.0);
    let ext = path.rsplit_once('.')?.1.to_lowercase();
    if ext.len() <= 4 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        Some(ext)
    } else {
        None
    }
}

impl TrackSource {
    pub fn open(uri: &str) -> Result<TrackSource> {
        let (mss, seekable): (MediaSourceStream<'static>, bool) =
            if let Some(path) = path_from_uri(uri) {
                let file = std::fs::File::open(&path).map_err(|e| {
                    AudioError::UnsupportedSource(format!("open {path}: {e}"))
                })?;
                (
                    MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default()),
                    true,
                )
            } else if uri.starts_with("http://") || uri.starts_with("https://") {
                let src: Box<dyn MediaSource + Send + 'static> = open_http(uri)?;
                (
                    MediaSourceStream::new(src, MediaSourceStreamOptions::default()),
                    false,
                )
            } else {
                return Err(AudioError::UnsupportedSource(uri.into()));
            };

        let mut hint = Hint::new();
        if let Some(ext) = extension_of(uri) {
            hint.with_extension(&ext);
        }
        let reader = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|e| AudioError::Decode(format!("probe {uri}: {e}")))?;

        let track = reader
            .default_track(TrackType::Audio)
            .ok_or_else(|| AudioError::Decode(format!("no audio track in {uri}")))?;
        let track_id = track.id;
        let time_base = track.time_base;
        let num_frames = track.num_frames;

        let params = match track.codec_params.as_ref() {
            Some(CodecParameters::Audio(p)) => p,
            _ => return Err(AudioError::Decode("no decodable audio codec".into())),
        };
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|e| AudioError::Decode(format!("decoder: {e}")))?;

        let tb = time_base.or_else(|| params.sample_rate.and_then(TimeBase::try_from_recip));
        let mut duration_ms = match (track.duration, tb) {
            (Some(dur), Some(tb)) => tb.calc_duration(dur).map(|t| t.as_millis() as u64),
            _ => None,
        };
        if duration_ms.is_none() {
            if let (Some(nf), Some(sr)) = (num_frames, params.sample_rate) {
                duration_ms = Some(nf * 1000 / sr as u64);
            }
        }

        Ok(TrackSource {
            reader,
            decoder,
            track_id,
            uri: uri.into(),
            duration_ms,
            format: None,
            pending: VecDeque::new(),
            end_of_stream: false,
            scratch: Vec::new(),
            seekable,
        })
    }

    /// Decode one packet (or until some samples are available). Returns
    /// `Ok(true)` while samples remain somewhere (pending or stream),
    /// `Ok(false)` at end of stream with an empty pending queue.
    pub fn pump(&mut self) -> Result<bool> {
        if !self.pending.is_empty() {
            return Ok(true);
        }
        loop {
            if self.end_of_stream {
                return Ok(false);
            }
            match self.reader.next_packet() {
                Ok(None) => {
                    self.end_of_stream = true;
                    let _ = self.decoder.finalize();
                    return Ok(false);
                }
                Ok(Some(packet)) => {
                    if packet.track_id != self.track_id {
                        continue;
                    }
                    match self.decoder.decode(&packet) {
                        Ok(buf) => {
                            let spec = buf.spec();
                            let rate = spec.rate();
                            let channels = spec.channels().count() as u16;
                            let intrinsic = intrinsic_bits(&buf) as u32;
                            self.scratch.clear();
                            buf.copy_to_vec_interleaved::<i32>(&mut self.scratch);
                            drop(buf);
                            let bits = self
                                .decoder
                                .codec_params()
                                .bits_per_sample
                                .unwrap_or(if intrinsic == 32 {
                                    24
                                } else {
                                    intrinsic
                                }) as u8;
                            // symphonia left-aligns content inside the sample
                            // type; right-shift back to LSB alignment so the
                            // container stage can re-align losslessly.
                            let rshift = intrinsic.saturating_sub(bits as u32).min(31);
                            if rshift > 0 {
                                for v in self.scratch.iter_mut() {
                                    *v >>= rshift;
                                }
                            }
                            self.format = Some(PcmFormat {
                                sample_rate: rate,
                                channels,
                                bits,
                            });
                            self.pending.extend(self.scratch.drain(..));
                            return Ok(true);
                        }
                        Err(symphonia::core::errors::Error::DecodeError(_))
                        | Err(symphonia::core::errors::Error::IoError(_)) => {
                            // undecodable packet: skip
                            continue;
                        }
                        Err(e) => {
                            return Err(AudioError::Decode(format!("decode: {e}")));
                        }
                    }
                }
                Err(symphonia::core::errors::Error::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                // A truncated final frame surfaces as an IO error, not as
                // `Ok(None)` — treat it as end of stream.
                Err(symphonia::core::errors::Error::IoError(_)) => {
                    self.end_of_stream = true;
                    let _ = self.decoder.finalize();
                    return Ok(!self.pending.is_empty());
                }
                Err(e) => return Err(AudioError::Decode(format!("packet: {e}"))),
            }
        }
    }

    /// Accurate seek (seekable sources only).
    pub fn seek_ms(&mut self, ms: u64) -> Result<()> {
        let time = symphonia::core::units::Time::try_from_secs_f64(ms as f64 / 1000.0)
            .ok_or_else(|| AudioError::Decode("seek out of range".into()))?;
        self.reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|e| AudioError::Decode(format!("seek: {e}")))?;
        self.decoder.reset();
        self.pending.clear();
        self.end_of_stream = false;
        Ok(())
    }
}
