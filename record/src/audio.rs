use std::sync::{
    Arc, Mutex,
    mpsc::{self, Sender},
};

use cpal::{
    Sample, SampleFormat, SampleRate,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use crossbeam::atomic::AtomicCell;
use ringbuf::{
    StaticRb,
    traits::{Consumer as _, Split},
    wrap::caching::Caching,
};
use tracing::info;

// ===============================
// struct Producer<const N: usize> {
//     producer: Caching<
//         ringbuf::Arc<
//             ringbuf::SharedRb<
//                 ringbuf::storage::Owning<[std::mem::MaybeUninit<AudioSampleType>; N]>,
//             >,
//         >,
//         true,
//         false,
//     >,
//     active: Arc<AtomicCell<bool>>,
// }
// impl<const N: usize> Drop for Producer<N> {
//     fn drop(&mut self) {
//         self.active.store(false);
//     }
// }
// struct Consumer<const N: usize> {
//     consumer: Caching<
//         ringbuf::Arc<
//             ringbuf::SharedRb<
//                 ringbuf::storage::Owning<[std::mem::MaybeUninit<AudioSampleType>; N]>,
//             >,
//         >,
//         false,
//         true,
//     >,
//     active: Arc<AtomicCell<bool>>,
// }
//
// struct Channel<const N: usize> {
//     channel_mode: u8,
//     sample_format: SampleFormat,
//     sample_rate: SampleRate,
//     effects: f32,
//     sample_consumer: Consumer<N>,
// }
// impl<const N: usize> Channel<N> {
//     fn new(
//         channel_mode: u8,
//         sample_format: SampleFormat,
//         sample_rate: SampleRate,
//     ) -> (Channel<N>, Producer<N>) {
//         let buffer = StaticRb::default();
//         let (producer, consumer) = buffer.split();
//
//         let active = Arc::new(AtomicCell::new(true));
//
//         (
//             Self {
//                 channel_mode,
//                 sample_format,
//                 sample_rate,
//                 effects: 1.0,
//                 sample_consumer: Consumer {
//                     active: active.clone(),
//                     consumer,
//                 },
//             },
//             Producer { producer, active },
//         )
//     }
// }
//
// // ==================================
//
// pub type AudioSampleType = f32;
// pub struct AudioMixer {
//     // Devices Selected
//     input_device: Option<cpal::Device>,
//     output_device: Option<cpal::Device>,
//     // Streams
//     output_stream: Option<cpal::Stream>,
//     // Mixer
//     channels: Arc<Mutex<Vec<Channel<5120>>>>,
//
//     // === Lagacy ===
//     output_sender: Sender<AudioSampleType>,
//     output_reciver: Arc<Mutex<mpsc::Receiver<AudioSampleType>>>,
// }
//
// impl AudioMixer {
//     pub fn new() -> Self {
//         let (output_sender, output_reciver) = mpsc::channel();
//
//         let mut audio_mixer = AudioMixer {
//             output_sender,
//             output_reciver: Arc::new(Mutex::new(output_reciver)),
//             input_device: Default::default(),
//             output_device: Default::default(),
//             output_stream: Default::default(),
//             channels: Arc::new(Mutex::new(Vec::new())),
//         };
//
//         let host = cpal::default_host();
//         if let Some(output) = host.default_output_device() {
//             audio_mixer.output_device = Some(output);
//         }
//         if let Some(input) = host.default_input_device() {
//             audio_mixer.input_device = Some(input);
//         }
//
//         audio_mixer
//     }
//     fn channel<S: Sample>(
//         &self,
//         channel_mode: u8,
//         sample_format: SampleFormat,
//         sample_rate: SampleRate,
//     ) -> Producer<5120> {
//         let (channel, producer) = Channel::new(channel_mode, sample_format, sample_rate);
//         self.channels.lock().unwrap().push(channel);
//
//         if let None = self.output_stream
//             && let Some(output) = &self.output_device
//         {
//             let config = output.default_output_config().unwrap();
//             let config = config.config();
//
//             let channels = self.channels.clone();
//             let stream = output
//                 .build_output_stream(
//                     &config,
//                     move |data: &mut [AudioSampleType], _| {
//                         let locekd = channels.lock().unwrap();
//                         println!("{:?}", locekd.len());
//                         // println!("{data:?}");
//                     },
//                     move |err| {
//                         eprintln!("{err:?}");
//                     },
//                     None,
//                 )
//                 .unwrap();
//
//             stream.play().unwrap();
//         };
//
//         producer
//     }
//     // pub fn get_sender(&self) -> Sender<AudioSampleType> {
//     //     self.output_sender.clone()
//     // }
// }
