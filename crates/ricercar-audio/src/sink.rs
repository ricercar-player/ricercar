use crate::device::{DeviceInfo, DeviceKind, classify};
use crate::error::{AudioError, Result};
use crate::fmt::{Container, PcmFormat, append_container};

/// A byte sink for one track. `open` configures the exact source format:
/// sinks must refuse (never silently resample) what they cannot honor.
pub trait AudioSink: Send {
    fn device(&self) -> &DeviceInfo;
    fn opened_format(&self) -> Option<PcmFormat>;
    fn opened_container(&self) -> Option<Container>;
    /// (Re)open the sink at the exact given format.
    fn open(&mut self, fmt: PcmFormat) -> Result<()>;
    /// Write interleaved LSB-aligned i32 samples; consumes them all or errors.
    fn write_i32(&mut self, samples: &[i32]) -> Result<()>;
    /// Flush everything currently queued (used before a format change).
    fn drain(&mut self) -> Result<()>;
    /// Release the device.
    fn close(&mut self);
}

pub fn make_sink(device: &DeviceInfo) -> Box<dyn AudioSink> {
    match device.kind {
        DeviceKind::Null | DeviceKind::Hardware | DeviceKind::Virtual => {
            Box::new(AlsaSink::new(device.clone()))
        }
        DeviceKind::File => Box::new(FileSink::new(device.clone())),
    }
}

// ---------------------------------------------------------------- ALSA

pub struct AlsaSink {
    device: DeviceInfo,
    pcm: Option<alsa::PCM>,
    fmt: Option<PcmFormat>,
    container: Option<Container>,
    bytes: Vec<u8>,
}

impl AlsaSink {
    pub fn new(device: DeviceInfo) -> AlsaSink {
        AlsaSink {
            device,
            pcm: None,
            fmt: None,
            container: None,
            bytes: Vec::new(),
        }
    }
}

fn to_alsa_format(c: Container) -> alsa::pcm::Format {
    use alsa::pcm::Format;
    match c {
        Container::S16 => Format::S16LE,
        Container::S24 => Format::S24LE,
        Container::S24_3 => Format::S243LE,
        Container::S32 => Format::S32LE,
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
        let container = Container::for_bits(fmt.bits);
        let alsa_fmt = to_alsa_format(container);
        let pcm = alsa::PCM::new(&self.device.name, alsa::Direction::Playback, false)
            .map_err(|e| AudioError::Alsa {
                device: self.device.name.clone(),
                source: e,
            })?;
        let setup = (|| -> Result<()> {
            let alsa_err =
                |e: alsa::Error| AudioError::Alsa { device: self.device.name.clone(), source: e };
            let h = pcm.hw_params_current().map_err(alsa_err)?;
            h.set_access(alsa::pcm::Access::RWInterleaved)
                .map_err(alsa_err)?;
            h.set_format(alsa_fmt).map_err(alsa_err)?;
            h.set_channels(fmt.channels as u32).map_err(alsa_err)?;
            h.set_rate(fmt.sample_rate, alsa::ValueOr::Nearest)
                .map_err(alsa_err)?;
            let period = (fmt.sample_rate / 20).clamp(1024, 16384) as alsa::pcm::Frames;
            h.set_period_size_near(period, alsa::ValueOr::Nearest)
                .map_err(alsa_err)?;
            h.set_periods(4, alsa::ValueOr::Nearest).map_err(alsa_err)?;
            pcm.hw_params(&h).map_err(alsa_err)?;
            // Bit-perfect policy: what the device settled on must be exactly
            // what the track asked for — no silent resampling, ever.
            let got_rate = h.get_rate().map_err(alsa_err)?;
            let got_channels = h.get_channels().map_err(alsa_err)?;
            let got_format = h.get_format().map_err(alsa_err)?;
            if got_rate != fmt.sample_rate
                || got_channels != fmt.channels as u32
                || got_format != alsa_fmt
            {
                return Err(AudioError::UnsupportedFormat(self.device.name.clone()));
            }
            let s = pcm.sw_params_current().map_err(alsa_err)?;
            let period_size = h.get_period_size().map_err(alsa_err)?;
            s.set_start_threshold(period_size).map_err(alsa_err)?;
            s.set_avail_min(period_size).map_err(alsa_err)?;
            pcm.sw_params(&s).map_err(alsa_err)?;
            Ok(())
        })();
        match setup {
            Ok(()) => {
                self.pcm = Some(pcm);
                self.fmt = Some(fmt);
                self.container = Some(container);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn write_i32(&mut self, samples: &[i32]) -> Result<()> {
        let (pcm, container, content_bits, channels) = match (&self.pcm, self.container, self.fmt)
        {
            (Some(p), Some(c), Some(f)) => (p, c, f.bits, f.channels as usize),
            _ => return Err(AudioError::UnsupportedSource("sink not open".into())),
        };
        self.bytes.clear();
        append_container(&mut self.bytes, samples, container, content_bits);
        let io = pcm
            .io_u8()
            .map_err(|e| AudioError::Alsa {
                device: self.device.name.clone(),
                source: e,
            })?;
        let frame_bytes = container.bytes_per_sample() * channels;
        let mut off = 0usize;
        while off < self.bytes.len() {
            match io.writei(&self.bytes[off..]) {
                Ok(frames) => {
                    let n = frames * frame_bytes;
                    if n == 0 {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    off += n;
                }
                Err(e) => {
                    // EPIPE (xrun) and EAGAIN are recoverable.
                    if matches!(e.errno(), 32 | 11) {
                        let _ = pcm.recover(e.errno(), false);
                        continue;
                    }
                    return Err(AudioError::Alsa {
                        device: self.device.name.clone(),
                        source: e,
                    });
                }
            }
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<()> {
        if let Some(pcm) = &self.pcm {
            pcm.drain()
                .map_err(|e| AudioError::Alsa {
                    device: self.device.name.clone(),
                    source: e,
                })?;
        }
        Ok(())
    }

    fn close(&mut self) {
        self.pcm = None;
        self.fmt = None;
        self.container = None;
    }
}

// ---------------------------------------------------------------- file

pub struct FileSink {
    device: DeviceInfo,
    fmt: Option<PcmFormat>,
    container: Option<Container>,
    file: Option<std::fs::File>,
    bytes: Vec<u8>,
}

impl FileSink {
    pub fn new(device: DeviceInfo) -> FileSink {
        FileSink {
            device,
            fmt: None,
            container: None,
            file: None,
            bytes: Vec::new(),
        }
    }
    fn path(&self) -> String {
        self.device
            .name
            .strip_prefix("file:")
            .unwrap_or("/tmp/ricercar-out.raw")
            .to_string()
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
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path())
            .map_err(AudioError::Io)?;
        self.container = Some(Container::for_bits(fmt.bits));
        self.fmt = Some(fmt);
        self.file = Some(file);
        Ok(())
    }
    fn write_i32(&mut self, samples: &[i32]) -> Result<()> {
        let (Some(file), Some(container), Some(f)) =
            (&mut self.file, self.container, self.fmt)
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
        self.file = None;
        self.fmt = None;
        self.container = None;
    }
}

// ---------------------------------------------------------------- null

pub struct NullSink {
    device: DeviceInfo,
    fmt: Option<PcmFormat>,
    container: Option<Container>,
    frames: u64,
    bytes: Vec<u8>,
}

impl NullSink {
    pub fn new(device: DeviceInfo) -> NullSink {
        NullSink {
            device,
            fmt: None,
            container: None,
            frames: 0,
            bytes: Vec::new(),
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
        self.container = Some(Container::for_bits(fmt.bits));
        self.fmt = Some(fmt);
        self.frames = 0;
        Ok(())
    }
    fn write_i32(&mut self, samples: &[i32]) -> Result<()> {
        let (Some(container), Some(f)) = (self.container, self.fmt) else {
            return Err(AudioError::UnsupportedSource("sink not open".into()));
        };
        self.bytes.clear();
        append_container(&mut self.bytes, samples, container, f.bits);
        self.frames += (self.bytes.len()
            / (container.bytes_per_sample() * f.channels as usize)) as u64;
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
    }
}
