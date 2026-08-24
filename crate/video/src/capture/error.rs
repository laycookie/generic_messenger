use std::{io, sync::mpsc::RecvError};

use super::ScreenSourceKind;
use crate::VideoError;
use nokhwa::NokhwaError;
use scap::capturer::CapturerBuildError;
use thiserror::Error;

/// Error returned while discovering or capturing video sources.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CaptureError {
    #[error("webcam backend failed: {0}")]
    Webcam(#[from] NokhwaError),
    #[error("screen capture could not start: {0}")]
    ScreenBuild(#[from] CapturerBuildError),
    #[error("screen capture backend panicked: {0}")]
    ScreenBackendPanicked(String),
    #[error("screen {kind:?} source {id} is no longer available")]
    ScreenSourceUnavailable { kind: ScreenSourceKind, id: u32 },
    #[error("another screen capture is already active in this process")]
    ScreenCaptureAlreadyActive,
    #[error("screen capture stream ended")]
    ScreenStreamEnded(#[from] RecvError),
    #[error("screen backend returned unsupported {0} pixels")]
    UnexpectedScreenFormat(&'static str),
    #[error("capture backend returned invalid dimensions {width}x{height}")]
    InvalidDimensions { width: i32, height: i32 },
    #[error("invalid capture configuration: {0}")]
    InvalidConfiguration(&'static str),
    #[error("webcam reported no usable capture formats")]
    NoUsableWebcamFormat,
    #[error("capture backend returned an invalid frame: {0}")]
    InvalidFrame(#[from] VideoError),
    #[error("could not spawn capture worker: {0}")]
    WorkerSpawn(#[source] io::Error),
    #[error("capture worker panicked: {0}")]
    WorkerPanicked(String),
}

/// Result type used throughout this crate.
pub type CaptureResult<T> = Result<T, CaptureError>;
