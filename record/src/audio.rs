use std::sync::{
    Arc, Mutex,
    mpsc::{self, Sender},
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::{
    StaticRb,
    traits::{Consumer, RingBuffer},
};
use tracing::info;

pub type AudioSampleType = i16;
pub(super) struct AudioControl {
    output_sender: Sender<AudioSampleType>,
    output_reciver: Arc<Mutex<mpsc::Receiver<AudioSampleType>>>,
    input_device: Option<cpal::Device>,
    output_stream: Option<cpal::Stream>,
}

impl AudioControl {
    pub fn new() -> Self {
        let (output_sender, output_reciver) = mpsc::channel();

        let mut audio_settings = AudioControl {
            output_sender,
            output_reciver: Arc::new(Mutex::new(output_reciver)),
            input_device: Default::default(),
            output_stream: Default::default(),
        };

        let host = cpal::default_host();
        if let Some(output) = host.default_output_device() {
            let rx = audio_settings.output_reciver.clone();

            let config = output.default_output_config().unwrap();
            let mut config = config.config();
            // TODO: Remove those
            config.sample_rate = cpal::SampleRate(48000);
            config.channels = 2;
            config.buffer_size = cpal::BufferSize::Default;
            info!("{config:?}");

            // let mut rb = StaticRb::<i16, 2560>::default();

            let a = output
                .build_output_stream(
                    &config,
                    move |data: &mut [AudioSampleType], _| {
                        let rx = rx.lock().unwrap();
                        // println!("{data:?}");

                        for (i, sample) in data.iter_mut().enumerate() {
                            *sample = rx.try_recv().unwrap_or(0);
                        }
                    },
                    move |err| {
                        eprintln!("{err:?}");
                    },
                    None,
                )
                .unwrap();

            a.play().unwrap();
            audio_settings.output_stream = Some(a);
        }
        if let Some(input) = host.default_input_device() {
            audio_settings.input_device = Some(input);
        }

        audio_settings
    }
    pub fn get_sender(&self) -> Sender<AudioSampleType> {
        self.output_sender.clone()
    }
}
