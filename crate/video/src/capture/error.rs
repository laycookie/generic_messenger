use std::{io, sync::mpsc::RecvError};

use facet::Facet;
use facet_error as error;
use nokhwa::NokhwaError;
use scap::capturer::CapturerBuildError;

use super::ScreenSourceKind;
use crate::VideoError;

/// Error returned while discovering or capturing video sources.
#[derive(Debug, Facet)]
#[facet(derive(Error))]
#[non_exhaustive]
#[repr(u8)]
pub enum CaptureError {
    /// webcam backend failed: {0}
    Webcam(
        #[facet(opaque)]
        #[facet(error::from)]
        NokhwaError,
    ),

    /// screen capture could not start: {0}
    ScreenBuild(
        #[facet(opaque)]
        #[facet(error::from)]
        CapturerBuildError,
    ),

    /// screen capture backend panicked: {0}
    ScreenBackendPanicked(String),

    /// screen {kind:?} source {id} is no longer available
    ScreenSourceUnavailable {
        #[facet(opaque)]
        kind: ScreenSourceKind,
        id: u32,
    },

    /// another screen capture is already active in this process
    ScreenCaptureAlreadyActive,

    /// screen capture stream ended
    ScreenStreamEnded(
        #[facet(opaque)]
        #[facet(error::from)]
        RecvError,
    ),

    /// screen backend returned unsupported {0} pixels
    UnexpectedScreenFormat(&'static str),

    /// capture backend returned invalid dimensions {width}x{height}
    InvalidDimensions { width: i32, height: i32 },

    /// invalid capture configuration: {0}
    InvalidConfiguration(&'static str),

    /// webcam reported no usable capture formats
    NoUsableWebcamFormat,

    /// capture backend returned an invalid frame: {0}
    InvalidFrame(
        #[facet(opaque)]
        #[facet(error::from)]
        VideoError,
    ),

    /// could not spawn capture worker: {0}
    WorkerSpawn(
        #[facet(opaque)]
        #[facet(error::source)]
        io::Error,
    ),

    /// capture worker panicked: {0}
    WorkerPanicked(String),
}

/// Result type used throughout this crate.
pub type CaptureResult<T> = Result<T, CaptureError>;

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    #[test]
    fn facet_error_formats_fields_and_exposes_sources() {
        let dimensions = CaptureError::InvalidDimensions {
            width: 1920,
            height: 0,
        };
        assert_eq!(
            dimensions.to_string(),
            "capture backend returned invalid dimensions 1920x0"
        );

        let worker = CaptureError::WorkerSpawn(io::Error::other("thread limit"));
        assert_eq!(
            worker.to_string(),
            "could not spawn capture worker: thread limit"
        );
        assert!(worker.source().is_some());
    }
}
