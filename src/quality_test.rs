use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crate::config::{Resolution, VideoSettings};
use crate::native::{RawStats, Session, SessionEvent, SessionSink};
use crate::session::{ConnectTarget, Waker, friendly_quit_message, session_params};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
const WARMUP: Duration = Duration::from_secs(3);
const MEASURE: Duration = Duration::from_secs(8);
const COOLDOWN: Duration = Duration::from_secs(4);
const PROBE_BITRATE_KBPS: u32 = 40_000;
const BITRATE_LADDER_KBPS: [u32; 6] = [15_000, 25_000, 40_000, 60_000, 80_000, 100_000];
const MAX_LOSS: f32 = 0.01;
const SAFETY_MARGIN: f32 = 0.85;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrialKind {
    Resolution,
    Bitrate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trial {
    pub kind: TrialKind,
    pub size: (u32, u32),
    pub bitrate_kbps: u32,
}

impl Trial {
    pub fn label(&self) -> String {
        match self.kind {
            TrialKind::Resolution => format!("Request {}x{}", self.size.0, self.size.1),
            TrialKind::Bitrate => format!("{} Mb/s at {}p", self.bitrate_kbps / 1000, self.size.1),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Measurement {
    pub decoded: (u32, u32),
    pub fps: f32,
    pub mbps: f32,
    pub loss: f32,
    pub decode_errors: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrialResult {
    pub trial: Trial,
    pub passed: bool,
    pub detail: String,
}

pub fn measure(
    before: &RawStats,
    after: &RawStats,
    seconds: f32,
    decoded: (u32, u32),
) -> Measurement {
    let frames = after.frames_decoded.saturating_sub(before.frames_decoded) as f32;
    let received = after.frames_received.saturating_sub(before.frames_received) as f32;
    let lost = after.frames_lost.saturating_sub(before.frames_lost) as f32;
    let bytes = after.video_bytes.saturating_sub(before.video_bytes) as f32;
    Measurement {
        decoded,
        fps: frames / seconds,
        mbps: bytes * 8.0 / seconds / 1_000_000.0,
        loss: if received + lost > 0.0 {
            lost / (received + lost)
        } else {
            0.0
        },
        decode_errors: after.decode_errors.saturating_sub(before.decode_errors),
    }
}

pub fn judge(trial: &Trial, m: &Measurement, target_fps: u32) -> (bool, String) {
    let stats = format!(
        "{}x{}, {:.0} fps, {:.1} Mb/s received, {:.1}% lost",
        m.decoded.0,
        m.decoded.1,
        m.fps,
        m.mbps,
        m.loss * 100.0
    );
    let smooth = m.fps >= target_fps as f32 * 0.85;
    match trial.kind {
        TrialKind::Resolution if m.decoded != trial.size => (
            false,
            format!("the console sent {}x{} instead", m.decoded.0, m.decoded.1),
        ),
        TrialKind::Resolution if !smooth => (false, format!("too slow: {stats}")),
        TrialKind::Resolution => (true, stats),
        TrialKind::Bitrate if m.loss > MAX_LOSS || m.decode_errors > 0 => {
            (false, format!("packet loss: {stats}"))
        }
        TrialKind::Bitrate if !smooth => (false, format!("frames dropped: {stats}")),
        TrialKind::Bitrate => (true, stats),
    }
}

/// Highest passing resolution and a bitrate with headroom below the highest passing step.
pub fn recommend(results: &[TrialResult]) -> (Resolution, Option<u32>) {
    let resolution = results
        .iter()
        .filter(|r| r.passed && r.trial.kind == TrialKind::Resolution)
        .filter_map(|r| Resolution::from_size(r.trial.size.0, r.trial.size.1))
        .max_by_key(|r| r.size().0)
        .unwrap_or(Resolution::P1080);
    let bitrate = results
        .iter()
        .filter(|r| r.passed && r.trial.kind == TrialKind::Bitrate)
        .map(|r| r.trial.bitrate_kbps)
        .max()
        .map(|kbps| ((kbps as f32 * SAFETY_MARGIN) as u32 / 500 * 500).max(10_000));
    (resolution, bitrate)
}

pub fn resolution_trials() -> VecDeque<Trial> {
    [(2560, 1440), (3840, 2160)]
        .into_iter()
        .map(|size| Trial {
            kind: TrialKind::Resolution,
            size,
            bitrate_kbps: PROBE_BITRATE_KBPS,
        })
        .collect()
}

pub fn bitrate_trials(size: (u32, u32)) -> VecDeque<Trial> {
    BITRATE_LADDER_KBPS
        .into_iter()
        .map(|bitrate_kbps| Trial {
            kind: TrialKind::Bitrate,
            size,
            bitrate_kbps,
        })
        .collect()
}

struct Sink {
    events: Sender<SessionEvent>,
    waker: Waker,
}

impl SessionSink for Sink {
    fn on_event(&self, event: SessionEvent) {
        let _ = self.events.send(event);
        (self.waker)();
    }

    fn on_audio(&self, _pcm: &[i16], _channels: u32, _rate: u32) {}
}

struct Running {
    trial: Trial,
    session: Session,
    events: Receiver<SessionEvent>,
    started: Instant,
    connected_at: Option<Instant>,
    first_frame_at: Option<Instant>,
    window: Option<(Instant, RawStats)>,
}

pub struct QualityTest {
    target: ConnectTarget,
    video: VideoSettings,
    device: *mut std::ffi::c_void,
    waker: Waker,
    queue: VecDeque<Trial>,
    phase: TrialKind,
    running: Option<Running>,
    next_start: Instant,
    pub results: Vec<TrialResult>,
    pub finished: bool,
    pub aborted: Option<String>,
}

impl QualityTest {
    pub fn new(
        target: ConnectTarget,
        video: VideoSettings,
        device: *mut std::ffi::c_void,
        waker: Waker,
    ) -> Self {
        log::info!("Quality test started for {}", target.console.nickname);
        Self {
            target,
            video,
            device,
            waker,
            queue: resolution_trials(),
            phase: TrialKind::Resolution,
            running: None,
            next_start: Instant::now() + COOLDOWN,
            results: Vec::new(),
            finished: false,
            aborted: None,
        }
    }

    pub fn current(&self) -> Option<(Trial, &'static str)> {
        let running = self.running.as_ref()?;
        let stage = if running.window.is_some() {
            "measuring"
        } else if running.first_frame_at.is_some() {
            "warming up"
        } else {
            "connecting"
        };
        Some((running.trial, stage))
    }

    pub fn total_planned(&self) -> usize {
        self.results.len() + self.queue.len() + usize::from(self.running.is_some())
    }

    pub fn recommendation(&self) -> (Resolution, Option<u32>) {
        recommend(&self.results)
    }

    pub fn cancel(&mut self) {
        self.stop_running();
        self.queue.clear();
        self.finished = true;
        self.aborted = Some("Cancelled".into());
    }

    fn stop_running(&mut self) {
        if let Some(running) = self.running.take() {
            std::thread::Builder::new()
                .name("quality-test-teardown".into())
                .spawn(move || drop(running.session))
                .ok();
        }
        self.next_start = Instant::now() + COOLDOWN;
    }

    fn finish_trial(&mut self, passed: bool, detail: String) {
        let Some(trial) = self.running.as_ref().map(|r| r.trial) else {
            return;
        };
        log::info!(
            "Quality test {}: {} ({detail})",
            trial.label(),
            if passed { "passed" } else { "failed" }
        );
        self.stop_running();
        self.results.push(TrialResult {
            trial,
            passed,
            detail,
        });
        match trial.kind {
            TrialKind::Resolution if !passed => {
                self.queue.retain(|t| t.kind != TrialKind::Resolution);
            }
            TrialKind::Bitrate if !passed => self.queue.clear(),
            _ => {}
        }
    }

    fn start_next(&mut self) {
        if self.queue.is_empty() && self.phase == TrialKind::Resolution {
            self.phase = TrialKind::Bitrate;
            let (best, _) = recommend(&self.results);
            self.queue = bitrate_trials(best.size());
        }
        let Some(trial) = self.queue.pop_front() else {
            self.finished = true;
            log::info!("Quality test finished: {:?}", self.recommendation());
            return;
        };
        let (tx, rx) = mpsc::channel();
        let sink = Arc::new(Sink {
            events: tx,
            waker: self.waker.clone(),
        });
        let params = session_params(
            &self.target,
            &self.video,
            trial.size,
            trial.bitrate_kbps,
            self.device,
        );
        match Session::start(&params, sink) {
            Ok(session) => {
                self.running = Some(Running {
                    trial,
                    session,
                    events: rx,
                    started: Instant::now(),
                    connected_at: None,
                    first_frame_at: None,
                    window: None,
                })
            }
            Err(e) => {
                self.results.push(TrialResult {
                    trial,
                    passed: false,
                    detail: e.clone(),
                });
                self.aborted = Some(e);
                self.queue.clear();
                self.finished = true;
            }
        }
    }

    pub fn tick(&mut self) {
        if self.finished {
            return;
        }
        let Some(running) = self.running.as_mut() else {
            if Instant::now() >= self.next_start {
                self.start_next();
            }
            return;
        };
        let mut outcome: Option<(bool, String)> = None;
        while let Ok(event) = running.events.try_recv() {
            match event {
                SessionEvent::Connected => running.connected_at = Some(Instant::now()),
                SessionEvent::Quit {
                    reason, message, ..
                } => outcome = Some((false, friendly_quit_message(reason, &message))),
                SessionEvent::LoginPinRequest { .. } => {
                    outcome = Some((false, "The console asks for a passcode".into()));
                    self.aborted =
                        Some("Turn off the PS5 login passcode or connect normally first.".into());
                    self.queue.clear();
                }
                _ => {}
            }
        }
        let now = Instant::now();
        if outcome.is_none() {
            match (running.connected_at, running.first_frame_at, running.window) {
                (None, _, _) if running.started.elapsed() > CONNECT_TIMEOUT => {
                    outcome = Some((false, "The console did not answer in time".into()));
                }
                (Some(connected), None, _) => {
                    if running.session.frame_info().is_some() {
                        running.first_frame_at = Some(now);
                    } else if connected.elapsed() > FIRST_FRAME_TIMEOUT {
                        outcome = Some((false, "No video arrived".into()));
                    }
                }
                (Some(_), Some(first), None) if first.elapsed() >= WARMUP => {
                    running.window = Some((now, running.session.stats()));
                }
                (Some(_), Some(_), Some((start, before))) if start.elapsed() >= MEASURE => {
                    let after = running.session.stats();
                    let decoded = running
                        .session
                        .frame_info()
                        .map(|i| (i.width, i.height))
                        .unwrap_or_default();
                    let m = measure(&before, &after, start.elapsed().as_secs_f32(), decoded);
                    outcome = Some(judge(&running.trial, &m, self.video.fps));
                }
                _ => {}
            }
        }
        if let Some((passed, detail)) = outcome {
            self.finish_trial(passed, detail);
            if self.aborted.is_some() {
                self.finished = true;
            }
        }
    }
}

impl Drop for QualityTest {
    fn drop(&mut self) {
        self.stop_running();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(decoded: u64, received: u64, lost: u64, bytes: u64) -> RawStats {
        RawStats {
            frames_decoded: decoded,
            frames_received: received,
            frames_lost: lost,
            video_bytes: bytes,
            ..Default::default()
        }
    }

    fn result(kind: TrialKind, size: (u32, u32), kbps: u32, passed: bool) -> TrialResult {
        TrialResult {
            trial: Trial {
                kind,
                size,
                bitrate_kbps: kbps,
            },
            passed,
            detail: String::new(),
        }
    }

    #[test]
    fn measures_rates() {
        let m = measure(
            &stats(0, 0, 0, 0),
            &stats(480, 490, 10, 40_000_000),
            8.0,
            (1920, 1080),
        );
        assert_eq!(m.fps, 60.0);
        assert_eq!(m.mbps, 40.0);
        assert!((m.loss - 0.02).abs() < 1e-6);
    }

    #[test]
    fn resolution_trial_needs_the_requested_size() {
        let trial = Trial {
            kind: TrialKind::Resolution,
            size: (2560, 1440),
            bitrate_kbps: 40_000,
        };
        let mut m = Measurement {
            decoded: (1920, 1080),
            fps: 60.0,
            mbps: 20.0,
            loss: 0.0,
            decode_errors: 0,
        };
        assert!(!judge(&trial, &m, 60).0);
        m.decoded = (2560, 1440);
        assert!(judge(&trial, &m, 60).0);
        m.fps = 30.0;
        assert!(!judge(&trial, &m, 60).0);
    }

    #[test]
    fn bitrate_trial_fails_on_loss() {
        let trial = Trial {
            kind: TrialKind::Bitrate,
            size: (1920, 1080),
            bitrate_kbps: 60_000,
        };
        let m = Measurement {
            decoded: (1920, 1080),
            fps: 60.0,
            mbps: 50.0,
            loss: 0.03,
            decode_errors: 0,
        };
        assert!(!judge(&trial, &m, 60).0);
    }

    #[test]
    fn recommends_best_passing_with_margin() {
        let results = vec![
            result(TrialKind::Resolution, (2560, 1440), 40_000, true),
            result(TrialKind::Resolution, (3840, 2160), 40_000, false),
            result(TrialKind::Bitrate, (2560, 1440), 15_000, true),
            result(TrialKind::Bitrate, (2560, 1440), 40_000, true),
            result(TrialKind::Bitrate, (2560, 1440), 60_000, false),
        ];
        assert_eq!(recommend(&results), (Resolution::P1440, Some(34_000)));
        assert_eq!(recommend(&[]), (Resolution::P1080, None));
    }

    #[test]
    fn ladder_is_ascending() {
        let ladder = bitrate_trials((1920, 1080));
        assert!(
            ladder
                .iter()
                .zip(ladder.iter().skip(1))
                .all(|(a, b)| a.bitrate_kbps < b.bitrate_kbps)
        );
        assert_eq!(resolution_trials().len(), 2);
    }
}
