use thiserror::Error;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("alsa error on device '{device}': {source}")]
    Alsa {
        device: String,
        #[source]
        source: alsa::Error,
    },
    #[error("unsupported source: {0}")]
    UnsupportedSource(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error(
        "device '{0}' does not support the track format (bit-perfect policy refuses resampling)"
    )]
    UnsupportedFormat(String),
    #[error("device '{device}' cannot play {detail} bit-perfectly")]
    FormatRefused { device: String, detail: String },
    #[error("audio device '{device}' disappeared (unplugged or powered off?)")]
    DeviceGone { device: String },
    #[error("engine is shut down")]
    ShutDown,
}

pub type Result<T> = std::result::Result<T, AudioError>;
