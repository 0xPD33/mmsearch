//! Voice query capture: the default input device, mixed to mono, resampled to 16 kHz.

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const TARGET_RATE: u32 = 16_000;
pub const MAX_SECONDS: u32 = 30;

pub struct Recorder {
    _stream: cpal::Stream,
    samples: Arc<Mutex<Vec<f32>>>,
    level: Arc<AtomicU32>,
    rate: u32,
    started: Instant,
}

fn input_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    samples: Arc<Mutex<Vec<f32>>>,
    level: Arc<AtomicU32>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let cap = (config.sample_rate.0 * MAX_SECONDS) as usize;
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _| {
            let mono: Vec<f32> = data
                .chunks(channels)
                .map(|frame| {
                    frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>() / channels as f32
                })
                .collect();
            let rms = (mono.iter().map(|s| s * s).sum::<f32>() / mono.len().max(1) as f32).sqrt();
            level.store(rms.to_bits(), Ordering::Relaxed);
            let mut buf = samples.lock().unwrap();
            let room = cap.saturating_sub(buf.len());
            buf.extend(mono.into_iter().take(room));
        },
        |e| tracing::warn!("audio input: {e}"),
        None,
    )?;
    Ok(stream)
}

impl Recorder {
    pub fn start() -> Result<Recorder> {
        let device = cpal::default_host()
            .default_input_device()
            .ok_or_else(|| anyhow!("no input device"))?;
        let supported = device.default_input_config()?;
        let config = supported.config();
        let samples = Arc::new(Mutex::new(Vec::new()));
        let level = Arc::new(AtomicU32::new(0));
        let (s, l) = (samples.clone(), level.clone());
        let stream = match supported.sample_format() {
            SampleFormat::F32 => input_stream::<f32>(&device, &config, s, l)?,
            SampleFormat::I16 => input_stream::<i16>(&device, &config, s, l)?,
            SampleFormat::U16 => input_stream::<u16>(&device, &config, s, l)?,
            SampleFormat::I32 => input_stream::<i32>(&device, &config, s, l)?,
            f => return Err(anyhow!("unsupported sample format {f:?}")),
        };
        stream.play()?;
        Ok(Recorder {
            _stream: stream,
            samples,
            level,
            rate: config.sample_rate.0,
            started: Instant::now(),
        })
    }

    /// The loudness of the latest buffer, from 0 to about 1.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    pub fn full(&self) -> bool {
        self.started.elapsed().as_secs() >= MAX_SECONDS as u64
    }

    /// Stops recording and returns 16 kHz mono samples.
    pub fn finish(self) -> Vec<f32> {
        let raw = std::mem::take(&mut *self.samples.lock().unwrap());
        // ponytail: linear interpolation without a low-pass filter; add one if speech matches badly.
        let step = self.rate as f64 / TARGET_RATE as f64;
        let n = (raw.len() as f64 / step) as usize;
        (0..n)
            .map(|i| {
                let pos = i as f64 * step;
                let i0 = pos as usize;
                let frac = (pos - i0 as f64) as f32;
                let a = raw[i0];
                let b = raw.get(i0 + 1).copied().unwrap_or(a);
                a + (b - a) * frac
            })
            .collect()
    }
}
