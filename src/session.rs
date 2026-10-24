use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crate::audio::AudioShared;
use crate::config::{ConsoleEntry, ConsoleSecret, VideoSettings};
use crate::controller::ControllerShared;
use crate::native::{self, Feedback, RawStats, Session, SessionEvent, SessionParams, SessionSink};

const STALL_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);
const WAKE_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(20);

pub type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Debug, PartialEq)]
pub enum StreamState {
    Idle,
    Waking {
        since: Instant,
    },
    Connecting {
        since: Instant,
    },
    WaitingForLoginPin {
        incorrect: bool,
    },
    Streaming {
        since: Instant,
    },
    Reconnecting {
        attempt: u32,
        at: Instant,
        reason: String,
    },
    Failed {
        message: String,
    },
}

impl StreamState {
    pub fn is_active(&self) -> bool {
        !matches!(self, StreamState::Idle | StreamState::Failed { .. })
    }
}

#[derive(Clone, Debug, Default)]
pub struct StreamStats {
    pub resolution: Option<(u32, u32)>,
    pub fps: f32,
    pub bitrate_mbps: f32,
    pub frames_lost: u64,
    pub frames_dropped: u64,
    pub decode_errors: u64,
    pub rtt_ms: Option<f32>,
    pub hardware_decoding: Option<bool>,
}

#[derive(Clone)]
pub struct ConnectTarget {
    pub console: ConsoleEntry,
    pub secret: ConsoleSecret,
}

struct Sink {
    generation: u64,
    events: Sender<(u64, SessionEvent)>,
    audio: Arc<AudioShared>,
    controller: Arc<ControllerShared>,
    waker: Waker,
}

impl SessionSink for Sink {
    fn on_event(&self, event: SessionEvent) {
        let _ = self.events.send((self.generation, event));
        (self.waker)();
    }

    fn on_audio(&self, pcm: &[i16], channels: u32, rate: u32) {
        self.audio.push_pcm(pcm, channels, rate);
    }

    fn on_feedback(&self, feedback: Feedback) {
        self.controller.on_feedback(feedback);
    }
}

pub struct StreamController {
    session: Option<Arc<Session>>,
    controller: Arc<ControllerShared>,
    generation: u64,
    events_tx: Sender<(u64, SessionEvent)>,
    events_rx: Receiver<(u64, SessionEvent)>,
    pub state: StreamState,
    target: Option<ConnectTarget>,
    audio: Arc<AudioShared>,
    waker: Waker,
    auto_reconnect: bool,
    reconnect_attempt: u32,
    hardware_decoding: Option<bool>,
    last_stats: Option<(Instant, RawStats)>,
    last_frame_progress: Instant,
    pub stats: StreamStats,
}

pub fn friendly_quit_message(reason: i32, raw: &str) -> String {
    match reason {
        3 => "The console refused the connection. Check that Remote Play is enabled. If it keeps happening, register the console again.".into(),
        4 => "Remote Play is already in use by another device (PS Remote Play app, PlayStation Portal). End that session and try again.".into(),
        5 => "Remote Play crashed on the console. Restart the PS5.".into(),
        6 => "Remote Play protocol version mismatch. Update the console software.".into(),
        8 => "Cannot connect to the console (TCP port 9295). Check that the PS5 is on and that Windows Firewall is not blocking the app.".into(),
        9 => "The console rejected the registration data. Register the console again.".into(),
        11 => "The console ended the stream.".into(),
        12 => "The console was turned off or went into rest mode.".into(),
        13 => "PSN registration failed.".into(),
        _ if raw.is_empty() => "The connection was interrupted.".into(),
        _ => format!("The connection was interrupted ({raw})."),
    }
}

pub fn is_retryable(reason: i32) -> bool {
    matches!(reason, 2 | 7 | 8 | 10 | 11)
}

pub fn reconnect_delay(attempt: u32) -> Duration {
    let secs = 2u64.saturating_pow(attempt.min(5));
    Duration::from_secs(secs).min(MAX_RECONNECT_DELAY)
}

impl StreamController {
    pub fn new(audio: Arc<AudioShared>, controller: Arc<ControllerShared>, waker: Waker) -> Self {
        let (events_tx, events_rx) = mpsc::channel();
        Self {
            session: None,
            controller,
            generation: 0,
            events_tx,
            events_rx,
            state: StreamState::Idle,
            target: None,
            audio,
            waker,
            auto_reconnect: true,
            reconnect_attempt: 0,
            hardware_decoding: None,
            last_stats: None,
            last_frame_progress: Instant::now(),
            stats: StreamStats::default(),
        }
    }

    pub fn session(&self) -> Option<&Session> {
        self.session.as_deref()
    }

    fn make_sink(&self) -> Arc<Sink> {
        Arc::new(Sink {
            generation: self.generation,
            events: self.events_tx.clone(),
            audio: self.audio.clone(),
            controller: self.controller.clone(),
            waker: self.waker.clone(),
        })
    }

    fn install_session(&mut self, session: Session) {
        let session = Arc::new(session);
        self.controller.set_session(Some(session.clone()));
        self.session = Some(session);
    }

    pub fn target(&self) -> Option<&ConnectTarget> {
        self.target.as_ref()
    }

    pub fn set_auto_reconnect(&mut self, enabled: bool) {
        self.auto_reconnect = enabled;
    }

    pub fn connect(&mut self, target: ConnectTarget, standby: bool) {
        self.stop_session();
        self.reconnect_attempt = 0;
        self.target = Some(target);
        if standby {
            self.wake();
        } else {
            self.state = StreamState::Connecting {
                since: Instant::now(),
            };
        }
    }

    fn wake(&mut self) {
        let Some(target) = &self.target else { return };
        match native::wakeup(
            &target.console.address,
            &target.secret.regist_key,
            target.console.is_ps5,
        ) {
            Ok(()) => {
                log::info!("Sent wake up packet to {}", target.console.address);
                self.state = StreamState::Waking {
                    since: Instant::now(),
                };
            }
            Err(e) => {
                self.state = StreamState::Failed { message: e };
            }
        }
    }

    pub fn disconnect(&mut self) {
        self.target = None;
        self.stop_session();
        self.state = StreamState::Idle;
    }

    /// Stops the session synchronously (joins chiaki threads); used on application exit.
    pub fn shutdown(&mut self) {
        self.target = None;
        self.controller.set_session(None);
        if let Some(session) = self.session.take() {
            self.generation += 1;
            drop(session);
        }
        self.state = StreamState::Idle;
    }

    pub fn submit_login_pin(&mut self, pin: &str) {
        if let Some(session) = &self.session {
            session.set_login_pin(pin);
            self.state = StreamState::Connecting {
                since: Instant::now(),
            };
        }
    }

    fn stop_session(&mut self) {
        self.controller.set_session(None);
        if let Some(session) = self.session.take() {
            self.generation += 1;
            std::thread::Builder::new()
                .name("session-teardown".into())
                .spawn(move || drop(session))
                .ok();
        }
        self.last_stats = None;
        self.stats = StreamStats::default();
        self.hardware_decoding = None;
    }

    fn start_session(&mut self, video: &VideoSettings, device: *mut std::ffi::c_void) {
        let Some(target) = self.target.clone() else {
            return;
        };
        self.stop_session();
        self.generation += 1;
        let (width, height) = video.resolution.size();
        let params = SessionParams {
            host: target.console.address.clone(),
            ps5: target.console.is_ps5,
            regist_key: target.secret.regist_key,
            morning: target.secret.rp_key,
            width,
            height,
            fps: video.fps,
            bitrate: video.bitrate_kbps,
            hevc: video.hevc && target.console.is_ps5,
            prefer_hw_decode: video.hardware_decoding,
            d3d11_device: device,
            packet_loss_max: 0.05,
        };
        let sink = self.make_sink();
        log::info!(
            "Connecting to {} ({}) {}x{}@{} {} kb/s {}",
            target.console.nickname,
            target.console.address,
            width,
            height,
            video.fps,
            video.bitrate_kbps,
            if params.hevc { "HEVC" } else { "H.264" }
        );
        match Session::start(&params, sink) {
            Ok(session) => {
                self.install_session(session);
                self.last_frame_progress = Instant::now();
                self.state = StreamState::Connecting {
                    since: Instant::now(),
                };
            }
            Err(e) => {
                log::error!("Cannot start session: {e}");
                self.schedule_reconnect_or_fail(format!("Cannot start session: {e}"), true);
            }
        }
    }

    fn schedule_reconnect_or_fail(&mut self, message: String, retryable: bool) {
        self.stop_session();
        if retryable && self.auto_reconnect && self.target.is_some() {
            self.reconnect_attempt += 1;
            let delay = reconnect_delay(self.reconnect_attempt);
            log::info!(
                "Reconnecting in {} s (attempt {})",
                delay.as_secs(),
                self.reconnect_attempt
            );
            self.state = StreamState::Reconnecting {
                attempt: self.reconnect_attempt,
                at: Instant::now() + delay,
                reason: message,
            };
        } else {
            self.state = StreamState::Failed { message };
        }
    }

    /// Drives the state machine. `console_ready` reports the latest discovery state of the target:
    /// Some(true) ready, Some(false) standby, None unknown.
    pub fn tick(
        &mut self,
        video: &VideoSettings,
        device: *mut std::ffi::c_void,
        console_ready: Option<bool>,
    ) {
        while let Ok((generation, event)) = self.events_rx.try_recv() {
            if generation != self.generation {
                continue;
            }
            self.handle_event(event);
        }

        match self.state.clone() {
            StreamState::Waking { since } => {
                if console_ready == Some(true) {
                    self.state = StreamState::Connecting {
                        since: Instant::now(),
                    };
                } else if since.elapsed() > WAKE_TIMEOUT {
                    self.state = StreamState::Failed {
                        message: "The console did not wake up. Turn it on manually or enable Stay Connected to the Internet in rest mode settings.".into(),
                    };
                }
            }
            StreamState::Connecting { since } => {
                if self.session.is_none() {
                    self.start_session(video, device);
                } else if since.elapsed() > CONNECT_TIMEOUT {
                    self.schedule_reconnect_or_fail(
                        "Timed out while connecting to the console.".into(),
                        true,
                    );
                }
            }
            StreamState::Reconnecting { at, .. } => {
                if Instant::now() >= at {
                    if console_ready == Some(false) {
                        self.wake();
                    } else {
                        self.state = StreamState::Connecting {
                            since: Instant::now(),
                        };
                    }
                }
            }
            StreamState::Streaming { .. } => {
                self.update_stats();
                if self.last_frame_progress.elapsed() > STALL_TIMEOUT {
                    log::warn!(
                        "No new frames for {} s, assuming the connection is lost",
                        STALL_TIMEOUT.as_secs()
                    );
                    self.schedule_reconnect_or_fail(
                        "The stream stopped arriving (network problem or the PC went to sleep)."
                            .into(),
                        true,
                    );
                }
            }
            _ => {}
        }
    }

    fn handle_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Connected => {
                log::info!("Connected to the console");
                self.reconnect_attempt = 0;
                self.last_frame_progress = Instant::now();
                self.state = StreamState::Streaming {
                    since: Instant::now(),
                };
            }
            SessionEvent::Nickname(name) => log::info!("Console name: {name}"),
            SessionEvent::Decoder { hardware } => self.hardware_decoding = Some(hardware),
            SessionEvent::LoginPinRequest { incorrect } => {
                self.state = StreamState::WaitingForLoginPin { incorrect };
            }
            SessionEvent::Quit {
                reason,
                is_error,
                message,
            } => {
                log::info!("Session ended: {message} (code {reason})");
                if !is_error && reason == 1 {
                    return;
                }
                let friendly = friendly_quit_message(reason, &message);
                let was_streaming = matches!(self.state, StreamState::Streaming { .. });
                self.schedule_reconnect_or_fail(
                    friendly,
                    is_retryable(reason) || (was_streaming && reason != 12),
                );
            }
        }
    }

    fn update_stats(&mut self) {
        let Some(session) = &self.session else { return };
        let now = Instant::now();
        let raw = session.stats();
        if let Some((then, previous)) = self.last_stats {
            let dt = now.duration_since(then).as_secs_f32();
            if dt < 1.0 {
                return;
            }
            let frames = raw.frames_decoded.saturating_sub(previous.frames_decoded);
            if frames > 0 {
                self.last_frame_progress = now;
            }
            self.stats.fps = frames as f32 / dt;
            self.stats.bitrate_mbps = raw.video_bytes.saturating_sub(previous.video_bytes) as f32
                * 8.0
                / dt
                / 1_000_000.0;
        } else if raw.frames_decoded > 0 {
            self.last_frame_progress = now;
        }
        self.stats.frames_lost = raw.frames_lost;
        self.stats.frames_dropped = raw.frames_dropped;
        self.stats.decode_errors = raw.decode_errors;
        self.stats.rtt_ms = (raw.rtt_us > 0).then(|| raw.rtt_us as f32 / 1000.0);
        self.stats.hardware_decoding = self.hardware_decoding;
        self.stats.resolution = session.frame_info().map(|i| (i.width, i.height));
        self.last_stats = Some((now, raw));
    }
}

impl Drop for StreamController {
    fn drop(&mut self) {
        self.controller.set_session(None);
        if let Some(session) = self.session.take() {
            drop(session);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_backoff_grows_and_caps() {
        assert_eq!(reconnect_delay(1), Duration::from_secs(2));
        assert_eq!(reconnect_delay(2), Duration::from_secs(4));
        assert_eq!(reconnect_delay(3), Duration::from_secs(8));
        assert_eq!(reconnect_delay(4), Duration::from_secs(16));
        assert_eq!(reconnect_delay(10), MAX_RECONNECT_DELAY);
    }

    #[test]
    fn auth_failures_are_not_retried() {
        assert!(!is_retryable(3));
        assert!(!is_retryable(4));
        assert!(!is_retryable(9));
        assert!(!is_retryable(12));
        assert!(is_retryable(8));
        assert!(is_retryable(11));
    }

    #[test]
    fn quit_messages_are_polish_and_specific() {
        assert!(
            friendly_quit_message(4, "").contains("another device")
                || friendly_quit_message(4, "").contains("another device")
        );
        assert!(friendly_quit_message(99, "xyz").contains("xyz"));
    }

    #[test]
    fn active_states() {
        assert!(!StreamState::Idle.is_active());
        assert!(
            !StreamState::Failed {
                message: String::new()
            }
            .is_active()
        );
        assert!(
            StreamState::Connecting {
                since: Instant::now()
            }
            .is_active()
        );
    }
}
