# simple-video

Cross-platform video capture, latest-frame streams, frame conversion, and optional
H264/AV1 codecs for `generic_messenger`.

The default `capture` feature provides webcam and screen discovery on Linux,
macOS, and Windows through `nokhwa` and `scap`. Frame/codec-only users can
disable default features.

## Discover and stream

`capture_sources()` returns webcams followed by screens and windows. Starting a
stream opens and owns the native device on a dedicated thread. Slow consumers
skip stale frames instead of accumulating latency.

```rust,no_run
use simple_video::{CaptureError, capture_sources};

async fn preview_first_source() -> Result<(), CaptureError> {
    let Some(source) = capture_sources().into_iter().next() else {
        return Ok(());
    };
    println!("Streaming {}", source.name());

    let mut stream = source.stream().await?;
    while let Some(frame) = stream.next_frame().await? {
        println!("{}x{}", frame.width, frame.height);
    }
    Ok(())
}
```

Webcam discovery failures do not hide usable screen sources. Use
`try_capture_sources()` when caller needs strict discovery errors.

Dropping the stream requests cooperative shutdown without blocking. Native
device reads cannot always be interrupted. `CaptureStream::shutdown()` can
remain pending if a backend read never returns; plain drop detaches that worker
instead of blocking its caller.

## Capture behavior

- Webcam and screen frames use `PixelData::Rgb8`.
- High-level streams default to 1280x720 and publish at most 30 FPS.
- Low-level capture methods block and should not run on UI or async executor
  threads.
- Capture errors remain typed as `CaptureError`.

On macOS, call `initialize_webcams` before discovery and continue from its
callback after permission resolves.

On Linux, screen capture uses the desktop portal and PipeWire. Target choice
happens when the default screen source opens. macOS and Windows also expose
enumerated display/window sources.

## Webcam control

```rust,no_run
use simple_video::{CaptureError, WebcamCaptureConfig, webcams};

fn inspect_webcam() -> Result<(), CaptureError> {
    let Some(webcam) = webcams()?.into_iter().next() else {
        return Ok(());
    };
    for format in webcam.formats()? {
        println!("{format:?}");
    }

    let mut capture = webcam.open(WebcamCaptureConfig::default())?;
    let frame = capture.next_frame()?;
    println!("{}x{}", frame.width, frame.height);
    Ok(())
}
```

## Screen permissions

```rust,no_run
use simple_video::{
    CaptureError, ScreenCaptureConfig, has_screen_capture_permission,
    request_screen_capture_permission, screen_capture_supported, screen_sources,
};

fn capture_one_screen_frame() -> Result<(), CaptureError> {
    if !screen_capture_supported() {
        return Ok(());
    }
    if !has_screen_capture_permission() && !request_screen_capture_permission() {
        return Ok(());
    }

    let source = screen_sources().remove(0);
    let mut capture = source.open(ScreenCaptureConfig::default())?;
    let frame = capture.next_frame()?;
    println!("{}x{}", frame.width, frame.height);
    Ok(())
}
```

## Features

- `capture` (default): webcam and screen discovery/streaming.
- `h264`: OpenH264 encoder and decoder.
- `av1`: rav1e encoder.

Codec-only dependency example:

```toml
simple-video = { version = "0.0.1", default-features = false, features = ["h264"] }
```

## Migrating from `simple-video-capture`

Remove the `simple-video-capture` dependency and import its capture types from
`simple_video` instead. Existing low-level names remain at crate root;
`sources()` remains as strict compatibility alias, while `capture_sources()`
adds partial-safe high-level discovery.
