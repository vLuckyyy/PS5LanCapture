use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::Arc;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawHost {
    pub addr: [c_char; 64],
    pub name: [c_char; 64],
    pub host_id: [c_char; 32],
    pub host_type: [c_char; 32],
    pub system_version: [c_char; 32],
    pub running_app: [c_char; 64],
    pub state: c_int,
    pub is_ps5: c_int,
    pub target: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawRegistResult {
    pub status: c_int,
    pub target: c_int,
    pub nickname: [c_char; 32],
    pub regist_key: [u8; 16],
    pub rp_key: [u8; 16],
    pub rp_key_type: u32,
    pub server_mac: [u8; 6],
    pub error: [c_char; 256],
}

#[repr(C)]
pub struct RawSessionConfig {
    pub host: *const c_char,
    pub ps5: c_int,
    pub regist_key: [u8; 16],
    pub morning: [u8; 16],
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub codec: c_int,
    pub prefer_hw_decode: c_int,
    pub d3d11_device: *mut c_void,
    pub packet_loss_max: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RawFrameInfo {
    pub width: u32,
    pub height: u32,
    pub format: c_int,
    pub hw: c_int,
    pub sequence: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct RawStats {
    pub frames_received: u64,
    pub frames_decoded: u64,
    pub frames_lost: u64,
    pub frames_dropped: u64,
    pub decode_errors: u64,
    pub video_bytes: u64,
    pub audio_frames: u64,
    pub rtt_us: u64,
    pub packet_loss: f64,
}

type LogCb = unsafe extern "C" fn(c_int, *const c_char, *mut c_void);
type EventCb = unsafe extern "C" fn(c_int, c_int, c_int, *const c_char, *mut c_void);
type AudioCb = unsafe extern "C" fn(*const i16, usize, u32, u32, *mut c_void);
type FeedbackCb = unsafe extern "C" fn(c_int, *const u8, usize, *mut c_void);

/// Mirrors `PslcControllerInput` (native/pslc.h).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ControllerInput {
    pub buttons: u32,
    pub l2: u8,
    pub r2: u8,
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
    pub touch_active: [u8; 2],
    pub touch_x: [u16; 2],
    pub touch_y: [u16; 2],
    pub has_motion: c_int,
    pub gyro: [f32; 3],
    pub accel: [f32; 3],
    pub timestamp_us: u32,
}

#[link(name = "pslc_core", kind = "dylib")]
unsafe extern "C" {
    fn pslc_init(log_cb: LogCb, user: *mut c_void) -> c_int;
    fn pslc_version() -> *const c_char;
    fn pslc_discovery_start(unicast_host: *const c_char) -> *mut c_void;
    fn pslc_discovery_hosts(discovery: *mut c_void, out: *mut RawHost, max: c_int) -> c_int;
    fn pslc_discovery_stop(discovery: *mut c_void);
    fn pslc_wakeup(host: *const c_char, regist_key: *const c_char, ps5: c_int) -> c_int;
    fn pslc_regist_start(
        host: *const c_char,
        broadcast: c_int,
        target: c_int,
        account_id: *const u8,
        pin: u32,
        console_pin: u32,
    ) -> *mut c_void;
    fn pslc_regist_poll(regist: *mut c_void, out: *mut RawRegistResult) -> c_int;
    fn pslc_regist_free(regist: *mut c_void);
    fn pslc_session_start(
        config: *const RawSessionConfig,
        event_cb: EventCb,
        audio_cb: AudioCb,
        feedback_cb: FeedbackCb,
        user: *mut c_void,
        err: *mut c_char,
        err_size: usize,
    ) -> *mut c_void;
    fn pslc_session_set_login_pin(session: *mut c_void, pin: *const c_char);
    fn pslc_session_set_controller(session: *mut c_void, input: *const ControllerInput);
    fn pslc_session_free(session: *mut c_void);
    fn pslc_session_frame_info(session: *mut c_void, out: *mut RawFrameInfo) -> c_int;
    fn pslc_session_copy_frame(
        session: *mut c_void,
        dst_texture: *mut c_void,
        sequence: *mut u64,
    ) -> c_int;
    fn pslc_session_stats(session: *mut c_void, out: *mut RawStats);
}

pub const TARGET_PS5_1: i32 = 1_000_100;

fn c_text(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

unsafe extern "C" fn log_trampoline(level: c_int, msg: *const c_char, _user: *mut c_void) {
    if msg.is_null() {
        return;
    }
    let text = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
    let text = text.trim_end();
    match level {
        1 => log::error!(target: "native", "{text}"),
        2 => log::warn!(target: "native", "{text}"),
        3 => log::info!(target: "native", "{text}"),
        _ => log::debug!(target: "native", "{text}"),
    }
}

pub fn init() -> Result<String, String> {
    let result = unsafe { pslc_init(log_trampoline, std::ptr::null_mut()) };
    if result != 0 {
        return Err("Cannot initialize the Remote Play library".into());
    }
    Ok(unsafe { CStr::from_ptr(pslc_version()) }
        .to_string_lossy()
        .into_owned())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostState {
    Unknown,
    Ready,
    Standby,
}

#[derive(Clone, Debug)]
pub struct DiscoveredHost {
    pub addr: String,
    pub name: String,
    pub host_id: String,
    pub host_type: String,
    pub system_version: String,
    pub running_app: String,
    pub state: HostState,
    pub is_ps5: bool,
    pub target: i32,
}

impl From<&RawHost> for DiscoveredHost {
    fn from(raw: &RawHost) -> Self {
        Self {
            addr: c_text(&raw.addr),
            name: c_text(&raw.name),
            host_id: c_text(&raw.host_id),
            host_type: c_text(&raw.host_type),
            system_version: c_text(&raw.system_version),
            running_app: c_text(&raw.running_app),
            state: match raw.state {
                1 => HostState::Ready,
                2 => HostState::Standby,
                _ => HostState::Unknown,
            },
            is_ps5: raw.is_ps5 != 0,
            target: raw.target,
        }
    }
}

pub struct Discovery {
    handle: *mut c_void,
}

unsafe impl Send for Discovery {}

impl Discovery {
    pub fn start(unicast_host: Option<&str>) -> Option<Self> {
        let host = unicast_host.and_then(|h| CString::new(h).ok());
        let handle =
            unsafe { pslc_discovery_start(host.as_ref().map_or(std::ptr::null(), |h| h.as_ptr())) };
        (!handle.is_null()).then_some(Self { handle })
    }

    pub fn hosts(&self) -> Vec<DiscoveredHost> {
        let mut raw = [RawHost {
            addr: [0; 64],
            name: [0; 64],
            host_id: [0; 32],
            host_type: [0; 32],
            system_version: [0; 32],
            running_app: [0; 64],
            state: 0,
            is_ps5: 0,
            target: 0,
        }; 16];
        let count =
            unsafe { pslc_discovery_hosts(self.handle, raw.as_mut_ptr(), raw.len() as c_int) };
        raw[..count.max(0) as usize]
            .iter()
            .map(DiscoveredHost::from)
            .collect()
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        unsafe { pslc_discovery_stop(self.handle) };
    }
}

pub fn wakeup(host: &str, regist_key: &[u8; 16], ps5: bool) -> Result<(), String> {
    let host = CString::new(host).map_err(|_| "Invalid address".to_string())?;
    let mut key = [0u8; 17];
    key[..16].copy_from_slice(regist_key);
    let result = unsafe { pslc_wakeup(host.as_ptr(), key.as_ptr() as *const c_char, ps5 as c_int) };
    if result == 0 {
        Ok(())
    } else {
        Err("Cannot send the wake up packet".into())
    }
}

#[derive(Clone, Debug)]
pub struct RegisteredConsole {
    pub nickname: String,
    pub target: i32,
    pub regist_key: [u8; 16],
    pub rp_key: [u8; 16],
    pub server_mac: [u8; 6],
}

pub enum RegistStatus {
    Pending,
    Success(RegisteredConsole),
    Failed(String),
    Canceled,
}

pub struct Regist {
    handle: *mut c_void,
}

unsafe impl Send for Regist {}

impl Regist {
    pub fn start(
        host: &str,
        broadcast: bool,
        target: i32,
        account_id: &[u8; 8],
        pin: u32,
    ) -> Result<Self, String> {
        let host = CString::new(host).map_err(|_| "Invalid address".to_string())?;
        let handle = unsafe {
            pslc_regist_start(
                host.as_ptr(),
                broadcast as c_int,
                target,
                account_id.as_ptr(),
                pin,
                0,
            )
        };
        if handle.is_null() {
            return Err("Out of memory".into());
        }
        Ok(Self { handle })
    }

    pub fn poll(&self) -> RegistStatus {
        let mut raw = RawRegistResult {
            status: 0,
            target: 0,
            nickname: [0; 32],
            regist_key: [0; 16],
            rp_key: [0; 16],
            rp_key_type: 0,
            server_mac: [0; 6],
            error: [0; 256],
        };
        unsafe { pslc_regist_poll(self.handle, &mut raw) };
        match raw.status {
            0 => RegistStatus::Pending,
            1 => RegistStatus::Success(RegisteredConsole {
                nickname: c_text(&raw.nickname),
                target: raw.target,
                regist_key: raw.regist_key,
                rp_key: raw.rp_key,
                server_mac: raw.server_mac,
            }),
            3 => RegistStatus::Canceled,
            _ => RegistStatus::Failed(c_text(&raw.error)),
        }
    }
}

impl Drop for Regist {
    fn drop(&mut self) {
        unsafe { pslc_regist_free(self.handle) };
    }
}

#[derive(Clone, Debug)]
pub enum SessionEvent {
    Connected,
    Quit {
        reason: i32,
        is_error: bool,
        message: String,
    },
    LoginPinRequest {
        incorrect: bool,
    },
    Nickname(String),
    Decoder {
        hardware: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feedback {
    Rumble { left: u8, right: u8 },
    HapticRumble { left: u8, right: u8 },
    TriggerEffects([u8; 22]),
    Led([u8; 3]),
    PlayerIndex(u8),
    MotionReset,
}

impl Feedback {
    fn parse(kind: c_int, data: &[u8]) -> Option<Self> {
        Some(match (kind, data.len()) {
            (1, 2) => Feedback::Rumble {
                left: data[0],
                right: data[1],
            },
            (2, 22) => Feedback::TriggerEffects(data.try_into().ok()?),
            (3, 3) => Feedback::Led(data.try_into().ok()?),
            (4, 1) => Feedback::PlayerIndex(data[0]),
            (5, 2) => Feedback::HapticRumble {
                left: data[0],
                right: data[1],
            },
            (6, _) => Feedback::MotionReset,
            _ => return None,
        })
    }
}

pub trait SessionSink: Send + Sync {
    fn on_event(&self, event: SessionEvent);
    fn on_audio(&self, pcm: &[i16], channels: u32, rate: u32);
    fn on_feedback(&self, _feedback: Feedback) {}
}

pub struct SessionParams {
    pub host: String,
    pub ps5: bool,
    pub regist_key: [u8; 16],
    pub morning: [u8; 16],
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub hevc: bool,
    pub prefer_hw_decode: bool,
    pub d3d11_device: *mut c_void,
    pub packet_loss_max: f64,
}

pub const FORMAT_P010: i32 = 2;

pub struct Session {
    handle: *mut c_void,
    sink: *const Arc<dyn SessionSink>,
}

unsafe impl Send for Session {}
unsafe impl Sync for Session {}

unsafe extern "C" fn event_trampoline(
    kind: c_int,
    code: c_int,
    is_error: c_int,
    msg: *const c_char,
    user: *mut c_void,
) {
    let sink = unsafe { &*(user as *const Arc<dyn SessionSink>) };
    let message = if msg.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned()
    };
    let event = match kind {
        1 => SessionEvent::Connected,
        2 => SessionEvent::Quit {
            reason: code,
            is_error: is_error != 0,
            message,
        },
        3 => SessionEvent::LoginPinRequest {
            incorrect: code != 0,
        },
        4 => SessionEvent::Nickname(message),
        5 => SessionEvent::Decoder {
            hardware: code != 0,
        },
        _ => return,
    };
    sink.on_event(event);
}

unsafe extern "C" fn audio_trampoline(
    pcm: *const i16,
    frames: usize,
    channels: u32,
    rate: u32,
    user: *mut c_void,
) {
    if pcm.is_null() || channels == 0 {
        return;
    }
    let sink = unsafe { &*(user as *const Arc<dyn SessionSink>) };
    let samples = unsafe { std::slice::from_raw_parts(pcm, frames * channels as usize) };
    sink.on_audio(samples, channels, rate);
}

unsafe extern "C" fn feedback_trampoline(
    kind: c_int,
    data: *const u8,
    size: usize,
    user: *mut c_void,
) {
    let sink = unsafe { &*(user as *const Arc<dyn SessionSink>) };
    let bytes = if data.is_null() || size == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(data, size) }
    };
    if let Some(feedback) = Feedback::parse(kind, bytes) {
        sink.on_feedback(feedback);
    }
}

impl Session {
    pub fn start(params: &SessionParams, sink: Arc<dyn SessionSink>) -> Result<Self, String> {
        let host = CString::new(params.host.as_str()).map_err(|_| "Invalid address".to_string())?;
        let config = RawSessionConfig {
            host: host.as_ptr(),
            ps5: params.ps5 as c_int,
            regist_key: params.regist_key,
            morning: params.morning,
            width: params.width,
            height: params.height,
            fps: params.fps,
            bitrate: params.bitrate,
            codec: params.hevc as c_int,
            prefer_hw_decode: params.prefer_hw_decode as c_int,
            d3d11_device: params.d3d11_device,
            packet_loss_max: params.packet_loss_max,
        };
        let sink_ptr: *const Arc<dyn SessionSink> = Box::into_raw(Box::new(sink));
        let mut err = [0 as c_char; 512];
        let handle = unsafe {
            pslc_session_start(
                &config,
                event_trampoline,
                audio_trampoline,
                feedback_trampoline,
                sink_ptr as *mut c_void,
                err.as_mut_ptr(),
                err.len(),
            )
        };
        if handle.is_null() {
            drop(unsafe { Box::from_raw(sink_ptr as *mut Arc<dyn SessionSink>) });
            return Err(c_text(&err));
        }
        Ok(Self {
            handle,
            sink: sink_ptr,
        })
    }

    pub fn id(&self) -> usize {
        self.handle as usize
    }

    pub fn set_controller(&self, input: &ControllerInput) {
        unsafe { pslc_session_set_controller(self.handle, input) };
    }

    pub fn set_login_pin(&self, pin: &str) {
        if let Ok(pin) = CString::new(pin) {
            unsafe { pslc_session_set_login_pin(self.handle, pin.as_ptr()) };
        }
    }

    pub fn frame_info(&self) -> Option<RawFrameInfo> {
        let mut info = RawFrameInfo::default();
        let ok = unsafe { pslc_session_frame_info(self.handle, &mut info) };
        (ok != 0).then_some(info)
    }

    /// # Safety
    /// `texture` must be a live ID3D11Texture2D created on the device passed in `SessionParams`,
    /// matching the size and format from `frame_info`.
    pub unsafe fn copy_frame(&self, texture: *mut c_void, sequence: &mut u64) -> bool {
        unsafe { pslc_session_copy_frame(self.handle, texture, sequence) != 0 }
    }

    pub fn stats(&self) -> RawStats {
        let mut stats = RawStats::default();
        unsafe { pslc_session_stats(self.handle, &mut stats) };
        stats
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            pslc_session_free(self.handle);
            drop(Box::from_raw(self.sink as *mut Arc<dyn SessionSink>));
        }
    }
}
