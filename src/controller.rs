use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hidapi::{BusType, HidApi, HidDevice};
use parking_lot::Mutex;
use windows::Win32::UI::Input::XboxController::{
    XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_B, XINPUT_GAMEPAD_BACK, XINPUT_GAMEPAD_DPAD_DOWN,
    XINPUT_GAMEPAD_DPAD_LEFT, XINPUT_GAMEPAD_DPAD_RIGHT, XINPUT_GAMEPAD_DPAD_UP,
    XINPUT_GAMEPAD_LEFT_SHOULDER, XINPUT_GAMEPAD_LEFT_THUMB, XINPUT_GAMEPAD_RIGHT_SHOULDER,
    XINPUT_GAMEPAD_RIGHT_THUMB, XINPUT_GAMEPAD_START, XINPUT_GAMEPAD_X, XINPUT_GAMEPAD_Y,
    XINPUT_STATE, XINPUT_VIBRATION, XInputGetState, XInputSetState,
};

use crate::native::{ControllerInput, Feedback, Session};

pub const BUTTON_CROSS: u32 = 1 << 0;
pub const BUTTON_CIRCLE: u32 = 1 << 1;
pub const BUTTON_SQUARE: u32 = 1 << 2;
pub const BUTTON_TRIANGLE: u32 = 1 << 3;
pub const BUTTON_DPAD_LEFT: u32 = 1 << 4;
pub const BUTTON_DPAD_RIGHT: u32 = 1 << 5;
pub const BUTTON_DPAD_UP: u32 = 1 << 6;
pub const BUTTON_DPAD_DOWN: u32 = 1 << 7;
pub const BUTTON_L1: u32 = 1 << 8;
pub const BUTTON_R1: u32 = 1 << 9;
pub const BUTTON_L3: u32 = 1 << 10;
pub const BUTTON_R3: u32 = 1 << 11;
pub const BUTTON_OPTIONS: u32 = 1 << 12;
pub const BUTTON_CREATE: u32 = 1 << 13;
pub const BUTTON_TOUCHPAD: u32 = 1 << 14;
pub const BUTTON_PS: u32 = 1 << 15;

const SONY_VID: u16 = 0x054c;
const DUALSENSE_PIDS: &[u16] = &[0x0ce6, 0x0df2];
const USB_INPUT_LEN: usize = 64;
const BT_INPUT_LEN: usize = 78;
const USB_OUTPUT_LEN: usize = 48;
const BT_OUTPUT_LEN: usize = 78;
const GYRO_COUNTS_PER_DEG_S: f32 = 1024.0;
const ACCEL_COUNTS_PER_G: f32 = 8192.0;
const HAPTIC_HOLD: Duration = Duration::from_millis(80);
const RESCAN_INTERVAL: Duration = Duration::from_secs(1);
const TRIGGER_EFFECT_OFF: u8 = 0x05;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connection {
    Usb,
    Bluetooth,
    XInput,
}

impl Connection {
    pub fn label(self) -> &'static str {
        match self {
            Connection::Usb => "USB",
            Connection::Bluetooth => "Bluetooth",
            Connection::XInput => "XInput",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ControllerStatus {
    pub name: Option<String>,
    pub connection: Option<Connection>,
    pub battery_percent: Option<u8>,
    pub charging: bool,
    pub input: ControllerInput,
    pub forwarding: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Output {
    rumble_left: u8,
    rumble_right: u8,
    triggers: Option<[u8; 22]>,
    led: [u8; 3],
    player_leds: u8,
}

impl Default for Output {
    fn default() -> Self {
        Self {
            rumble_left: 0,
            rumble_right: 0,
            triggers: None,
            led: [0, 0, 64],
            player_leds: 0,
        }
    }
}

#[derive(Default)]
struct FeedbackState {
    rumble: (u8, u8),
    haptic: (u8, u8),
    haptic_at: Option<Instant>,
    triggers: Option<[u8; 22]>,
    led: Option<[u8; 3]>,
    player_index: Option<u8>,
}

pub struct ControllerShared {
    session: Mutex<Option<Arc<Session>>>,
    feedback: Mutex<FeedbackState>,
    status: Mutex<ControllerStatus>,
    enabled: AtomicBool,
    rumble: AtomicBool,
    triggers: AtomicBool,
    lightbar: AtomicBool,
    stop: AtomicBool,
}

impl ControllerShared {
    pub fn set_session(&self, session: Option<Arc<Session>>) {
        let mut slot = self.session.lock();
        if session.is_none() {
            *self.feedback.lock() = FeedbackState::default();
        }
        *slot = session;
    }

    pub fn on_feedback(&self, feedback: Feedback) {
        let mut state = self.feedback.lock();
        match feedback {
            Feedback::Rumble { left, right } => state.rumble = (left, right),
            Feedback::HapticRumble { left, right } => {
                state.haptic = (left, right);
                state.haptic_at = Some(Instant::now());
            }
            Feedback::TriggerEffects(effects) => state.triggers = Some(effects),
            Feedback::Led(color) => state.led = Some(color),
            Feedback::PlayerIndex(index) => state.player_index = Some(index),
            Feedback::MotionReset => {}
        }
    }

    fn desired_output(&self) -> Output {
        let state = self.feedback.lock();
        let mut out = Output::default();
        if self.rumble.load(Ordering::Relaxed) {
            let haptic_live = state.haptic_at.is_some_and(|t| t.elapsed() < HAPTIC_HOLD);
            let (hl, hr) = if haptic_live { state.haptic } else { (0, 0) };
            out.rumble_left = state.rumble.0.max(hl);
            out.rumble_right = state.rumble.1.max(hr);
        }
        if self.triggers.load(Ordering::Relaxed) {
            out.triggers = state.triggers;
        }
        if self.lightbar.load(Ordering::Relaxed) {
            if let Some(led) = state.led {
                out.led = led;
            }
            out.player_leds = player_leds(state.player_index.unwrap_or(0));
        }
        out
    }

    fn forward(&self, input: &ControllerInput) -> bool {
        if !self.enabled.load(Ordering::Relaxed) {
            return false;
        }
        let session = self.session.lock().clone();
        match session {
            Some(session) => {
                session.set_controller(input);
                true
            }
            None => false,
        }
    }

    fn set_status(&self, update: impl FnOnce(&mut ControllerStatus)) {
        update(&mut self.status.lock());
    }
}

pub struct ControllerSettings {
    pub enabled: bool,
    pub rumble: bool,
    pub triggers: bool,
    pub lightbar: bool,
}

pub struct ControllerManager {
    shared: Arc<ControllerShared>,
    thread: Option<JoinHandle<()>>,
}

impl ControllerManager {
    pub fn start(settings: &ControllerSettings, waker: Arc<dyn Fn() + Send + Sync>) -> Self {
        let shared = Arc::new(ControllerShared {
            session: Mutex::new(None),
            feedback: Mutex::new(FeedbackState::default()),
            status: Mutex::new(ControllerStatus::default()),
            enabled: AtomicBool::new(settings.enabled),
            rumble: AtomicBool::new(settings.rumble),
            triggers: AtomicBool::new(settings.triggers),
            lightbar: AtomicBool::new(settings.lightbar),
            stop: AtomicBool::new(false),
        });
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("controller-input".into())
            .spawn(move || input_thread(thread_shared, waker))
            .expect("spawn controller thread");
        Self {
            shared,
            thread: Some(thread),
        }
    }

    pub fn shared(&self) -> Arc<ControllerShared> {
        self.shared.clone()
    }

    pub fn apply(&self, settings: &ControllerSettings) {
        self.shared
            .enabled
            .store(settings.enabled, Ordering::Relaxed);
        self.shared.rumble.store(settings.rumble, Ordering::Relaxed);
        self.shared
            .triggers
            .store(settings.triggers, Ordering::Relaxed);
        self.shared
            .lightbar
            .store(settings.lightbar, Ordering::Relaxed);
    }

    pub fn status(&self) -> ControllerStatus {
        self.shared.status.lock().clone()
    }
}

impl Drop for ControllerManager {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.set_session(None);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn player_leds(index: u8) -> u8 {
    match index {
        1 => 0x04,
        2 => 0x0a,
        3 => 0x15,
        4 => 0x1b,
        5 => 0x1f,
        _ => 0x00,
    }
}

fn stick(value: u8) -> i16 {
    ((value as i32 - 128) * 256).clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

fn dpad(hat: u8) -> u32 {
    match hat & 0x0f {
        0 => BUTTON_DPAD_UP,
        1 => BUTTON_DPAD_UP | BUTTON_DPAD_RIGHT,
        2 => BUTTON_DPAD_RIGHT,
        3 => BUTTON_DPAD_DOWN | BUTTON_DPAD_RIGHT,
        4 => BUTTON_DPAD_DOWN,
        5 => BUTTON_DPAD_DOWN | BUTTON_DPAD_LEFT,
        6 => BUTTON_DPAD_LEFT,
        7 => BUTTON_DPAD_UP | BUTTON_DPAD_LEFT,
        _ => 0,
    }
}

fn face_and_shoulders(b0: u8, b1: u8, b2: u8) -> u32 {
    let mut buttons = dpad(b0);
    let map = [
        (b0, 0x10, BUTTON_SQUARE),
        (b0, 0x20, BUTTON_CROSS),
        (b0, 0x40, BUTTON_CIRCLE),
        (b0, 0x80, BUTTON_TRIANGLE),
        (b1, 0x01, BUTTON_L1),
        (b1, 0x02, BUTTON_R1),
        (b1, 0x10, BUTTON_CREATE),
        (b1, 0x20, BUTTON_OPTIONS),
        (b1, 0x40, BUTTON_L3),
        (b1, 0x80, BUTTON_R3),
        (b2, 0x01, BUTTON_PS),
        (b2, 0x02, BUTTON_TOUCHPAD),
    ];
    for (byte, mask, button) in map {
        if byte & mask != 0 {
            buttons |= button;
        }
    }
    buttons
}

/// Parsed DualSense report: input plus battery (level 0..100, charging).
#[derive(Debug, PartialEq)]
pub struct DualSenseReport {
    pub input: ControllerInput,
    pub battery: Option<(u8, bool)>,
}

/// Parses a DualSense input report (USB 0x01, Bluetooth 0x31, or the reduced Bluetooth 0x01 report
/// sent before full mode is enabled). `timestamp_us` is the host receive time.
pub fn parse_dualsense(report: &[u8], timestamp_us: u32) -> Option<DualSenseReport> {
    let offset = match (report.first()?, report.len()) {
        (0x01, n) if n >= USB_INPUT_LEN - 1 => 1,
        (0x31, n) if n >= BT_INPUT_LEN - 1 => 2,
        (0x01, n) if n >= 10 => {
            let input = ControllerInput {
                buttons: face_and_shoulders(report[5], report[6], report[7]),
                left_x: stick(report[1]),
                left_y: stick(report[2]),
                right_x: stick(report[3]),
                right_y: stick(report[4]),
                l2: report[8],
                r2: report[9],
                timestamp_us,
                ..Default::default()
            };
            return Some(DualSenseReport {
                input,
                battery: None,
            });
        }
        _ => return None,
    };
    let r = &report[offset..];
    let i16_at = |i: usize| i16::from_le_bytes([r[i], r[i + 1]]) as f32;
    let mut input = ControllerInput {
        buttons: face_and_shoulders(r[7], r[8], r[9]),
        left_x: stick(r[0]),
        left_y: stick(r[1]),
        right_x: stick(r[2]),
        right_y: stick(r[3]),
        l2: r[4],
        r2: r[5],
        has_motion: 1,
        gyro: [
            (i16_at(15) / GYRO_COUNTS_PER_DEG_S).to_radians(),
            (i16_at(17) / GYRO_COUNTS_PER_DEG_S).to_radians(),
            (i16_at(19) / GYRO_COUNTS_PER_DEG_S).to_radians(),
        ],
        accel: [
            i16_at(21) / ACCEL_COUNTS_PER_G,
            i16_at(23) / ACCEL_COUNTS_PER_G,
            i16_at(25) / ACCEL_COUNTS_PER_G,
        ],
        timestamp_us,
        ..Default::default()
    };
    for point in 0..2 {
        let p = &r[32 + point * 4..36 + point * 4];
        if p[0] & 0x80 == 0 {
            input.touch_active[point] = 1;
            input.touch_x[point] = (p[1] as u16 | ((p[2] as u16 & 0x0f) << 8)).min(1919);
            input.touch_y[point] = ((p[2] as u16 >> 4) | ((p[3] as u16) << 4)).min(1079);
        }
    }
    let status = r[52];
    let level = (status & 0x0f).min(10) * 10;
    let charging = matches!(status >> 4, 0x1 | 0x2);
    Some(DualSenseReport {
        input,
        battery: Some((level, charging)),
    })
}

fn crc32(seed: u8, data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in std::iter::once(&seed).chain(data) {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn build_output(out: &Output, bluetooth: bool, sequence: u8) -> Vec<u8> {
    let (mut report, c) = if bluetooth {
        let mut r = vec![0u8; BT_OUTPUT_LEN];
        r[0] = 0x31;
        r[1] = sequence << 4;
        r[2] = 0x10;
        (r, 3)
    } else {
        let mut r = vec![0u8; USB_OUTPUT_LEN];
        r[0] = 0x02;
        (r, 1)
    };
    report[c] = 0x01 | 0x02 | 0x04 | 0x08;
    report[c + 1] = 0x04 | 0x10;
    report[c + 2] = out.rumble_right;
    report[c + 3] = out.rumble_left;
    match out.triggers {
        Some(t) => {
            report[c + 10..c + 21].copy_from_slice(&t[11..22]);
            report[c + 21..c + 32].copy_from_slice(&t[0..11]);
        }
        None => {
            report[c + 10] = TRIGGER_EFFECT_OFF;
            report[c + 21] = TRIGGER_EFFECT_OFF;
        }
    }
    report[c + 42] = 0x00;
    report[c + 43] = out.player_leds;
    report[c + 44..c + 47].copy_from_slice(&out.led);
    if bluetooth {
        let crc = crc32(0xa2, &report[..BT_OUTPUT_LEN - 4]);
        report[BT_OUTPUT_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
    }
    report
}

struct DualSense {
    device: HidDevice,
    bluetooth: bool,
    last_output: Option<Output>,
    last_output_at: Instant,
    sequence: u8,
}

enum Source {
    DualSense(DualSense),
    XInput { index: u32, last_rumble: (u8, u8) },
}

fn open_dualsense(api: &mut HidApi) -> Option<(DualSense, String)> {
    let _ = api.refresh_devices();
    let info = api
        .device_list()
        .find(|d| {
            d.vendor_id() == SONY_VID
                && DUALSENSE_PIDS.contains(&d.product_id())
                && (d.usage_page() == 1 || d.usage_page() == 0)
        })?
        .clone();
    let device = api.open_path(info.path()).ok()?;
    let bluetooth = matches!(info.bus_type(), BusType::Bluetooth);
    if bluetooth {
        let mut calibration = [0u8; 41];
        calibration[0] = 0x05;
        let _ = device.get_feature_report(&mut calibration);
    }
    let name = info.product_string().unwrap_or("DualSense").to_string();
    Some((
        DualSense {
            device,
            bluetooth,
            last_output: None,
            last_output_at: Instant::now(),
            sequence: 0,
        },
        name,
    ))
}

fn find_xinput() -> Option<u32> {
    (0..4u32).find(|&i| {
        let mut state = XINPUT_STATE::default();
        unsafe { XInputGetState(i, &mut state) == 0 }
    })
}

fn xinput_to_input(state: &XINPUT_STATE, timestamp_us: u32) -> ControllerInput {
    let pad = &state.Gamepad;
    let mut buttons = 0;
    let map = [
        (XINPUT_GAMEPAD_A, BUTTON_CROSS),
        (XINPUT_GAMEPAD_B, BUTTON_CIRCLE),
        (XINPUT_GAMEPAD_X, BUTTON_SQUARE),
        (XINPUT_GAMEPAD_Y, BUTTON_TRIANGLE),
        (XINPUT_GAMEPAD_DPAD_UP, BUTTON_DPAD_UP),
        (XINPUT_GAMEPAD_DPAD_DOWN, BUTTON_DPAD_DOWN),
        (XINPUT_GAMEPAD_DPAD_LEFT, BUTTON_DPAD_LEFT),
        (XINPUT_GAMEPAD_DPAD_RIGHT, BUTTON_DPAD_RIGHT),
        (XINPUT_GAMEPAD_LEFT_SHOULDER, BUTTON_L1),
        (XINPUT_GAMEPAD_RIGHT_SHOULDER, BUTTON_R1),
        (XINPUT_GAMEPAD_LEFT_THUMB, BUTTON_L3),
        (XINPUT_GAMEPAD_RIGHT_THUMB, BUTTON_R3),
        (XINPUT_GAMEPAD_START, BUTTON_OPTIONS),
        (XINPUT_GAMEPAD_BACK, BUTTON_CREATE),
    ];
    for (mask, button) in map {
        if pad.wButtons.0 & mask.0 != 0 {
            buttons |= button;
        }
    }
    if pad.wButtons.0 & XINPUT_GAMEPAD_BACK.0 != 0 && pad.wButtons.0 & XINPUT_GAMEPAD_START.0 != 0 {
        buttons = (buttons & !(BUTTON_CREATE | BUTTON_OPTIONS)) | BUTTON_PS;
    }
    let invert = |v: i16| v.checked_neg().unwrap_or(i16::MAX);
    ControllerInput {
        buttons,
        l2: pad.bLeftTrigger,
        r2: pad.bRightTrigger,
        left_x: pad.sThumbLX,
        left_y: invert(pad.sThumbLY),
        right_x: pad.sThumbRX,
        right_y: invert(pad.sThumbRY),
        timestamp_us,
        ..Default::default()
    }
}

fn input_thread(shared: Arc<ControllerShared>, waker: Arc<dyn Fn() + Send + Sync>) {
    let started = Instant::now();
    let mut api = match HidApi::new() {
        Ok(api) => Some(api),
        Err(e) => {
            log::error!("HID unavailable: {e}");
            shared.set_status(|s| s.error = Some(format!("Cannot read HID devices: {e}")));
            None
        }
    };
    let mut source: Option<Source> = None;
    let mut last_scan = Instant::now() - RESCAN_INTERVAL;
    let mut last_ui_update = Instant::now();
    let mut buf = [0u8; 128];
    let mut was_forwarding = false;

    while !shared.stop.load(Ordering::Relaxed) {
        if source.is_none() {
            if last_scan.elapsed() < RESCAN_INTERVAL {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            last_scan = Instant::now();
            if let Some((pad, name)) = api.as_mut().and_then(open_dualsense) {
                let connection = if pad.bluetooth {
                    Connection::Bluetooth
                } else {
                    Connection::Usb
                };
                log::info!("Controller: {name} ({})", connection.label());
                shared.set_status(|s| {
                    *s = ControllerStatus {
                        name: Some(name),
                        connection: Some(connection),
                        ..Default::default()
                    };
                });
                source = Some(Source::DualSense(pad));
            } else if let Some(index) = find_xinput() {
                log::info!("XInput controller #{index}");
                shared.set_status(|s| {
                    *s = ControllerStatus {
                        name: Some(format!("XInput controller #{}", index + 1)),
                        connection: Some(Connection::XInput),
                        ..Default::default()
                    };
                });
                source = Some(Source::XInput {
                    index,
                    last_rumble: (0, 0),
                });
            }
            if source.is_some() {
                waker();
            }
            continue;
        }

        let timestamp_us = started.elapsed().as_micros() as u32;
        let mut lost = false;
        let mut sample: Option<(ControllerInput, Option<(u8, bool)>)> = None;
        match source.as_mut().unwrap() {
            Source::DualSense(pad) => {
                match pad.device.read_timeout(&mut buf, 8) {
                    Ok(0) => {}
                    Ok(n) => {
                        if let Some(report) = parse_dualsense(&buf[..n], timestamp_us) {
                            sample = Some((report.input, report.battery));
                        }
                    }
                    Err(e) => {
                        log::info!("Controller disconnected: {e}");
                        lost = true;
                    }
                }
                if !lost {
                    let desired = shared.desired_output();
                    let stale = pad.last_output_at.elapsed() > Duration::from_secs(2);
                    if pad.last_output != Some(desired) || stale {
                        pad.sequence = (pad.sequence + 1) & 0x0f;
                        let report = build_output(&desired, pad.bluetooth, pad.sequence);
                        if let Err(e) = pad.device.write(&report) {
                            log::debug!("Controller write failed: {e}");
                        }
                        pad.last_output = Some(desired);
                        pad.last_output_at = Instant::now();
                    }
                }
            }
            Source::XInput { index, last_rumble } => {
                let mut state = XINPUT_STATE::default();
                if unsafe { XInputGetState(*index, &mut state) } != 0 {
                    lost = true;
                } else {
                    sample = Some((xinput_to_input(&state, timestamp_us), None));
                    let desired = shared.desired_output();
                    let rumble = (desired.rumble_left, desired.rumble_right);
                    if rumble != *last_rumble {
                        let vibration = XINPUT_VIBRATION {
                            wLeftMotorSpeed: rumble.0 as u16 * 257,
                            wRightMotorSpeed: rumble.1 as u16 * 257,
                        };
                        unsafe { XInputSetState(*index, &vibration) };
                        *last_rumble = rumble;
                    }
                }
                std::thread::sleep(Duration::from_millis(4));
            }
        }

        if lost {
            source = None;
            if was_forwarding {
                shared.forward(&ControllerInput::default());
            }
            was_forwarding = false;
            shared.set_status(|s| *s = ControllerStatus::default());
            waker();
            continue;
        }
        if let Some((input, battery)) = sample {
            let forwarded = shared.forward(&input);
            was_forwarding = forwarded;
            shared.set_status(|s| {
                s.input = input;
                s.forwarding = forwarded;
                if let Some((level, charging)) = battery {
                    s.battery_percent = Some(level);
                    s.charging = charging;
                }
            });
            if last_ui_update.elapsed() > Duration::from_millis(100) {
                last_ui_update = Instant::now();
                waker();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb_report() -> Vec<u8> {
        let mut r = vec![0u8; USB_INPUT_LEN];
        r[0] = 0x01;
        r[1..5].copy_from_slice(&[128, 128, 128, 128]);
        r[8] = 0x08;
        r[33] = 0x80;
        r[37] = 0x80;
        r
    }

    #[test]
    fn neutral_usb_report() {
        let report = parse_dualsense(&usb_report(), 7).unwrap();
        assert_eq!(report.input.buttons, 0);
        assert_eq!(report.input.left_x, 0);
        assert_eq!(report.input.touch_active, [0, 0]);
        assert_eq!(report.input.has_motion, 1);
        assert_eq!(report.input.timestamp_us, 7);
    }

    #[test]
    fn buttons_sticks_and_triggers() {
        let mut r = usb_report();
        r[1] = 0;
        r[2] = 255;
        r[5] = 200;
        r[6] = 10;
        r[8] = 0x20 | 0x80 | 0x02;
        r[9] = 0x01 | 0x20;
        r[10] = 0x01 | 0x02;
        let input = parse_dualsense(&r, 0).unwrap().input;
        assert_eq!(input.left_x, -32768);
        assert_eq!(input.left_y, 32512);
        assert_eq!((input.l2, input.r2), (200, 10));
        assert_eq!(
            input.buttons,
            BUTTON_CROSS
                | BUTTON_TRIANGLE
                | BUTTON_DPAD_RIGHT
                | BUTTON_L1
                | BUTTON_OPTIONS
                | BUTTON_PS
                | BUTTON_TOUCHPAD
        );
    }

    #[test]
    fn touch_and_motion() {
        let mut r = usb_report();
        r[33..37].copy_from_slice(&[0x01, 0x7f, 0x47, 0x21]);
        r[16..18].copy_from_slice(&1024i16.to_le_bytes());
        r[22..24].copy_from_slice(&8192i16.to_le_bytes());
        let input = parse_dualsense(&r, 0).unwrap().input;
        assert_eq!(input.touch_active, [1, 0]);
        assert_eq!(input.touch_x[0], 0x77f);
        assert_eq!(input.touch_y[0], 0x214);
        assert!((input.gyro[0] - 1f32.to_radians()).abs() < 1e-6);
        assert!((input.accel[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn bluetooth_report_is_offset_by_one() {
        let usb = usb_report();
        let mut bt = vec![0u8; BT_INPUT_LEN];
        bt[0] = 0x31;
        bt[2..2 + USB_INPUT_LEN - 1].copy_from_slice(&usb[1..]);
        bt[9] = 0x20;
        let input = parse_dualsense(&bt, 0).unwrap().input;
        assert_eq!(input.buttons & BUTTON_CROSS, BUTTON_CROSS);
    }

    #[test]
    fn reduced_bluetooth_report() {
        let r = [0x01, 128, 128, 128, 128, 0x28, 0x00, 0x00, 0, 0];
        let input = parse_dualsense(&r, 0).unwrap().input;
        assert_eq!(input.buttons, BUTTON_CROSS);
        assert_eq!(input.has_motion, 0);
    }

    #[test]
    fn rejects_unknown_reports() {
        assert!(parse_dualsense(&[0x05, 1, 2], 0).is_none());
        assert!(parse_dualsense(&[], 0).is_none());
    }

    #[test]
    fn usb_output_layout() {
        let out = Output {
            rumble_left: 200,
            rumble_right: 50,
            triggers: Some({
                let mut t = [0u8; 22];
                t[0] = 0x21;
                t[11] = 0x26;
                t
            }),
            led: [1, 2, 3],
            player_leds: 0x04,
        };
        let r = build_output(&out, false, 0);
        assert_eq!(r.len(), USB_OUTPUT_LEN);
        assert_eq!(r[0], 0x02);
        assert_eq!(r[1], 0x0f);
        assert_eq!((r[3], r[4]), (50, 200));
        assert_eq!(r[11], 0x26);
        assert_eq!(r[22], 0x21);
        assert_eq!(r[44], 0x04);
        assert_eq!(&r[45..48], &[1, 2, 3]);
    }

    #[test]
    fn bluetooth_output_has_valid_crc() {
        let r = build_output(&Output::default(), true, 3);
        assert_eq!(r.len(), BT_OUTPUT_LEN);
        assert_eq!(r[0], 0x31);
        assert_eq!(r[1], 0x30);
        let crc = u32::from_le_bytes(r[74..78].try_into().unwrap());
        assert_eq!(crc, crc32(0xa2, &r[..74]));
        assert_eq!(r[3 + 10], TRIGGER_EFFECT_OFF);
    }

    #[test]
    fn crc32_matches_reference() {
        assert_eq!(crc32(b'1', b"23456789"), 0xcbf4_3926);
    }

    #[test]
    fn xinput_mapping_inverts_y_and_maps_guide_combo() {
        let mut state = XINPUT_STATE::default();
        state.Gamepad.sThumbLY = i16::MIN;
        state.Gamepad.wButtons = XINPUT_GAMEPAD_BACK | XINPUT_GAMEPAD_START | XINPUT_GAMEPAD_A;
        let input = xinput_to_input(&state, 0);
        assert_eq!(input.left_y, i16::MAX);
        assert_eq!(input.buttons, BUTTON_PS | BUTTON_CROSS);
    }

    #[test]
    fn player_led_patterns() {
        assert_eq!(player_leds(1), 0x04);
        assert_eq!(player_leds(0), 0);
    }
}
