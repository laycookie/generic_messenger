//! Hosting [fundsp] graphs over the crate's interleaved buffers.
//!
//! Everything fundsp-shaped funnels through one bridge: [`GraphHost`] runs
//! any unit whose input and output arity match the stream's channel count,
//! staging interleaved audio through planar blocks. Rack effects host a
//! per-channel [`Net`] built from a mono prototype.

use fundsp::{MAX_BUFFER_SIZE, audiounit::AudioUnit, buffer::BufferVec, net::Net};

use crate::{AudioSampleType, StreamFormat};

/// A fundsp unit applied to interleaved audio of a matching channel count.
///
/// The planar staging is preallocated, so `process` neither allocates nor
/// locks — it is safe on the real-time callback as long as the hosted unit
/// is (a committed [`Net`] backend is; rack effects run control-side anyway).
pub(crate) struct GraphHost {
    unit: Box<dyn AudioUnit>,
    input: BufferVec,
    output: BufferVec,
    channels: usize,
}

impl GraphHost {
    /// Host `unit`, whose inputs and outputs must both number `channels`.
    pub(crate) fn new(mut unit: Box<dyn AudioUnit>, channels: usize) -> Self {
        debug_assert_eq!(
            (unit.inputs(), unit.outputs()),
            (channels, channels),
            "hosted graph must match the stream's channel count",
        );
        unit.allocate();
        GraphHost {
            unit,
            input: BufferVec::new(channels),
            output: BufferVec::new(channels),
            channels,
        }
    }

    /// Host a mono (1-in/1-out) prototype applied independently to every
    /// channel of a stream: one clone per channel, so filter state never
    /// leaks between channels. [`fundsp::prelude::Shared`] controls baked
    /// into the prototype survive the cloning — one atomic write retunes
    /// every channel.
    pub(crate) fn per_channel(prototype: Box<dyn AudioUnit>, format: StreamFormat) -> Self {
        debug_assert_eq!(
            (prototype.inputs(), prototype.outputs()),
            (1, 1),
            "per-channel hosting needs a mono prototype",
        );
        let channels = (format.channels as usize).max(1);
        let mut net = Net::new(channels, channels);
        for channel in 0..channels {
            let id = net.push(prototype.clone());
            net.connect_input(channel, id, 0);
            net.connect_output(id, 0, channel);
        }
        net.set_sample_rate(format.sample_rate as f64);
        Self::new(Box::new(net), channels)
    }

    /// Run the graph over an interleaved buffer of whole frames, in place.
    pub(crate) fn process(&mut self, interleaved: &mut [AudioSampleType]) {
        let channels = self.channels;
        let frames = interleaved.len() / channels;
        let mut done = 0;
        while done < frames {
            let block = (frames - done).min(MAX_BUFFER_SIZE);
            for frame in 0..block {
                let at = (done + frame) * channels;
                for channel in 0..channels {
                    self.input
                        .set_f32(channel, frame, interleaved[at + channel]);
                }
            }
            self.unit.process(
                block,
                &self.input.buffer_ref(),
                &mut self.output.buffer_mut(),
            );
            for frame in 0..block {
                let at = (done + frame) * channels;
                for channel in 0..channels {
                    interleaved[at + channel] = self.output.at_f32(channel, frame);
                }
            }
            done += block;
        }
    }
}
