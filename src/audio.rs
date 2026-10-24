use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use parking_lot::Mutex;

pub const OUTPUT_CHANNELS: usize = 2;
const MAX_BUFFER_MS: u32 = 1000;
const TEST_TONE_SECONDS: u32 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputDevice {
    pub id: String,
    pub name: String,
}

pub fn list_output_devices() -> Vec<OutputDevice> {
    let host = cpal::default_host();
    let Ok(devices) = host.output_devices() else {
        return Vec::new();
    };
    devices
        .filter_map(|device| {
            let id = device.id().ok()?.to_string();
            let name = device
                .description()
                .map(|d| d.name().to_string())
                .unwrap_or_else(|_| id.clone());
            Some(OutputDevice { id, name })
        })
        .collect()
}

pub fn default_device_name() -> Option<String> {
    let device = cpal::default_host().default_output_device()?;
    device.description().ok().map(|d| d.name().to_string())
}

/// Linear-interpolating stereo resampler. Simple and allocation-free per sample; adequate for
/// the 48 kHz → device-rate conversion needed when a device runs at 44.1/96 kHz.
#[derive(Debug)]
pub struct Resampler {
    in_rate: u32,
    out_rate: u32,
    position: f64,
    previous: [f32; OUTPUT_CHANNELS],
    primed: bool,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            in_rate,
            out_rate,
            position: 0.0,
            previous: [0.0; OUTPUT_CHANNELS],
            primed: false,
        }
    }

    pub fn rates(&self) -> (u32, u32) {
        (self.in_rate, self.out_rate)
    }

    pub fn process(&mut self, input: &[[f32; OUTPUT_CHANNELS]], out: &mut Vec<f32>) {
        if self.in_rate == self.out_rate {
            for frame in input {
                out.extend_from_slice(frame);
            }
            return;
        }
        let step = self.in_rate as f64 / self.out_rate as f64;
        for frame in input {
            if !self.primed {
                self.previous = *frame;
                self.primed = true;
                continue;
            }
            while self.position < 1.0 {
                let t = self.position as f32;
                for (previous, current) in self.previous.iter().zip(frame) {
                    out.push(previous + (current - previous) * t);
                }
                self.position += step;
            }
            self.position -= 1.0;
            self.previous = *frame;
        }
    }
}

/// Bounded queue of interleaved stereo samples at the device rate. The producer trims the queue
/// back to the latency target whenever it grows past target + slack, so a burst of network
/// packets after a stall never turns into permanent extra delay.
pub struct JitterBuffer {
    samples: VecDeque<f32>,
    target_frames: usize,
    slack_frames: usize,
    capacity_frames: usize,
    priming: bool,
}

impl JitterBuffer {
    pub fn new(rate: u32, target_ms: u32) -> Self {
        let mut buffer = Self {
            samples: VecDeque::new(),
            target_frames: 0,
            slack_frames: 0,
            capacity_frames: (rate * MAX_BUFFER_MS / 1000) as usize,
            priming: true,
        };
        buffer.configure(rate, target_ms);
        buffer
    }

    pub fn configure(&mut self, rate: u32, target_ms: u32) {
        self.target_frames = (rate as u64 * target_ms as u64 / 1000) as usize;
        self.slack_frames = (rate as u64 * (target_ms as u64 / 2 + 40) / 1000) as usize;
        self.capacity_frames = (rate * MAX_BUFFER_MS / 1000) as usize;
    }

    pub fn buffered_frames(&self) -> usize {
        self.samples.len() / OUTPUT_CHANNELS
    }

    pub fn push(&mut self, interleaved: &[f32]) -> usize {
        self.samples.extend(interleaved.iter().copied());
        let mut dropped = 0;
        let limit = (self.target_frames + self.slack_frames).min(self.capacity_frames);
        if self.buffered_frames() > limit {
            let excess = self.buffered_frames() - self.target_frames;
            self.samples.drain(..excess * OUTPUT_CHANNELS);
            dropped = excess;
        }
        dropped
    }

    /// Fills `out` (interleaved stereo); returns true when it had to insert silence.
    pub fn pop_into(&mut self, out: &mut [f32]) -> bool {
        if self.priming {
            if self.buffered_frames() < self.target_frames.max(1) {
                out.fill(0.0);
                return false;
            }
            self.priming = false;
        }
        let available = self.samples.len().min(out.len());
        for (dst, src) in out.iter_mut().zip(self.samples.drain(..available)) {
            *dst = src;
        }
        if available < out.len() {
            out[available..].fill(0.0);
            self.priming = true;
            return true;
        }
        false
    }

    pub fn clear(&mut self) {
        self.samples.clear();
        self.priming = true;
    }
}

pub struct AudioShared {
    buffer: Mutex<JitterBuffer>,
    resampler: Mutex<Resampler>,
    volume_bits: AtomicU32,
    muted: AtomicBool,
    latency_ms: AtomicU32,
    device_rate: AtomicU32,
    test_frames: AtomicU32,
    underruns: AtomicU64,
    dropped_frames: AtomicU64,
    received_frames: AtomicU64,
    device_ok: AtomicBool,
    device_name: Mutex<String>,
    last_error: Mutex<Option<String>>,
}

#[derive(Clone, Debug, Default)]
pub struct AudioStatus {
    pub device_name: String,
    pub device_rate: u32,
    pub buffered_ms: u32,
    pub target_ms: u32,
    pub underruns: u64,
    pub dropped_frames: u64,
    pub device_ok: bool,
    pub last_error: Option<String>,
}

enum Command {
    SetDevice(Option<String>),
    Shutdown,
}

pub struct AudioOutput {
    shared: Arc<AudioShared>,
    commands: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl AudioOutput {
    pub fn start(device_id: Option<String>, volume: f32, muted: bool, latency_ms: u32) -> Self {
        let shared = Arc::new(AudioShared {
            buffer: Mutex::new(JitterBuffer::new(48_000, latency_ms)),
            resampler: Mutex::new(Resampler::new(48_000, 48_000)),
            volume_bits: AtomicU32::new(volume.to_bits()),
            muted: AtomicBool::new(muted),
            latency_ms: AtomicU32::new(latency_ms),
            device_rate: AtomicU32::new(48_000),
            test_frames: AtomicU32::new(0),
            underruns: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
            received_frames: AtomicU64::new(0),
            device_ok: AtomicBool::new(false),
            device_name: Mutex::new(String::new()),
            last_error: Mutex::new(None),
        });
        let (commands, rx) = mpsc::channel();
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("audio-output".into())
            .spawn(move || audio_thread(thread_shared, rx, device_id))
            .expect("spawn audio thread");
        Self {
            shared,
            commands,
            thread: Some(thread),
        }
    }

    pub fn sink(&self) -> Arc<AudioShared> {
        self.shared.clone()
    }

    pub fn set_device(&self, device_id: Option<String>) {
        let _ = self.commands.send(Command::SetDevice(device_id));
    }

    pub fn set_volume(&self, volume: f32) {
        self.shared
            .volume_bits
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
    }

    pub fn set_latency(&self, latency_ms: u32) {
        self.shared.latency_ms.store(latency_ms, Ordering::Relaxed);
        let rate = self.shared.device_rate.load(Ordering::Relaxed);
        self.shared.buffer.lock().configure(rate, latency_ms);
    }

    pub fn play_test_tone(&self) {
        let rate = self.shared.device_rate.load(Ordering::Relaxed);
        self.shared
            .test_frames
            .store(rate * TEST_TONE_SECONDS, Ordering::Relaxed);
    }

    pub fn clear(&self) {
        self.shared.buffer.lock().clear();
    }

    pub fn status(&self) -> AudioStatus {
        let s = &self.shared;
        let rate = s.device_rate.load(Ordering::Relaxed).max(1);
        let buffered = s.buffer.lock().buffered_frames() as u64;
        AudioStatus {
            device_name: s.device_name.lock().clone(),
            device_rate: rate,
            buffered_ms: (buffered * 1000 / rate as u64) as u32,
            target_ms: s.latency_ms.load(Ordering::Relaxed),
            underruns: s.underruns.load(Ordering::Relaxed),
            dropped_frames: s.dropped_frames.load(Ordering::Relaxed),
            device_ok: s.device_ok.load(Ordering::Relaxed),
            last_error: s.last_error.lock().clone(),
        }
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl AudioShared {
    /// Called on the network thread with decoded PS5 audio (interleaved i16).
    pub fn push_pcm(&self, pcm: &[i16], channels: u32, rate: u32) {
        let channels = channels.max(1) as usize;
        let frames: Vec<[f32; OUTPUT_CHANNELS]> = pcm
            .chunks_exact(channels)
            .map(|f| {
                let l = f[0] as f32 / 32768.0;
                let r = if channels > 1 {
                    f[1] as f32 / 32768.0
                } else {
                    l
                };
                [l, r]
            })
            .collect();
        self.received_frames
            .fetch_add(frames.len() as u64, Ordering::Relaxed);
        let device_rate = self.device_rate.load(Ordering::Relaxed);
        let mut out = Vec::with_capacity(frames.len() * OUTPUT_CHANNELS * 2);
        {
            let mut resampler = self.resampler.lock();
            if resampler.rates() != (rate, device_rate) {
                *resampler = Resampler::new(rate, device_rate);
            }
            resampler.process(&frames, &mut out);
        }
        let dropped = self.buffer.lock().push(&out);
        if dropped > 0 {
            self.dropped_frames
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
    }
}

fn audio_thread(
    shared: Arc<AudioShared>,
    commands: Receiver<Command>,
    mut device_id: Option<String>,
) {
    let (error_tx, error_rx) = mpsc::channel::<String>();
    let mut stream = open_stream(&shared, device_id.as_deref(), error_tx.clone());
    loop {
        if let Ok(err) = error_rx.try_recv() {
            log::warn!("Audio device error: {err}; reopening");
            *shared.last_error.lock() = Some(err);
            shared.device_ok.store(false, Ordering::Relaxed);
            drop(stream.take());
            std::thread::sleep(Duration::from_millis(500));
            while error_rx.try_recv().is_ok() {}
            stream = open_stream(&shared, device_id.as_deref(), error_tx.clone());
            continue;
        }
        match commands.recv_timeout(Duration::from_millis(250)) {
            Ok(Command::SetDevice(id)) => {
                device_id = id;
                drop(stream.take());
                stream = open_stream(&shared, device_id.as_deref(), error_tx.clone());
            }
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                if stream.is_none() {
                    stream = open_stream(&shared, device_id.as_deref(), error_tx.clone());
                    if stream.is_none() {
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            }
        }
    }
    drop(stream);
}

fn open_stream(
    shared: &Arc<AudioShared>,
    device_id: Option<&str>,
    errors: Sender<String>,
) -> Option<cpal::Stream> {
    let host = cpal::default_host();
    let device = device_id
        .and_then(|id| {
            let found = host
                .output_devices()
                .ok()?
                .find(|d| d.id().map(|did| did.to_string() == id).unwrap_or(false));
            if found.is_none() {
                log::warn!("Selected audio device is not available, using the default one");
            }
            found
        })
        .or_else(|| host.default_output_device());
    let Some(device) = device else {
        *shared.last_error.lock() = Some("No audio output device".into());
        shared.device_ok.store(false, Ordering::Relaxed);
        return None;
    };
    let name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "Unknown device".into());
    let supported = match device.default_output_config() {
        Ok(c) => c,
        Err(e) => {
            *shared.last_error.lock() = Some(format!("{name}: {e}"));
            return None;
        }
    };
    let format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let rate = config.sample_rate;
    shared.device_rate.store(rate, Ordering::Relaxed);
    shared
        .buffer
        .lock()
        .configure(rate, shared.latency_ms.load(Ordering::Relaxed));
    shared.buffer.lock().clear();

    let result = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, shared.clone(), errors),
        SampleFormat::I16 => build::<i16>(&device, &config, shared.clone(), errors),
        SampleFormat::I32 => build::<i32>(&device, &config, shared.clone(), errors),
        SampleFormat::U16 => build::<u16>(&device, &config, shared.clone(), errors),
        other => Err(format!("Unsupported sample format: {other}")),
    };
    match result {
        Ok(stream) => {
            log::info!(
                "Audio output: {name}, {rate} Hz, {} ch, {format}",
                config.channels
            );
            *shared.device_name.lock() = name;
            *shared.last_error.lock() = None;
            shared.device_ok.store(true, Ordering::Relaxed);
            Some(stream)
        }
        Err(e) => {
            log::error!("Cannot open audio output {name}: {e}");
            *shared.last_error.lock() = Some(e);
            shared.device_ok.store(false, Ordering::Relaxed);
            None
        }
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    shared: Arc<AudioShared>,
    errors: Sender<String>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let rate = config.sample_rate as f32;
    let mut stereo: Vec<f32> = Vec::new();
    let mut phase = 0f32;
    let stream = device
        .build_output_stream(
            *config,
            move |data: &mut [T], _| {
                let frames = data.len() / channels.max(1);
                stereo.resize(frames * OUTPUT_CHANNELS, 0.0);
                if shared.buffer.lock().pop_into(&mut stereo) {
                    shared.underruns.fetch_add(1, Ordering::Relaxed);
                }
                let gain = if shared.muted.load(Ordering::Relaxed) {
                    0.0
                } else {
                    let v = f32::from_bits(shared.volume_bits.load(Ordering::Relaxed));
                    v * v
                };
                let mut tone_left = shared.test_frames.load(Ordering::Relaxed);
                for (i, frame) in data.chunks_mut(channels).enumerate() {
                    let mut l = stereo[i * 2];
                    let mut r = stereo[i * 2 + 1];
                    if tone_left > 0 {
                        let envelope = (tone_left as f32 / rate).min(0.05) / 0.05;
                        let s = (phase * std::f32::consts::TAU).sin() * 0.25 * envelope;
                        phase = (phase + 440.0 / rate).fract();
                        l += s;
                        r += s;
                        tone_left -= 1;
                    }
                    for (c, sample) in frame.iter_mut().enumerate() {
                        let value = match c {
                            0 => l,
                            1 => r,
                            _ => 0.0,
                        };
                        *sample = T::from_sample((value * gain).clamp(-1.0, 1.0));
                    }
                }
                shared.test_frames.store(tone_left, Ordering::Relaxed);
            },
            move |err| {
                let _ = errors.send(err.to_string());
            },
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_passthrough_when_rates_match() {
        let mut r = Resampler::new(48_000, 48_000);
        let mut out = Vec::new();
        r.process(&[[0.1, 0.2], [0.3, 0.4]], &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn resampler_produces_proportional_frame_count() {
        let mut r = Resampler::new(48_000, 44_100);
        let input: Vec<[f32; 2]> = (0..48_000).map(|i| [(i as f32 * 0.001).sin(); 2]).collect();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        let frames = out.len() / 2;
        assert!((frames as i64 - 44_100).abs() <= 2, "got {frames}");
    }

    #[test]
    fn resampler_upsamples() {
        let mut r = Resampler::new(48_000, 96_000);
        let input = vec![[0.5f32, -0.5]; 480];
        let mut out = Vec::new();
        r.process(&input, &mut out);
        let frames = out.len() / 2;
        assert!((frames as i64 - 958).abs() <= 2, "got {frames}");
        assert!(out.iter().all(|s| s.abs() <= 0.5 + f32::EPSILON));
    }

    #[test]
    fn jitter_buffer_primes_before_playing() {
        let mut b = JitterBuffer::new(1000, 100);
        b.push(&vec![0.5; 50 * 2]);
        let mut out = vec![1.0; 20];
        assert!(!b.pop_into(&mut out));
        assert!(out.iter().all(|s| *s == 0.0));
        b.push(&vec![0.5; 60 * 2]);
        b.pop_into(&mut out);
        assert!(out.iter().all(|s| *s == 0.5));
    }

    #[test]
    fn jitter_buffer_trims_back_to_target() {
        let mut b = JitterBuffer::new(1000, 100);
        let dropped = b.push(&vec![0.0; 400 * 2]);
        assert_eq!(b.buffered_frames(), 100);
        assert_eq!(dropped, 300);
    }

    #[test]
    fn jitter_buffer_reports_underrun_and_reprimes() {
        let mut b = JitterBuffer::new(1000, 10);
        b.push(&[0.25; 12 * 2]);
        let mut out = vec![0.0; 30 * 2];
        assert!(b.pop_into(&mut out));
        assert_eq!(out[0], 0.25);
        assert_eq!(out[59], 0.0);
        let mut out2 = vec![1.0; 4];
        assert!(!b.pop_into(&mut out2));
        assert_eq!(out2, vec![0.0; 4]);
    }

    #[test]
    fn push_pcm_converts_and_buffers() {
        let audio = AudioShared {
            buffer: Mutex::new(JitterBuffer::new(48_000, 60)),
            resampler: Mutex::new(Resampler::new(48_000, 48_000)),
            volume_bits: AtomicU32::new(1f32.to_bits()),
            muted: AtomicBool::new(false),
            latency_ms: AtomicU32::new(60),
            device_rate: AtomicU32::new(48_000),
            test_frames: AtomicU32::new(0),
            underruns: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
            received_frames: AtomicU64::new(0),
            device_ok: AtomicBool::new(true),
            device_name: Mutex::new(String::new()),
            last_error: Mutex::new(None),
        };
        let pcm: Vec<i16> = (0..480 * 2)
            .map(|i| if i % 2 == 0 { 16384 } else { -16384 })
            .collect();
        audio.push_pcm(&pcm, 2, 48_000);
        assert_eq!(audio.buffer.lock().buffered_frames(), 480);
        assert_eq!(audio.received_frames.load(Ordering::Relaxed), 480);
    }
}
