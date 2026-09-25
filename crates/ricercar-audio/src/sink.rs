use crate::device::{DeviceInfo, DeviceKind, alsa_format, classify, supported_containers};
use crate::error::{AudioError, Result};
use crate::fmt::{Container, PcmFormat, append_container, negotiate};

/// A byte sink for one track. `open` configures the exact source format:
/// sinks must refuse (never silently resample) what they cannot honor.
pub trait AudioSink: Send {
    fn device(&self) -> &DeviceInfo;
    fn opened_format(&self) -> Option<PcmFormat>;
    /// Container actually negotiated with the device by the last `open`.
    fn opened_container(&self) -> Option<Container>;
    /// (Re)open the sink at the exact given format.
    fn open(&mut self, fmt: PcmFormat) -> Result<()>;
    /// Write interleaved LSB-aligned i32 samples; consumes them all or errors.
    fn write_i32(&mut self, samples: &[i32]) -> Result<()>;
    /// Flush everything currently queued (used before a format change).
    fn drain(&mut self) -> Result<()>;
    /// Release the device.
    fn close(&mut self);
    /// Halt output without underrunning; `resume` continues where it stopped.
    fn pause(&mut self) -> Result<()> {
        Ok(())
    }
    fn resume(&mut self) -> Result<()> {
        Ok(())
    }
    /// Throw away queued, not yet audible audio (seek / stop).
    fn discard(&mut self) -> Result<()> {
        Ok(())
    }
    /// Frames written but not yet audible.
    fn delay_frames(&self) -> u64 {
        0
    }
}

pub fn make_sink(device: &DeviceInfo) -> Box<dyn AudioSink> {
    match device.kind {
        DeviceKind::Null | DeviceKind::Hardware | DeviceKind::Virtual => {
            Box::new(AlsaSink::new(device.clone()))
        }
        DeviceKind::File => Box::new(FileSink::new(device.clone())),
    }
}

fn refuse(device: &DeviceInfo, detail: String) -> AudioError {
    AudioError::FormatRefused {
        device: device.name.clone(),
        detail,
    }
}

fn negotiate_or_refuse(
    device: &DeviceInfo,
    fmt: PcmFormat,
    supported: &[Container],
) -> Result<Container> {
    negotiate(fmt.bits, supported).ok_or_else(|| {
        let labels: Vec<&str> = supported.iter().map(|c| c.label()).collect();
        refuse(
            device,
            format!(
                "{}-bit content (it only accepts {})",
                fmt.bits,
                if labels.is_empty() {
                    "no linear PCM format".to_string()
                } else {
                    labels.join(", ")
                }
            ),
        )
    })
}

// ---------------------------------------------------------------- ALSA

// errno values (positive, as reported by `alsa::Error::errno`).
const EAGAIN: i32 = 11;
const ENODEV: i32 = 19;
const EPIPE: i32 = 32;
const ESTRPIPE: i32 = 86;
const ESHUTDOWN: i32 = 108;

pub struct AlsaSink {
    device: DeviceInfo,
    pcm: Option<alsa::PCM>,
    fmt: Option<PcmFormat>,
    container: Option<Container>,
    can_pause: bool,
    bytes: Vec<u8>,
}

impl AlsaSink {
    pub fn new(device: DeviceInfo) -> AlsaSink {
        AlsaSink {
            device,
            pcm: None,
            fmt: None,
            container: None,
            can_pause: false,
            bytes: Vec::new(),
        }
    }

    fn err(&self, e: alsa::Error) -> AudioError {
        if matches!(e.errno(), ENODEV | ESHUTDOWN) {
            AudioError::DeviceGone {
                device: self.device.name.clone(),
            }
        } else {
            AudioError::Alsa {
                device: self.device.name.clone(),
                source: e,
            }
        }
    }

    fn configure(&self, pcm: &alsa::PCM, fmt: PcmFormat) -> Result<(Container, bool)> {
        let err = |e| self.err(e);
        let h = alsa::pcm::HwParams::any(pcm).map_err(err)?;
        h.set_access(alsa::pcm::Access::RWInterleaved)
            .map_err(err)?;
        let container = negotiate_or_refuse(&self.device, fmt, &supported_containers(&h))?;
        let alsa_fmt = alsa_format(container);
        h.set_format(alsa_fmt).map_err(err)?;
        h.set_channels(fmt.channels as u32)
            .map_err(|_| refuse(&self.device, format!("{} channels", fmt.channels)))?;
        if h.test_rate(fmt.sample_rate).is_err() {
            return Err(refuse(&self.device, format!("{} Hz", fmt.sample_rate)));
        }
        h.set_rate(fmt.sample_rate, alsa::ValueOr::Nearest)
            .map_err(err)?;
        let period = (fmt.sample_rate / 20).clamp(1024, 16384) as alsa::pcm::Frames;
        h.set_period_size_near(period, alsa::ValueOr::Nearest)
            .map_err(err)?;
        h.set_periods(4, alsa::ValueOr::Nearest).map_err(err)?;
        pcm.hw_params(&h).map_err(err)?;

        // Bit-perfect policy: what the device settled on must be exactly what
        // the track asked for — no silent resampling, ever.
        let cur = pcm.hw_params_current().map_err(err)?;
        let got = (
            cur.get_rate().map_err(err)?,
            cur.get_channels().map_err(err)?,
            cur.get_format().map_err(err)?,
        );
        if got != (fmt.sample_rate, fmt.channels as u32, alsa_fmt) {
            return Err(AudioError::UnsupportedFormat(self.device.name.clone()));
        }
        let can_pause = cur.can_pause();
        let period_size = cur.get_period_size().map_err(err)?;
        let s = pcm.sw_params_current().map_err(err)?;
        s.set_start_threshold(period_size).map_err(err)?;
        s.set_avail_min(period_size).map_err(err)?;
        pcm.sw_params(&s).map_err(err)?;
        Ok((container, can_pause))
    }
}

impl AudioSink for AlsaSink {
    fn device(&self) -> &DeviceInfo {
        &self.device
    }
    fn opened_format(&self) -> Option<PcmFormat> {
        self.fmt
    }
    fn opened_container(&self) -> Option<Container> {
        self.container
    }

    fn open(&mut self, fmt: PcmFormat) -> Result<()> {
        self.close();
        let pcm = alsa::PCM::new(&self.device.name, alsa::Direction::Playback, false)
            .map_err(|e| self.err(e))?;
        let (container, can_pause) = self.configure(&pcm, fmt)?;
        self.pcm = Some(pcm);
        self.fmt = Some(fmt);
        self.container = Some(container);
        self.can_pause = can_pause;
        Ok(())
    }

    fn write_i32(&mut self, samples: &[i32]) -> Result<()> {
        let (Some(pcm), Some(container), Some(f)) = (&self.pcm, self.container, self.fmt) else {
            return Err(AudioError::UnsupportedSource("sink not open".into()));
        };
        self.bytes.clear();
        append_container(&mut self.bytes, samples, container, f.bits);
        let io = pcm.io_bytes();
        let frame_bytes = container.bytes_per_sample() * f.channels as usize;
        let mut off = 0usize;
        let mut recoveries = 0;
        while off < self.bytes.len() {
            match io.writei(&self.bytes[off..]) {
                Ok(0) => std::thread::sleep(std::time::Duration::from_millis(2)),
                Ok(frames) => {
                    off += frames * frame_bytes;
                    recoveries = 0;
                }
                // Underrun / suspend / would-block are recoverable; give up if
                // recovery keeps failing rather than spinning forever.
                Err(e) if matches!(e.errno(), EPIPE | ESTRPIPE | EAGAIN) && recoveries < 8 => {
                    recoveries += 1;
                    if e.errno() != EAGAIN {
                        pcm.recover(e.errno(), true).map_err(|e| self.err(e))?;
                    }
                }
                Err(e) => return Err(self.err(e)),
            }
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<()> {
        if let Some(pcm) = &self.pcm {
            pcm.drain().map_err(|e| self.err(e))?;
        }
        Ok(())
    }

    fn close(&mut self) {
        self.pcm = None;
        self.fmt = None;
        self.container = None;
        self.can_pause = false;
    }

    fn pause(&mut self) -> Result<()> {
        use alsa::pcm::State;
        let Some(pcm) = &self.pcm else { return Ok(()) };
        if pcm.state() != State::Running {
            // Prepared (not started yet) cannot underrun; nothing to do.
            return Ok(());
        }
        // Hardware pause keeps the buffer; otherwise drop it (a few hundred
        // ms are lost) so the device stops cleanly instead of underrunning.
        if self.can_pause && pcm.pause(true).is_ok() {
            return Ok(());
        }
        pcm.drop().map_err(|e| self.err(e))
    }

    fn resume(&mut self) -> Result<()> {
        use alsa::pcm::State;
        let Some(pcm) = &self.pcm else { return Ok(()) };
        match pcm.state() {
            State::Paused => pcm.pause(false),
            State::Setup | State::XRun => pcm.prepare(),
            _ => Ok(()),
        }
        .map_err(|e| self.err(e))
    }

    fn discard(&mut self) -> Result<()> {
        if let Some(pcm) = &self.pcm {
            pcm.drop().map_err(|e| self.err(e))?;
            pcm.prepare().map_err(|e| self.err(e))?;
        }
        Ok(())
    }

    fn delay_frames(&self) -> u64 {
        self.pcm
            .as_ref()
            .and_then(|p| p.delay().ok())
            .map_or(0, |d| d.max(0) as u64)
    }
}

// ---------------------------------------------------------------- file

/// `file:<path>[?formats=S32_LE,S24_3LE]`: the optional list restricts the
/// containers the sink accepts, emulating a DAC's capabilities.
pub fn parse_file_spec(name: &str) -> (String, Vec<Container>) {
    let spec = name.strip_prefix("file:").unwrap_or(name);
    if let Some((path, list)) = spec.rsplit_once("?formats=") {
        let formats: Option<Vec<Container>> = list.split(',').map(Container::from_label).collect();
        if let Some(formats) = formats {
            return (path.to_string(), formats);
        }
    }
    (spec.to_string(), Container::ALL.to_vec())
}

/// Writes exactly the bytes that would be handed to ALSA for the negotiated
/// container, appended across tracks.
pub struct FileSink {
    device: DeviceInfo,
    path: String,
    supported: Vec<Container>,
    fmt: Option<PcmFormat>,
    container: Option<Container>,
    file: Option<std::io::BufWriter<std::fs::File>>,
    bytes: Vec<u8>,
}

impl FileSink {
    pub fn new(device: DeviceInfo) -> FileSink {
        let (path, supported) = parse_file_spec(&device.name);
        FileSink {
            device,
            path,
            supported,
            fmt: None,
            container: None,
            file: None,
            bytes: Vec::new(),
        }
    }
}

impl AudioSink for FileSink {
    fn device(&self) -> &DeviceInfo {
        &self.device
    }
    fn opened_format(&self) -> Option<PcmFormat> {
        self.fmt
    }
    fn opened_container(&self) -> Option<Container> {
        self.container
    }
    fn open(&mut self, fmt: PcmFormat) -> Result<()> {
        self.close();
        let container = negotiate_or_refuse(&self.device, fmt, &self.supported)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(AudioError::Io)?;
        self.container = Some(container);
        self.fmt = Some(fmt);
        self.file = Some(std::io::BufWriter::new(file));
        Ok(())
    }
    fn write_i32(&mut self, samples: &[i32]) -> Result<()> {
        let (Some(file), Some(container), Some(f)) = (&mut self.file, self.container, self.fmt)
        else {
            return Err(AudioError::UnsupportedSource("sink not open".into()));
        };
        self.bytes.clear();
        append_container(&mut self.bytes, samples, container, f.bits);
        use std::io::Write;
        file.write_all(&self.bytes).map_err(AudioError::Io)
    }
    fn drain(&mut self) -> Result<()> {
        if let Some(f) = &mut self.file {
            use std::io::Write;
            f.flush().map_err(AudioError::Io)?;
        }
        Ok(())
    }
    fn close(&mut self) {
        if let Some(mut f) = self.file.take() {
            use std::io::Write;
            let _ = f.flush();
        }
        self.fmt = None;
        self.container = None;
    }
    fn pause(&mut self) -> Result<()> {
        // Make everything written so far observable while paused.
        self.drain()
    }
}

// ---------------------------------------------------------------- null

pub struct NullSink {
    device: DeviceInfo,
    fmt: Option<PcmFormat>,
    container: Option<Container>,
    frames: u64,
}

impl NullSink {
    pub fn new(device: DeviceInfo) -> NullSink {
        NullSink {
            device,
            fmt: None,
            container: None,
            frames: 0,
        }
    }
    pub fn frames_written(&self) -> u64 {
        self.frames
    }
}

impl AudioSink for NullSink {
    fn device(&self) -> &DeviceInfo {
        &self.device
    }
    fn opened_format(&self) -> Option<PcmFormat> {
        self.fmt
    }
    fn opened_container(&self) -> Option<Container> {
        self.container
    }
    fn open(&mut self, fmt: PcmFormat) -> Result<()> {
        self.container = Some(negotiate_or_refuse(&self.device, fmt, &Container::ALL)?);
        self.fmt = Some(fmt);
        self.frames = 0;
        Ok(())
    }
    fn write_i32(&mut self, samples: &[i32]) -> Result<()> {
        let Some(f) = self.fmt else {
            return Err(AudioError::UnsupportedSource("sink not open".into()));
        };
        self.frames += (samples.len() / f.channels.max(1) as usize) as u64;
        Ok(())
    }
    fn drain(&mut self) -> Result<()> {
        Ok(())
    }
    fn close(&mut self) {
        self.fmt = None;
        self.container = None;
    }
}

/// Sink selected by device string (`null`, `file:<path>`, else ALSA).
pub fn device_info(name: &str) -> DeviceInfo {
    DeviceInfo {
        name: name.into(),
        description: "user selected".into(),
        kind: classify(name),
        card_name: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Container::*;

    #[test]
    fn file_spec_parsing() {
        assert_eq!(
            parse_file_spec("file:/tmp/a.raw"),
            ("/tmp/a.raw".to_string(), Container::ALL.to_vec())
        );
        assert_eq!(
            parse_file_spec("file:/tmp/a.raw?formats=S32_LE,S24_3LE"),
            ("/tmp/a.raw".to_string(), vec![S32, S24_3])
        );
        // Unknown labels: the whole thing is a path.
        assert_eq!(
            parse_file_spec("file:/tmp/a?formats=FLOAT").0,
            "/tmp/a?formats=FLOAT"
        );
    }

    #[test]
    fn file_sink_refuses_what_it_cannot_carry() {
        let mut s = FileSink::new(device_info("file:/nonexistent/x?formats=S16_LE"));
        let fmt = PcmFormat {
            sample_rate: 96000,
            channels: 2,
            bits: 24,
        };
        assert!(matches!(s.open(fmt), Err(AudioError::FormatRefused { .. })));
    }
}
