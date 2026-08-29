use std::{
    any::Any,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use futures::{Stream, channel::oneshot, future::poll_fn, task::AtomicWaker};

use crate::{
    FrameConsumer, FrameProducer, FrameScaler, VideoFrame, VideoFrameTiming, frame_channel,
    timing::elapsed_micros,
};

mod error;
mod screen;
mod webcam;

pub use error::{CaptureError, CaptureResult};
pub use screen::{
    CropArea, ScreenCapture, ScreenCaptureConfig, ScreenResolution, ScreenSource, ScreenSourceKind,
    has_screen_capture_permission, request_screen_capture_permission, screen_capture_supported,
    screen_sources,
};
pub use webcam::{
    WebcamCapture, WebcamCaptureConfig, WebcamFormat, WebcamFormatPreference, WebcamId,
    WebcamPixelFormat, WebcamSource, has_webcam_permission, initialize_webcams, webcams,
};

/// Upper bounds applied to realtime capture before frames reach an encoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureLimits {
    pub max_width: u32,
    pub max_height: u32,
    pub max_frames_per_second: u32,
}

impl CaptureLimits {
    /// Build validated realtime capture limits.
    pub fn new(max_width: u32, max_height: u32, max_frames_per_second: u32) -> CaptureResult<Self> {
        let limits = Self {
            max_width,
            max_height,
            max_frames_per_second,
        };
        limits.validate()?;
        Ok(limits)
    }

    /// Validate that every bound is non-zero.
    pub fn validate(self) -> CaptureResult<()> {
        if self.max_width == 0 || self.max_height == 0 || self.max_frames_per_second == 0 {
            return Err(CaptureError::InvalidConfiguration(
                "capture limits must be greater than zero",
            ));
        }
        Ok(())
    }
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            max_width: 1280,
            max_height: 720,
            max_frames_per_second: 30,
        }
    }
}

/// Blocking source of decoded video frames.
pub trait FrameCapture {
    fn next_frame(&mut self) -> CaptureResult<VideoFrame>;

    /// Capture next frame while bounding dimensions.
    ///
    /// Default implementation captures then scales. Backends may override this
    /// to fuse native pixel conversion and scaling without processing pixels
    /// which will be discarded.
    fn next_frame_bounded(&mut self, max_width: u32, max_height: u32) -> CaptureResult<VideoFrame> {
        self.next_frame_bounded_with(&mut FrameScaler::new(), max_width, max_height)
    }

    /// Stateful form for capture loops. Default software path reuses scaling
    /// allocations; native backends may override to fuse conversion/scaling.
    fn next_frame_bounded_with(
        &mut self,
        scaler: &mut FrameScaler,
        max_width: u32,
        max_height: u32,
    ) -> CaptureResult<VideoFrame> {
        let fallback_timing = tracing::enabled!(target: "video_timing", tracing::Level::TRACE)
            .then(VideoFrameTiming::start);
        let frame = self.next_frame()?;
        let timing = frame.timing().or(fallback_timing);
        Ok(scaler.scale_to_fit(frame.with_pipeline_timing(timing), max_width, max_height)?)
    }
}

/// Capture frames until consuming side of `output` closes or capture fails.
///
/// Function blocks. Run it on a dedicated thread when UI or async tasks consume
/// receiving half of [`crate::frame_channel`]. Some native capture
/// handles are not [`Send`], so open capture inside that thread.
pub fn capture_to_channel(
    mut capture: impl FrameCapture,
    mut output: FrameProducer,
) -> CaptureResult<()> {
    while !output.is_closed() {
        let fallback_timing = tracing::enabled!(target: "video_timing", tracing::Level::TRACE)
            .then(VideoFrameTiming::start);
        let frame = capture.next_frame()?;
        let timing = frame.timing().or(fallback_timing);
        let frame = frame.with_pipeline_timing(timing);
        if !output.push(frame) {
            break;
        }
    }
    Ok(())
}

/// Capture frames with software dimension and publish-rate bounds until
/// `output` closes.
///
/// Software scaling is required as a fallback because some native backends,
/// notably PipeWire through `scap`, can return native resolution despite an
/// output-resolution request. Frames arriving above the configured rate are
/// consumed but not published, so backend queues continue draining.
pub fn capture_to_channel_bounded(
    capture: impl FrameCapture,
    output: FrameProducer,
    limits: CaptureLimits,
) -> CaptureResult<()> {
    capture_to_channel_bounded_until(capture, output, limits, || false)
}

fn capture_to_channel_bounded_until(
    mut capture: impl FrameCapture,
    mut output: FrameProducer,
    limits: CaptureLimits,
    should_stop: impl Fn() -> bool,
) -> CaptureResult<()> {
    limits.validate()?;
    let mut scaler = FrameScaler::new();
    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(limits.max_frames_per_second));
    let mut next_publish = None;
    while !should_stop() && !output.is_closed() {
        let timing_enabled = tracing::enabled!(target: "video_timing", tracing::Level::TRACE);
        let total_started = timing_enabled.then(Instant::now);
        let capture_started = timing_enabled.then(Instant::now);
        let frame =
            capture.next_frame_bounded_with(&mut scaler, limits.max_width, limits.max_height)?;
        if should_stop() {
            break;
        }
        let captured_at = Instant::now();
        if !claim_publish_slot(&mut next_publish, captured_at, frame_interval) {
            continue;
        }
        let capture_us = capture_started.map(elapsed_micros).unwrap_or_default();
        let output_width = frame.width;
        let output_height = frame.height;
        let output_bytes = frame.pixel_data.as_bytes().len();
        let frame_timing = frame.timing();
        let publish_started = timing_enabled.then(Instant::now);
        let published = output.push(frame);
        let publish_us = publish_started.map(elapsed_micros).unwrap_or_default();
        if let Some(total_started) = total_started {
            tracing::trace!(
                target: "video_timing",
                stage = "capture.bounded",
                frame_id = frame_timing.map_or(0, |timing| timing.id()),
                pipeline_elapsed_us = frame_timing.map_or(0, |timing| timing.elapsed_micros()),
                elapsed_us = elapsed_micros(total_started),
                capture_us,
                publish_us,
                output_width,
                output_height,
                output_bytes,
                max_width = limits.max_width,
                max_height = limits.max_height,
                max_frames_per_second = limits.max_frames_per_second,
                published,
                "video stage complete"
            );
        }
        if !published {
            break;
        }
    }
    Ok(())
}

fn claim_publish_slot(
    next_publish: &mut Option<Instant>,
    captured_at: Instant,
    frame_interval: Duration,
) -> bool {
    if next_publish.is_some_and(|deadline| captured_at < deadline) {
        return false;
    }
    *next_publish = Some(captured_at + frame_interval);
    true
}

/// Any discoverable video input source.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum CaptureSource {
    Webcam(WebcamSource),
    Screen(ScreenSource),
}

/// Broad category of a discoverable video input.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum CaptureSourceKind {
    Webcam,
    Screen,
}

impl CaptureSource {
    /// Source category suitable for grouping a device picker.
    pub fn kind(&self) -> CaptureSourceKind {
        match self {
            Self::Webcam(_) => CaptureSourceKind::Webcam,
            Self::Screen(_) => CaptureSourceKind::Screen,
        }
    }

    /// Human-readable source name suitable for a picker.
    pub fn name(&self) -> &str {
        match self {
            Self::Webcam(source) => source.name(),
            Self::Screen(source) => source.title(),
        }
    }

    /// Open source with backend's default capture configuration.
    pub fn open(&self) -> CaptureResult<Capture> {
        match self {
            Self::Webcam(source) => source
                .open(WebcamCaptureConfig::default())
                .map(Capture::Webcam),
            Self::Screen(source) => source
                .open(ScreenCaptureConfig::default())
                .map(|capture| Capture::Screen(Box::new(capture))),
        }
    }

    /// Open source with device-side realtime bounds where supported.
    ///
    /// Use [`capture_to_channel_bounded`] as well: Linux screen capture can
    /// negotiate native PipeWire resolution despite this request.
    pub fn open_bounded(&self, limits: CaptureLimits) -> CaptureResult<Capture> {
        limits.validate()?;
        match self {
            Self::Webcam(source) => source
                .open_bounded(
                    limits.max_width,
                    limits.max_height,
                    limits.max_frames_per_second,
                )
                .map(Capture::Webcam),
            Self::Screen(source) => source
                .open(ScreenCaptureConfig {
                    frames_per_second: limits.max_frames_per_second,
                    resolution: screen_resolution(limits.max_height),
                    ..ScreenCaptureConfig::default()
                })
                .map(|capture| Capture::Screen(Box::new(capture))),
        }
    }

    /// Start a latest-frame stream using [`CaptureLimits::default`].
    ///
    /// Device construction and blocking reads run on a dedicated worker. The
    /// returned future resolves only after the native device opens.
    ///
    /// # Errors
    ///
    /// Returns worker-spawn or native device-open failures.
    pub async fn stream(&self) -> CaptureResult<CaptureStream> {
        self.stream_with_limits(CaptureLimits::default()).await
    }

    /// Start a latest-frame stream bounded by `limits`.
    ///
    /// # Errors
    ///
    /// Returns configuration, worker-spawn, and native device-open failures.
    pub async fn stream_with_limits(&self, limits: CaptureLimits) -> CaptureResult<CaptureStream> {
        limits.validate()?;
        let source = self.clone();
        spawn_capture_worker(move || source.open_bounded(limits), limits).await
    }
}

fn screen_resolution(max_height: u32) -> ScreenResolution {
    match max_height {
        0..=480 => ScreenResolution::P480,
        481..=720 => ScreenResolution::P720,
        721..=1080 => ScreenResolution::P1080,
        1081..=1440 => ScreenResolution::P1440,
        1441..=2160 => ScreenResolution::P2160,
        _ => ScreenResolution::P4320,
    }
}

/// Discover available webcams followed by displays and windows.
///
/// A webcam backend failure is logged but does not hide valid screen sources.
/// Use [`try_capture_sources`] when discovery errors must be returned.
pub fn capture_sources() -> Vec<CaptureSource> {
    let mut sources = match webcams() {
        Ok(webcams) => webcams
            .into_iter()
            .map(CaptureSource::Webcam)
            .collect::<Vec<_>>(),
        Err(error) => {
            tracing::warn!(%error, "webcam discovery failed; returning screen sources only");
            Vec::new()
        }
    };
    if screen_capture_supported() {
        sources.extend(screen_sources().into_iter().map(CaptureSource::Screen));
    }
    sources
}

/// Discover every source, failing when webcam discovery fails.
///
/// # Errors
///
/// Returns [`CaptureError::Webcam`] when webcam discovery fails.
pub fn try_capture_sources() -> CaptureResult<Vec<CaptureSource>> {
    let mut sources = webcams()?
        .into_iter()
        .map(CaptureSource::Webcam)
        .collect::<Vec<_>>();
    if screen_capture_supported() {
        sources.extend(screen_sources().into_iter().map(CaptureSource::Screen));
    }
    Ok(sources)
}

/// Compatibility name for [`try_capture_sources`].
pub fn sources() -> CaptureResult<Vec<CaptureSource>> {
    try_capture_sources()
}

enum WorkerStatus {
    Running,
    Finished(Option<CaptureResult<()>>),
}

struct WorkerShared {
    stop_requested: AtomicBool,
    status: Mutex<WorkerStatus>,
    completion_waker: AtomicWaker,
}

impl WorkerShared {
    fn new() -> Self {
        Self {
            stop_requested: AtomicBool::new(false),
            status: Mutex::new(WorkerStatus::Running),
            completion_waker: AtomicWaker::new(),
        }
    }

    fn status(&self) -> MutexGuard<'_, WorkerStatus> {
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
    }

    fn should_stop(&self) -> bool {
        self.stop_requested.load(Ordering::Acquire)
    }

    fn finish(&self, result: CaptureResult<()>) {
        *self.status() = WorkerStatus::Finished(Some(result));
        self.completion_waker.wake();
    }

    fn is_finished(&self) -> bool {
        matches!(*self.status(), WorkerStatus::Finished(_))
    }

    fn poll_result(&self, cx: &mut Context<'_>) -> Poll<CaptureResult<()>> {
        let mut status = self.status();
        match &mut *status {
            WorkerStatus::Running => {
                self.completion_waker.register(cx.waker());
                Poll::Pending
            }
            WorkerStatus::Finished(result) => Poll::Ready(result.take().unwrap_or(Ok(()))),
        }
    }
}

/// Completion and cooperative-stop handle for a capture worker.
#[must_use = "dropping completion stops capture without reporting terminal errors"]
pub struct CaptureCompletion {
    shared: Arc<WorkerShared>,
    worker: Option<JoinHandle<()>>,
}

impl CaptureCompletion {
    /// Ask worker to stop after current blocking device read returns.
    pub fn request_stop(&self) {
        self.shared.request_stop();
    }

    /// Whether worker has returned or panicked.
    pub fn is_finished(&self) -> bool {
        self.shared.is_finished()
    }

    /// Await worker completion and join its native thread.
    ///
    /// Consumes handle because terminal error can be observed only once.
    /// Native reads are not always interruptible, so this future can remain
    /// pending when a backend does not return.
    ///
    /// # Errors
    ///
    /// Returns runtime capture failures or [`CaptureError::WorkerPanicked`].
    pub async fn wait(mut self) -> CaptureResult<()> {
        let result = poll_fn(|cx| self.shared.poll_result(cx)).await;
        let join_result = self.worker.take().map(JoinHandle::join);
        if let Some(Err(payload)) = join_result {
            return Err(CaptureError::WorkerPanicked(panic_message(payload)));
        }
        result
    }
}

impl Drop for CaptureCompletion {
    fn drop(&mut self) {
        self.request_stop();
    }
}

/// Async latest-frame view over a dedicated blocking capture worker.
#[must_use = "dropping the stream requests capture shutdown"]
pub struct CaptureStream {
    frames: Option<FrameConsumer>,
    completion: Option<CaptureCompletion>,
    ended: bool,
}

impl CaptureStream {
    fn new(frames: FrameConsumer, completion: CaptureCompletion) -> Self {
        Self {
            frames: Some(frames),
            completion: Some(completion),
            ended: false,
        }
    }

    /// Await next frame, terminal capture error, or clean stream end.
    ///
    /// # Errors
    ///
    /// Returns runtime capture failure once, after any buffered final frame.
    pub async fn next_frame(&mut self) -> CaptureResult<Option<VideoFrame>> {
        match poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await {
            Some(result) => result.map(Some),
            None => Ok(None),
        }
    }

    /// Ask worker to stop after current blocking device read returns.
    pub fn request_stop(&self) {
        if let Some(completion) = &self.completion {
            completion.request_stop();
        }
    }

    /// Request stop, close frame consumer, and await worker completion.
    ///
    /// Native reads are not always interruptible. This future remains pending
    /// if a backend never returns; drop the stream instead when caller must not
    /// wait for native shutdown.
    ///
    /// # Errors
    ///
    /// Returns runtime capture failures or [`CaptureError::WorkerPanicked`].
    pub async fn shutdown(mut self) -> CaptureResult<()> {
        self.request_stop();
        drop(self.frames.take());
        let Some(completion) = self.completion.take() else {
            return Ok(());
        };
        completion.wait().await
    }

    /// Split stream into raw latest-frame receiver and completion handle.
    ///
    /// Useful when an application API already accepts [`FrameConsumer`].
    pub fn into_parts(mut self) -> (FrameConsumer, CaptureCompletion) {
        let frames = self
            .frames
            .take()
            .expect("capture stream always owns frame consumer before splitting");
        let completion = self
            .completion
            .take()
            .expect("capture stream always owns completion before splitting");
        (frames, completion)
    }
}

impl Stream for CaptureStream {
    type Item = CaptureResult<VideoFrame>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.ended {
            return Poll::Ready(None);
        }

        let Some(frames) = &mut this.frames else {
            this.ended = true;
            return Poll::Ready(None);
        };
        match frames.poll_pop(cx) {
            Poll::Ready(Some(frame)) => Poll::Ready(Some(Ok(frame))),
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                let Some(completion) = &this.completion else {
                    this.ended = true;
                    return Poll::Ready(None);
                };
                match completion.shared.poll_result(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Ok(())) => {
                        this.ended = true;
                        Poll::Ready(None)
                    }
                    Poll::Ready(Err(error)) => {
                        this.ended = true;
                        Poll::Ready(Some(Err(error)))
                    }
                }
            }
        }
    }
}

impl Drop for CaptureStream {
    fn drop(&mut self) {
        self.request_stop();
    }
}

async fn spawn_capture_worker<C, F>(
    factory: F,
    limits: CaptureLimits,
) -> CaptureResult<CaptureStream>
where
    C: FrameCapture,
    F: FnOnce() -> CaptureResult<C> + Send + 'static,
{
    let (output, frames) = frame_channel();
    let (startup_sender, startup_receiver) = oneshot::channel();
    let shared = Arc::new(WorkerShared::new());
    let worker_shared = shared.clone();
    let worker = thread::Builder::new()
        .name("simple-video-stream".to_owned())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                run_capture_worker(factory, output, limits, startup_sender, &worker_shared)
            }))
            .unwrap_or_else(|payload| Err(CaptureError::WorkerPanicked(panic_message(payload))));
            worker_shared.finish(result);
        })
        .map_err(CaptureError::WorkerSpawn)?;
    let completion = CaptureCompletion {
        shared,
        worker: Some(worker),
    };

    match startup_receiver.await {
        Ok(Ok(())) => Ok(CaptureStream::new(frames, completion)),
        Ok(Err(error)) => Err(error),
        Err(_) => match completion.wait().await {
            Err(error) => Err(error),
            Ok(()) => Err(CaptureError::WorkerPanicked(
                "worker ended before startup handshake".to_owned(),
            )),
        },
    }
}

fn run_capture_worker<C, F>(
    factory: F,
    output: FrameProducer,
    limits: CaptureLimits,
    startup: oneshot::Sender<CaptureResult<()>>,
    shared: &WorkerShared,
) -> CaptureResult<()>
where
    C: FrameCapture,
    F: FnOnce() -> CaptureResult<C>,
{
    let capture = match factory() {
        Ok(capture) => capture,
        Err(error) => {
            if let Err(Err(error)) = startup.send(Err(error)) {
                return Err(error);
            }
            return Ok(());
        }
    };
    if startup.send(Ok(())).is_err() {
        return Ok(());
    }

    capture_to_channel_bounded_until(capture, output, limits, || shared.should_stop())
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else {
        "unknown panic".to_owned()
    }
}

/// Any active webcam or screen capture stream.
#[non_exhaustive]
pub enum Capture {
    Webcam(WebcamCapture),
    /// Screen backends hold substantially more native state than webcams.
    /// Indirection keeps this sum type cheap to move through capture tasks.
    Screen(Box<ScreenCapture>),
}

impl Capture {
    /// Capture next decoded frame. Blocks until frame arrives.
    pub fn next_frame(&mut self) -> CaptureResult<VideoFrame> {
        match self {
            Self::Webcam(capture) => capture.next_frame(),
            Self::Screen(capture) => capture.next_frame(),
        }
    }
}

impl FrameCapture for Capture {
    fn next_frame(&mut self) -> CaptureResult<VideoFrame> {
        Self::next_frame(self)
    }

    fn next_frame_bounded(&mut self, max_width: u32, max_height: u32) -> CaptureResult<VideoFrame> {
        match self {
            Self::Webcam(capture) => capture.next_frame_bounded(max_width, max_height),
            Self::Screen(capture) => capture.next_frame_bounded(max_width, max_height),
        }
    }

    fn next_frame_bounded_with(
        &mut self,
        scaler: &mut FrameScaler,
        max_width: u32,
        max_height: u32,
    ) -> CaptureResult<VideoFrame> {
        match self {
            Self::Webcam(capture) => {
                Ok(scaler.scale_to_fit(capture.next_frame()?, max_width, max_height)?)
            }
            // ScreenCapture owns a fused converter/scaler with equivalent
            // reusable state; a second software pass would waste work.
            Self::Screen(capture) => capture.next_frame_bounded(max_width, max_height),
        }
    }
}

#[cfg(feature = "bench-internals")]
pub(crate) fn packed_4_to_rgb_scalar(source: &[u8], output: &mut Vec<u8>, channels: [usize; 3]) {
    screen::packed_4_to_rgb_scalar(source, output, channels);
}

#[cfg(feature = "bench-internals")]
pub(crate) fn packed_4_to_rgb_simd(source: &[u8], output: &mut Vec<u8>, channels: [usize; 3]) {
    screen::packed_4_to_rgb_simd(source, output, channels);
}

#[cfg(feature = "bench-internals")]
pub(crate) fn bgr_to_rgb_scalar(pixels: &mut [u8]) {
    screen::bgr_to_rgb_scalar(pixels);
}

#[cfg(feature = "bench-internals")]
pub(crate) fn bgr_to_rgb_simd(pixels: &mut [u8]) {
    screen::bgr_to_rgb_simd(pixels);
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;
    use crate::PixelData;

    struct NonSendCapture {
        frames_remaining: usize,
        terminal_error: bool,
        _not_send: Rc<()>,
    }

    impl FrameCapture for NonSendCapture {
        fn next_frame(&mut self) -> CaptureResult<VideoFrame> {
            if self.frames_remaining > 0 {
                self.frames_remaining -= 1;
                return Ok(VideoFrame::new(1, 1, PixelData::Rgb8(vec![1, 2, 3])));
            }
            if self.terminal_error {
                return Err(CaptureError::InvalidConfiguration("fake terminal error"));
            }
            std::thread::yield_now();
            Ok(VideoFrame::new(1, 1, PixelData::Rgb8(vec![4, 5, 6])))
        }
    }

    fn non_send_capture(terminal_error: bool) -> NonSendCapture {
        NonSendCapture {
            frames_remaining: 1,
            terminal_error,
            _not_send: Rc::new(()),
        }
    }

    #[test]
    fn worker_constructs_and_owns_non_send_capture() {
        futures::executor::block_on(async {
            let mut stream =
                spawn_capture_worker(move || Ok(non_send_capture(true)), CaptureLimits::default())
                    .await
                    .expect("fake capture should open");

            let frame = stream
                .next_frame()
                .await
                .expect("first frame should succeed")
                .expect("first frame should exist");
            assert_eq!(frame.pixel_data, PixelData::Rgb8(vec![1, 2, 3]));

            assert!(matches!(
                stream.next_frame().await,
                Err(CaptureError::InvalidConfiguration("fake terminal error"))
            ));
            assert!(stream.next_frame().await.unwrap().is_none());
        });
    }

    #[test]
    fn startup_handshake_returns_open_error() {
        futures::executor::block_on(async {
            let result = spawn_capture_worker::<NonSendCapture, _>(
                move || {
                    Err(CaptureError::InvalidConfiguration(
                        "fake source could not open",
                    ))
                },
                CaptureLimits::default(),
            )
            .await;
            assert!(matches!(
                result,
                Err(CaptureError::InvalidConfiguration(
                    "fake source could not open"
                ))
            ));
        });
    }

    #[test]
    fn into_parts_preserves_terminal_error_for_completion() {
        futures::executor::block_on(async {
            let stream =
                spawn_capture_worker(move || Ok(non_send_capture(true)), CaptureLimits::default())
                    .await
                    .expect("fake capture should open");
            let (mut frames, completion) = stream.into_parts();

            let frame = frames.pop().await.expect("published frame should exist");
            assert_eq!(frame.pixel_data, PixelData::Rgb8(vec![1, 2, 3]));
            assert!(frames.pop().await.is_none());
            assert!(completion.is_finished());
            assert!(matches!(
                completion.wait().await,
                Err(CaptureError::InvalidConfiguration("fake terminal error"))
            ));
        });
    }

    #[test]
    fn worker_panic_becomes_terminal_error() {
        futures::executor::block_on(async {
            struct PanickingCapture;

            impl FrameCapture for PanickingCapture {
                fn next_frame(&mut self) -> CaptureResult<VideoFrame> {
                    panic!("fake capture panic");
                }
            }

            let mut stream =
                spawn_capture_worker(move || Ok(PanickingCapture), CaptureLimits::default())
                    .await
                    .expect("fake capture should open");
            assert!(matches!(
                stream.next_frame().await,
                Err(CaptureError::WorkerPanicked(message)) if message == "fake capture panic"
            ));
        });
    }

    #[test]
    fn source_descriptors_and_stream_are_send() {
        fn assert_send<T: Send>() {}
        fn assert_stream<T: Stream<Item = CaptureResult<VideoFrame>>>() {}

        assert_send::<CaptureSource>();
        assert_send::<CaptureStream>();
        assert_stream::<CaptureStream>();
    }

    #[test]
    fn capture_limits_constructor_rejects_zero_values() {
        assert!(CaptureLimits::new(1280, 720, 30).is_ok());
        assert!(matches!(
            CaptureLimits::new(0, 720, 30),
            Err(CaptureError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn publish_cadence_drops_early_frames() {
        let start = Instant::now();
        let interval = Duration::from_millis(40);
        let mut next_publish = None;

        assert!(claim_publish_slot(&mut next_publish, start, interval));
        assert!(!claim_publish_slot(
            &mut next_publish,
            start + Duration::from_millis(39),
            interval
        ));
        assert!(claim_publish_slot(
            &mut next_publish,
            start + interval,
            interval
        ));
    }
}
