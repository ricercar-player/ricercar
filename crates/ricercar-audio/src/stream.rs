use std::collections::VecDeque;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::TimeBase;

use crate::error::{AudioError, Result};
use crate::fmt::PcmFormat;
use crate::http;

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
    time_base: Option<TimeBase>,
    titles: Option<crossbeam_channel::Receiver<String>>,
}

/// Integer width of the decoded buffer; `None` for float output (lossy
/// codecs), whose full-scale content we keep 24 bits of, like every other
/// lossy player.
fn native_bits(b: &GenericAudioBufferRef) -> Option<u8> {
    use GenericAudioBufferRef as G;
    match b {
        G::U8(_) | G::S8(_) => Some(8),
        G::U16(_) | G::S16(_) => Some(16),
        G::U24(_) | G::S24(_) => Some(24),
        G::U32(_) | G::S32(_) => Some(32),
        G::F32(_) | G::F64(_) => None,
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
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
        let (src, seekable, titles): (Box<dyn MediaSource>, bool, _) =
            if let Some(path) = path_from_uri(uri) {
                let file = std::fs::File::open(&path)
                    .map_err(|e| AudioError::UnsupportedSource(format!("open {path}: {e}")))?;
                (Box::new(file), true, None)
            } else if uri.starts_with("http://") || uri.starts_with("https://") {
                let h = http::open(uri)?;
                (h.source, h.seekable, h.titles)
            } else {
                return Err(AudioError::UnsupportedSource(uri.into()));
            };
        let mss = MediaSourceStream::new(src, MediaSourceStreamOptions::default());

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
        if duration_ms.is_none()
            && let (Some(nf), Some(sr)) = (num_frames, params.sample_rate)
        {
            duration_ms = Some(nf * 1000 / sr as u64);
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
            time_base: tb,
            titles,
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
                            let native = native_bits(&buf);
                            self.scratch.clear();
                            buf.copy_to_vec_interleaved::<i32>(&mut self.scratch);
                            let bits = self
                                .decoder
                                .codec_params()
                                .bits_per_sample
                                .map(|b| b as u8)
                                .or(native)
                                .unwrap_or(24)
                                .clamp(1, 32);
                            // Conversion to i32 scales every sample type to
                            // full range (content MSB-aligned); shift back to
                            // LSB alignment so the container stage can
                            // re-align losslessly.
                            let rshift = 32 - bits as u32;
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

    /// Latest stream title announced by the source (internet radio), if a
    /// new one arrived since the last call.
    pub fn take_stream_title(&mut self) -> Option<String> {
        self.titles.as_ref().and_then(|rx| rx.try_iter().last())
    }

    /// Seek (seekable sources only). Returns the position actually reached:
    /// the demuxer lands on a packet boundary at or before the request, and
    /// that is where playback resumes.
    pub fn seek_ms(&mut self, ms: u64) -> Result<std::time::Duration> {
        let time = symphonia::core::units::Time::try_from_secs_f64(ms as f64 / 1000.0)
            .ok_or_else(|| AudioError::Decode("seek out of range".into()))?;
        let seeked = self
            .reader
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
        let actual = self
            .time_base
            .and_then(|tb| tb.calc_time(seeked.actual_ts))
            .map_or(ms as f64 / 1000.0, |t| t.as_secs_f64().max(0.0));
        Ok(std::time::Duration::from_secs_f64(actual))
    }
}
