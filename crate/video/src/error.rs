use std::{
    error::Error,
    fmt::{self, Display, Formatter},
};

/// Error produced while validating, converting, encoding, or decoding video.
#[derive(Debug)]
#[non_exhaustive]
pub enum VideoError {
    InvalidDimensions,
    InvalidFrameRate {
        minimum: u32,
        maximum: u32,
        actual: u32,
    },
    InvalidBitrate {
        minimum: u32,
        maximum: u32,
        actual: u32,
    },
    FrameSizeOverflow,
    FrameSizeMismatch {
        expected: usize,
        actual: usize,
    },
    UnsupportedPixelFormat,
    UnsupportedCodec,
    EncodedFrameTooLarge {
        limit: usize,
        actual: usize,
    },
    #[cfg(feature = "av1")]
    Av1Config(rav1e::InvalidConfig),
    #[cfg(feature = "av1")]
    Av1Encoder(rav1e::EncoderStatus),
    #[cfg(feature = "h264")]
    Codec(openh264::Error),
}

impl Display for VideoError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDimensions => formatter.write_str("invalid video frame dimensions"),
            Self::InvalidFrameRate {
                minimum,
                maximum,
                actual,
            } => write!(
                formatter,
                "video frame rate must be between {minimum} and {maximum} FPS, got {actual}"
            ),
            Self::InvalidBitrate {
                minimum,
                maximum,
                actual,
            } => write!(
                formatter,
                "video bitrate must be between {minimum} and {maximum} bps, got {actual}"
            ),
            Self::FrameSizeOverflow => formatter.write_str("video frame size overflow"),
            Self::FrameSizeMismatch { expected, actual } => {
                write!(
                    formatter,
                    "video frame size does not match dimensions: expected {expected}, got {actual}"
                )
            }
            Self::UnsupportedPixelFormat => {
                formatter.write_str("unsupported video frame pixel format")
            }
            Self::UnsupportedCodec => formatter.write_str("unsupported video codec"),
            Self::EncodedFrameTooLarge { limit, actual } => {
                write!(
                    formatter,
                    "encoded video frame exceeds {limit}-byte limit: got {actual}"
                )
            }
            #[cfg(feature = "av1")]
            Self::Av1Config(error) => Display::fmt(error, formatter),
            #[cfg(feature = "av1")]
            Self::Av1Encoder(error) => Display::fmt(error, formatter),
            #[cfg(feature = "h264")]
            Self::Codec(error) => Display::fmt(error, formatter),
        }
    }
}

impl Error for VideoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            #[cfg(feature = "av1")]
            Self::Av1Config(error) => Some(error),
            #[cfg(feature = "av1")]
            Self::Av1Encoder(error) => Some(error),
            #[cfg(feature = "h264")]
            Self::Codec(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(feature = "av1")]
impl From<rav1e::InvalidConfig> for VideoError {
    fn from(error: rav1e::InvalidConfig) -> Self {
        Self::Av1Config(error)
    }
}

#[cfg(feature = "av1")]
impl From<rav1e::EncoderStatus> for VideoError {
    fn from(error: rav1e::EncoderStatus) -> Self {
        Self::Av1Encoder(error)
    }
}

#[cfg(feature = "h264")]
impl From<openh264::Error> for VideoError {
    fn from(error: openh264::Error) -> Self {
        Self::Codec(error)
    }
}
