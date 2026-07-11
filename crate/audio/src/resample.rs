//! Conversion between an application's declared stream format and the audio
//! device's preferred format: channel-count remapping plus linear-interpolation
//! resampling. Resamplers run on the control side (inside the producer/consumer
//! handles), never on the real-time audio thread.

use crate::AudioSampleType;

/// Streaming format resampler that pulls source frames on demand.
///
/// State (the interpolation window) persists across `pull` calls, so a single
/// continuous audio stream can be resampled in arbitrarily sized pieces.
pub(crate) struct Resampler {
    src_channels: usize,
    dst_channels: usize,
    /// Source frames advanced per destination frame (`src_rate / dst_rate`).
    step: f64,
    /// Interpolation position between `a` and `b`, in `[0, 1)`.
    phase: f64,
    /// Frames currently loaded into the interpolation window (0..=2).
    loaded: u8,
    a: Box<[AudioSampleType]>,
    b: Box<[AudioSampleType]>,
    interp: Box<[AudioSampleType]>,
}

impl Resampler {
    pub(crate) fn new(
        src_channels: usize,
        src_rate: u32,
        dst_channels: usize,
        dst_rate: u32,
    ) -> Self {
        debug_assert!(src_channels > 0 && dst_channels > 0 && src_rate > 0 && dst_rate > 0);
        Resampler {
            src_channels,
            dst_channels,
            step: src_rate as f64 / dst_rate as f64,
            phase: 0.0,
            loaded: 0,
            a: vec![0.0; src_channels].into_boxed_slice(),
            b: vec![0.0; src_channels].into_boxed_slice(),
            interp: vec![0.0; src_channels].into_boxed_slice(),
        }
    }

    pub(crate) fn dst_channels(&self) -> usize {
        self.dst_channels
    }

    /// Fill `dst` with converted frames, pulling source frames through `next`.
    ///
    /// `next` writes one source frame (`src_channels` samples) into its
    /// argument and returns `false` when the source is exhausted. Returns the
    /// number of samples written to `dst` — always whole destination frames.
    pub(crate) fn pull(
        &mut self,
        mut next: impl FnMut(&mut [AudioSampleType]) -> bool,
        dst: &mut [AudioSampleType],
    ) -> usize {
        let dst_ch = self.dst_channels;
        let usable = dst.len() - dst.len() % dst_ch;
        let mut written = 0;

        if self.step == 1.0 {
            // Same rate: pure channel remap (or passthrough), no lookahead.
            while written + dst_ch <= usable {
                if self.src_channels == dst_ch {
                    if !next(&mut dst[written..written + dst_ch]) {
                        break;
                    }
                } else {
                    if !next(&mut self.a) {
                        break;
                    }
                    Self::map_frame(&self.a, &mut dst[written..written + dst_ch]);
                }
                written += dst_ch;
            }
            return written;
        }

        while written + dst_ch <= usable {
            // Keep the window [a, b] loaded with `phase` inside it.
            loop {
                if self.loaded == 0 {
                    if !next(&mut self.a) {
                        return written;
                    }
                    self.loaded = 1;
                }
                if self.loaded == 1 {
                    if !next(&mut self.b) {
                        return written;
                    }
                    self.loaded = 2;
                }
                if self.phase < 1.0 {
                    break;
                }
                core::mem::swap(&mut self.a, &mut self.b);
                self.loaded = 1;
                self.phase -= 1.0;
            }

            let t = self.phase as AudioSampleType;
            for ((sample, a), b) in self.interp.iter_mut().zip(&self.a).zip(&self.b) {
                *sample = a + (b - a) * t;
            }
            Self::map_frame(&self.interp, &mut dst[written..written + dst_ch]);
            written += dst_ch;
            self.phase += self.step;
        }
        written
    }

    /// Remap one frame between channel layouts: identity when equal, mono is
    /// duplicated to the front pair, stereo averages down to mono; otherwise
    /// channels map positionally with extras dropped or zero-filled.
    fn map_frame(src: &[AudioSampleType], dst: &mut [AudioSampleType]) {
        match (src.len(), dst.len()) {
            (s, d) if s == d => dst.copy_from_slice(src),
            (1, _) => {
                dst.fill(0.0);
                dst[0] = src[0];
                if dst.len() >= 2 {
                    dst[1] = src[0];
                }
            }
            (2, 1) => dst[0] = 0.5 * (src[0] + src[1]),
            (s, d) => {
                let shared = s.min(d);
                dst[..shared].copy_from_slice(&src[..shared]);
                dst[shared..].fill(0.0);
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/resample.rs"]
mod tests;
