use std::sync::atomic::{AtomicU8, Ordering};

use simple_video::{VideoCodec, VideoError};

/// Audio codecs Discord accepts in its Voice Gateway codec list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub(crate) enum DiscordAudioCodec {
    Opus,
}

impl DiscordAudioCodec {
    pub(crate) const fn wire_name(self) -> &'static str {
        match self {
            Self::Opus => "opus",
        }
    }
}

/// Video codecs Discord accepts in its Voice Gateway codec list.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
#[repr(u8)]
pub(crate) enum DiscordVideoCodec {
    #[default]
    H264 = 0,
    Av1 = 1,
}

impl DiscordVideoCodec {
    pub(crate) const fn wire_name(self) -> &'static str {
        match self {
            Self::H264 => "H264",
            Self::Av1 => "AV1",
        }
    }

    pub(crate) fn from_wire_name(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("H264") {
            Some(Self::H264)
        } else if name.eq_ignore_ascii_case("AV1") {
            Some(Self::Av1)
        } else {
            None
        }
    }

    const fn from_atomic_value(value: u8) -> Self {
        match value {
            0 => Self::H264,
            1 => Self::Av1,
            _ => Self::H264,
        }
    }
}

impl TryFrom<VideoCodec> for DiscordVideoCodec {
    type Error = VideoError;

    fn try_from(codec: VideoCodec) -> Result<Self, Self::Error> {
        match codec {
            VideoCodec::H264 => Ok(Self::H264),
            VideoCodec::Av1 => Ok(Self::Av1),
            _ => Err(VideoError::UnsupportedCodec),
        }
    }
}

impl From<DiscordVideoCodec> for VideoCodec {
    fn from(codec: DiscordVideoCodec) -> Self {
        match codec {
            DiscordVideoCodec::H264 => Self::H264,
            DiscordVideoCodec::Av1 => Self::Av1,
        }
    }
}

/// Lock-free shared Discord video preference.
#[derive(Debug)]
pub(crate) struct AtomicDiscordVideoCodec(AtomicU8);

impl AtomicDiscordVideoCodec {
    pub(crate) const fn new(codec: DiscordVideoCodec) -> Self {
        Self(AtomicU8::new(codec as u8))
    }

    pub(crate) fn load(&self, ordering: Ordering) -> DiscordVideoCodec {
        DiscordVideoCodec::from_atomic_value(self.0.load(ordering))
    }

    pub(crate) fn swap(&self, codec: DiscordVideoCodec, ordering: Ordering) -> DiscordVideoCodec {
        DiscordVideoCodec::from_atomic_value(self.0.swap(codec as u8, ordering))
    }
}

impl Default for AtomicDiscordVideoCodec {
    fn default() -> Self {
        Self::new(DiscordVideoCodec::H264)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_codec_conversion_rejects_future_variants() {
        assert!(matches!(
            DiscordVideoCodec::try_from(VideoCodec::H264),
            Ok(DiscordVideoCodec::H264)
        ));
        assert!(matches!(
            DiscordVideoCodec::try_from(VideoCodec::Av1),
            Ok(DiscordVideoCodec::Av1)
        ));
    }

    #[test]
    fn atomic_codec_defaults_to_h264_and_switches() {
        let codec = AtomicDiscordVideoCodec::default();

        assert_eq!(codec.load(Ordering::Acquire), DiscordVideoCodec::H264);
        assert_eq!(
            codec.swap(DiscordVideoCodec::Av1, Ordering::AcqRel),
            DiscordVideoCodec::H264
        );
        assert_eq!(codec.load(Ordering::Acquire), DiscordVideoCodec::Av1);
    }
}
