# simple-video-capture

Cross-platform webcam and screen capture for `generic_messenger`.

- `nokhwa` discovers webcams and decodes native camera formats.
- `scap` captures displays and windows through native operating-system APIs.
- Public capture methods return `simple_video::VideoFrame`.
- Webcam frames use `PixelData::Rgb8`.
- Screen frames use `PixelData::Rgba8`.
- Capture methods block. Run capture loops outside UI or async executor threads.

## Webcam capture

On macOS, call `initialize_webcams` before discovery and continue from its
callback after permission resolves.

```rust,no_run
use simple_video_capture::{WebcamCaptureConfig, webcams};

let webcam = webcams()?.into_iter().next().expect("no webcam");
for format in webcam.formats()? {
    println!("{format:?}");
}

let mut capture = webcam.open(WebcamCaptureConfig::default())?;
loop {
    let frame = capture.next_frame()?;
    // Encode, preview, or publish frame.
}
# #[allow(unreachable_code)]
# Ok::<(), simple_video_capture::CaptureError>(())
```

## Screen capture

Permission helpers let UI check support and request access before opening
capture. Permission requests may show operating-system UI.

```rust,no_run
use simple_video_capture::{
    ScreenCaptureConfig, has_screen_capture_permission,
    request_screen_capture_permission, screen_capture_supported, screen_sources,
};

if !screen_capture_supported() {
    return;
}
if !has_screen_capture_permission() && !request_screen_capture_permission() {
    return;
}

let source = screen_sources().remove(0);
let mut capture = source.open(ScreenCaptureConfig::default())?;
loop {
    let frame = capture.next_frame()?;
    // Encode, preview, or publish frame.
}
# #[allow(unreachable_code)]
# Ok::<(), simple_video_capture::CaptureError>(())
```

On Linux, `scap` uses desktop portal and PipeWire. Target choice happens when
default screen source opens, so discovery returns portal-selection source rather
than enumerating displays and windows. macOS and Windows also return enumerated
display/window sources.

## Unified source picker

`sources()` combines webcams and screen sources. `CaptureSource::open()` uses
backend defaults and returns unified `Capture`; `Capture::next_frame()` works for
either source.

For low-latency async delivery, create `frame_channel()` and run
`capture_to_channel()` on dedicated thread. Producer replaces stale unseen
frames instead of accumulating latency. Consumer's `pop()` method can be awaited
by UI or messenger code.

```rust,no_run
use simple_video_capture::{
    WebcamCaptureConfig, capture_to_channel, frame_channel, webcams,
};

let source = webcams()?.into_iter().next().expect("no webcam");
let (producer, mut consumer) = frame_channel();

std::thread::spawn(move || {
    let capture = source.open(WebcamCaptureConfig::default())?;
    capture_to_channel(capture, producer)
});

# futures::executor::block_on(async move {
while let Some(frame) = consumer.pop().await {
    // Preview or publish newest frame.
}
# });
# Ok::<(), simple_video_capture::CaptureError>(())
```
