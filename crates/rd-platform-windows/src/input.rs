//! `SendInput` injection.
//!
//! [`SendInputInjector`] turns viewer [`InputEvent`]s into Win32 input. All translation
//! (coordinates, wheel units, which keys and buttons are held) is pure and lives in
//! `InputState`, which describes each event as `Synth` values for an `EventSink`. Only
//! `SendInputSink` touches Win32; tests record into a sink instead, so they never inject
//! anything.
//!
//! Conventions:
//! - MOUSE_ABS / MOUSE_BTN `x`, `y` are 0..=65535 across the video frame. With a
//!   [`CaptureGeometry`] they map through the rect the captured display occupies in that frame
//!   (letterbox aware, see [`crate::geometry`]) onto the display's rect in virtual-screen
//!   pixels, clamped to the display. Without one the frame is the primary display.
//! - MOUSE_REL deltas move the pointer like a mouse would (pointer lock). Buttons then act
//!   where the pointer is, not at the viewer's stale absolute position.
//! - WHEEL deltas use DOM signs (`dy > 0` scrolls down, `dx > 0` scrolls right). Windows wheel
//!   deltas are positive for up and right: 120 per notch, a notch being three lines (the
//!   viewer's Line mode) or about 100 CSS pixels (Pixel mode, e.g. a Mac trackpad, which comes
//!   through as high-resolution wheel deltas; fractions carry over).
//! - KEY carries HID usages, injected as scan codes ([`keymap::win_scancode`]), so the host's
//!   own keyboard layout turns them into characters, exactly like a physical key. The table
//!   follows the low-level keyboard hook's convention, which differs from injection for two
//!   keys: Num Lock (`E0 45` in the table) is injected as scan code `45`, and Pause (`45` in
//!   the table; really `E1 1D 45`) as the virtual key `VK_PAUSE`.
//! - Caps Lock is a toggle on Windows as on the Mac: the viewer sends a press per toggle. The
//!   viewer assumes the host's Caps Lock is off when a session starts and after RELEASE_ALL,
//!   so it is switched off then.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rd_agent_core::{InputEvent, InputInjector};
use rd_proto::bin::{MouseButton, WheelMode};
use rd_proto::keymap;
use tracing::{debug, warn};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_CAPITAL, VK_PAUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

use crate::display::{self, Rect};
use crate::geometry::{self, CaptureGeometry, CaptureLayout};

/// Largest normalised MOUSE_ABS / MOUSE_BTN coordinate.
const NORM_MAX: f64 = 65535.0;
/// Wheel deltas accumulate in fifths of a Windows wheel unit (exact for both modes).
const WHEEL_FIFTHS: i64 = 5;
/// One viewer line (Line mode): a notch (120 units) scrolls three lines, so 40 units.
const FIFTHS_PER_LINE: i64 = 40 * WHEEL_FIFTHS;
/// One CSS pixel (Pixel mode): Chromium on Windows reports 100 px per notch, so 1.2 units.
const FIFTHS_PER_PIXEL: i64 = 6;
/// Display rects are re-read at least this often (display arrangement or mode changes).
const BOUNDS_MAX_AGE: Duration = Duration::from_secs(2);
/// `XBUTTON1` / `XBUTTON2` (mouseData of MOUSEEVENTF_X*).
const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;
/// Scan codes that need special handling (table values, see the module docs).
const SC_PAUSE: u16 = 0x0045;
const SC_NUM_LOCK: u16 = 0xe045;
const SC_CAPS_LOCK: u16 = 0x003a;

const BUTTONS: [MouseButton; 5] = [
    MouseButton::Left,
    MouseButton::Middle,
    MouseButton::Right,
    MouseButton::Back,
    MouseButton::Forward,
];

/// A key as injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    /// Scan code; `0xE0xx` = extended.
    Scan(u16),
    /// Virtual key (with the scan code that hardware reports for it).
    Virtual { vk: u16, scan: u16 },
}

impl Key {
    fn from_table(sc: u16) -> Key {
        match sc {
            SC_PAUSE => Key::Virtual {
                vk: VK_PAUSE.0,
                scan: 0x45,
            },
            SC_NUM_LOCK => Key::Scan(0x45),
            sc => Key::Scan(sc),
        }
    }
}

/// One input event to inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Synth {
    /// Pointer to this virtual-screen pixel.
    MoveTo {
        x: i32,
        y: i32,
    },
    /// Pointer moved by this many mickeys.
    MoveBy {
        dx: i32,
        dy: i32,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    /// Windows wheel units: `vertical > 0` scrolls up, `horizontal > 0` right.
    Wheel {
        vertical: i32,
        horizontal: i32,
    },
    Key {
        key: Key,
        down: bool,
    },
}

/// Where translated input goes.
trait EventSink {
    /// Injects `events` together (one `SendInput` call).
    fn post(&mut self, events: &[Synth]);
    /// Type `text` as Unicode: one `SendInput` call, no key table, layout or shortcuts involved.
    fn text(&mut self, text: &str);
    /// Whether Caps Lock is on, if known.
    fn caps_lock(&self) -> Option<bool>;
    /// The pointer position, virtual-screen pixels.
    fn cursor(&self) -> Option<(i32, i32)>;
}

/// Input translation state: what is held, where the pointer is.
struct InputState {
    /// Target display, virtual-screen pixels.
    bounds: Rect,
    /// How the display sits in the video frame; `None` = it fills the frame.
    layout: Option<CaptureLayout>,
    /// Last posted pointer position.
    pos: (i32, i32),
    /// The last motion was MOUSE_REL (pointer lock).
    relative: bool,
    /// Held buttons, as [`MouseButton::mask`] bits.
    buttons: u8,
    /// Held keys, in press order.
    keys: Vec<Key>,
    /// Wheel remainders (fifths of Windows units) not posted yet.
    wheel: (i64, i64),
    /// Caps Lock was switched off for this session (done before the first key).
    caps_checked: bool,
}

impl InputState {
    fn new(bounds: Rect) -> Self {
        InputState {
            bounds,
            layout: None,
            pos: (bounds.x, bounds.y),
            relative: false,
            buttons: 0,
            keys: Vec::new(),
            wheel: (0, 0),
            caps_checked: false,
        }
    }

    fn apply(&mut self, ev: InputEvent, sink: &mut dyn EventSink) {
        match ev {
            // Committed IME/soft-keyboard text: Unicode input, not scan codes.
            InputEvent::Text(text) => sink.text(&text),
            // `buttons` mirrors the viewer's view; drags follow the buttons pressed here.
            InputEvent::MouseAbs { x, y, .. } => {
                self.relative = false;
                self.pos = self.map_abs(x, y);
                sink.post(&[Synth::MoveTo {
                    x: self.pos.0,
                    y: self.pos.1,
                }]);
            }
            InputEvent::MouseRel { dx, dy } => {
                self.relative = true;
                if let Some(p) = sink.cursor() {
                    self.pos = p;
                }
                sink.post(&[Synth::MoveBy {
                    dx: i32::from(dx),
                    dy: i32::from(dy),
                }]);
            }
            InputEvent::MouseButton { button, down, x, y } => {
                let mut events = Vec::with_capacity(2);
                if !self.relative {
                    let pos = self.map_abs(x, y);
                    if pos != self.pos {
                        self.pos = pos;
                        events.push(Synth::MoveTo { x: pos.0, y: pos.1 });
                    }
                }
                if self.is_down(button) != down {
                    self.buttons ^= button.mask();
                    events.push(Synth::Button { button, down });
                }
                if !events.is_empty() {
                    sink.post(&events);
                }
            }
            InputEvent::Wheel { mode, dx, dy, .. } => {
                let per = match mode {
                    WheelMode::Line => FIFTHS_PER_LINE,
                    WheelMode::Pixel => FIFTHS_PER_PIXEL,
                };
                // DOM signs on the wire, Windows signs on the event (see the module docs).
                self.wheel.0 -= i64::from(dy) * per;
                self.wheel.1 += i64::from(dx) * per;
                let vertical = self.wheel.0 / WHEEL_FIFTHS;
                let horizontal = self.wheel.1 / WHEEL_FIFTHS;
                self.wheel.0 %= WHEEL_FIFTHS;
                self.wheel.1 %= WHEEL_FIFTHS;
                if vertical != 0 || horizontal != 0 {
                    sink.post(&[Synth::Wheel {
                        vertical: vertical.clamp(i32::MIN.into(), i32::MAX.into()) as i32,
                        horizontal: horizontal.clamp(i32::MIN.into(), i32::MAX.into()) as i32,
                    }]);
                }
            }
            InputEvent::Key { down, hid, .. } => self.key(hid, down, sink),
            InputEvent::ReleaseAll => self.release_all(sink),
        }
    }

    fn is_down(&self, button: MouseButton) -> bool {
        self.buttons & button.mask() != 0
    }

    fn map_abs(&self, x: u16, y: u16) -> (i32, i32) {
        let (u, v) = (f64::from(x) / NORM_MAX, f64::from(y) / NORM_MAX);
        let (px, py) = geometry::frame_to_display(u, v, self.layout.as_ref(), self.bounds);
        // A point on a letterbox bar lands on the nearest display edge.
        let Rect { x: bx, y: by, w, h } = self.bounds;
        let clamp = |p: f64, lo: i32, len: u32| {
            (p.floor() as i64).clamp(i64::from(lo), i64::from(lo) + i64::from(len.max(1)) - 1)
                as i32
        };
        (clamp(px, bx, w), clamp(py, by, h))
    }

    fn key(&mut self, hid: u32, down: bool, sink: &mut dyn EventSink) {
        let Some(sc) = keymap::win_scancode(hid) else {
            debug!(
                hid = format_args!("{hid:#010x}"),
                "key has no Windows scan code; ignored"
            );
            return;
        };
        if !self.caps_checked {
            self.caps_checked = true;
            self.caps_off(sink);
        }
        let key = Key::from_table(sc);
        if down {
            // A key that is already down can only repeat (Windows autorepeat is a key-down
            // without a key-up). A repeat for a key that isn't held, e.g. after RELEASE_ALL,
            // starts a fresh press.
            if !self.keys.contains(&key) {
                self.keys.push(key);
            }
            sink.post(&[Synth::Key { key, down: true }]);
        } else if let Some(i) = self.keys.iter().position(|&k| k == key) {
            self.keys.remove(i);
            sink.post(&[Synth::Key { key, down: false }]);
        }
    }

    /// Switches Caps Lock off if it is on (a press of the toggle).
    fn caps_off(&mut self, sink: &mut dyn EventSink) {
        if sink.caps_lock() == Some(true) && !self.keys.contains(&Key::Scan(SC_CAPS_LOCK)) {
            let key = Key::Scan(SC_CAPS_LOCK);
            sink.post(&[
                Synth::Key { key, down: true },
                Synth::Key { key, down: false },
            ]);
        }
    }

    /// Release everything held (keys in reverse order, then buttons) and clear Caps Lock.
    fn release_all(&mut self, sink: &mut dyn EventSink) {
        let mut events: Vec<Synth> = std::mem::take(&mut self.keys)
            .into_iter()
            .rev()
            .map(|key| Synth::Key { key, down: false })
            .collect();
        for button in BUTTONS {
            if self.is_down(button) {
                events.push(Synth::Button {
                    button,
                    down: false,
                });
            }
        }
        self.buttons = 0;
        self.wheel = (0, 0);
        if !events.is_empty() {
            sink.post(&events);
        }
        self.caps_off(sink);
        self.caps_checked = true;
    }
}

// ---------------------------------------------------------------------------
// Win32

/// The virtual screen (all displays), physical pixels.
fn virtual_screen() -> Rect {
    // SAFETY: plain metric queries.
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as u32,
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as u32,
        }
    }
}

/// `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` coordinate for virtual-screen pixel `p`:
/// the centre of the pixel's range (Windows maps `n` to pixel `n * len / 65536`).
fn normalise(p: i32, origin: i32, len: u32) -> i32 {
    let offset = i64::from(p) - i64::from(origin);
    let n = (offset * 2 + 1) * 32768 / i64::from(len.max(1));
    n.clamp(0, 65535) as i32
}

struct SendInputSink {
    screen: Rect,
    /// Failed `SendInput` calls (an elevated window or the secure desktop has the focus).
    blocked: u64,
}

impl SendInputSink {
    fn input(&self, synth: &Synth) -> Vec<INPUT> {
        let mouse = |dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS| INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let key = |vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        match *synth {
            Synth::MoveTo { x, y } => vec![mouse(
                normalise(x, self.screen.x, self.screen.w),
                normalise(y, self.screen.y, self.screen.h),
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            )],
            Synth::MoveBy { dx, dy } => vec![mouse(dx, dy, 0, MOUSEEVENTF_MOVE)],
            Synth::Button { button, down } => {
                let (flags, data) = match (button, down) {
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
                    (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
                    (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
                    (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
                };
                vec![mouse(0, 0, data, flags)]
            }
            Synth::Wheel {
                vertical,
                horizontal,
            } => {
                let mut v = Vec::with_capacity(2);
                if vertical != 0 {
                    v.push(mouse(0, 0, vertical as u32, MOUSEEVENTF_WHEEL));
                }
                if horizontal != 0 {
                    v.push(mouse(0, 0, horizontal as u32, MOUSEEVENTF_HWHEEL));
                }
                v
            }
            Synth::Key { key: k, down } => {
                let up = if down {
                    KEYBD_EVENT_FLAGS(0)
                } else {
                    KEYEVENTF_KEYUP
                };
                match k {
                    Key::Scan(sc) => {
                        let ext = if sc & 0xff00 == 0xe000 {
                            KEYEVENTF_EXTENDEDKEY
                        } else {
                            KEYBD_EVENT_FLAGS(0)
                        };
                        vec![key(0, sc & 0xff, KEYEVENTF_SCANCODE | ext | up)]
                    }
                    Key::Virtual { vk, scan } => vec![key(vk, scan, up)],
                }
            }
        }
    }
}

impl SendInputSink {
    /// One `SendInput` call for `inputs`; counts the calls a secure desktop or elevated window blocked.
    fn send(&mut self, inputs: &[INPUT]) {
        if inputs.is_empty() {
            return;
        }
        // SAFETY: a slice of fully initialised INPUTs of the size given.
        let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            self.blocked += 1;
            if self.blocked.is_power_of_two() {
                warn!(
                    sent,
                    of = inputs.len(),
                    total = self.blocked,
                    "SendInput was blocked (an elevated window or the secure desktop has the \
                     focus)"
                );
            }
        }
    }
}

impl EventSink for SendInputSink {
    fn post(&mut self, events: &[Synth]) {
        let inputs: Vec<INPUT> = events.iter().flat_map(|e| self.input(e)).collect();
        self.send(&inputs);
    }

    fn text(&mut self, text: &str) {
        // Each UTF-16 unit (a surrogate pair included) as a down/up pair; the system combines them.
        let mut inputs = Vec::with_capacity(text.len() * 2);
        for unit in text.encode_utf16() {
            for up in [false, true] {
                inputs.push(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(0),
                            wScan: unit,
                            dwFlags: KEYEVENTF_UNICODE
                                | if up {
                                    KEYEVENTF_KEYUP
                                } else {
                                    KEYBD_EVENT_FLAGS(0)
                                },
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
        }
        self.send(&inputs);
    }

    fn caps_lock(&self) -> Option<bool> {
        // SAFETY: plain state query; the low bit is the toggle state.
        Some(unsafe { GetKeyState(i32::from(VK_CAPITAL.0)) } & 1 != 0)
    }

    fn cursor(&self) -> Option<(i32, i32)> {
        let mut p = POINT::default();
        // SAFETY: out pointer to a local.
        unsafe { GetCursorPos(&mut p) }.ok()?;
        Some((p.x, p.y))
    }
}

/// Injects viewer input into the interactive session.
pub struct SendInputInjector {
    state: InputState,
    sink: SendInputSink,
    geometry: Option<Arc<CaptureGeometry>>,
    /// The display the bounds belong to, and when they were read.
    display: u32,
    bounds_read: Instant,
}

impl SendInputInjector {
    pub fn new() -> anyhow::Result<Self> {
        display::ensure_dpi_aware();
        let bounds = display::resolve(0).map_or_else(virtual_screen, |m| m.rect);
        Ok(SendInputInjector {
            state: InputState::new(bounds),
            sink: SendInputSink {
                screen: virtual_screen(),
                blocked: 0,
            },
            geometry: None,
            display: 0,
            bounds_read: Instant::now(),
        })
    }

    /// Map pointer coordinates through the layout the capture publishes to `geometry`.
    pub fn with_capture_geometry(mut self, geometry: Arc<CaptureGeometry>) -> Self {
        self.geometry = Some(geometry);
        self
    }

    /// Follows the running capture's display and layout, and re-reads display bounds.
    fn follow_capture(&mut self) {
        let layout = self.geometry.as_ref().and_then(|g| g.current());
        let display = layout.map_or(self.display, |l| l.display);
        let now = Instant::now();
        if display != self.display || now.duration_since(self.bounds_read) >= BOUNDS_MAX_AGE {
            if let Some(m) = display::resolve(display) {
                self.state.bounds = m.rect;
            }
            self.sink.screen = virtual_screen();
            self.display = display;
            self.bounds_read = now;
        }
        self.state.layout = layout;
    }
}

impl InputInjector for SendInputInjector {
    fn inject(&mut self, ev: InputEvent) {
        if matches!(
            ev,
            InputEvent::MouseAbs { .. } | InputEvent::MouseButton { .. }
        ) {
            self.follow_capture();
        }
        self.state.apply(ev, &mut self.sink);
    }

    fn release_all(&mut self) {
        self.state.release_all(&mut self.sink);
    }
}

impl Drop for SendInputInjector {
    fn drop(&mut self) {
        // Never leave anything held down.
        let mut events: Vec<Synth> = self
            .state
            .keys
            .drain(..)
            .rev()
            .map(|key| Synth::Key { key, down: false })
            .collect();
        for button in BUTTONS {
            if self.state.is_down(button) {
                events.push(Synth::Button {
                    button,
                    down: false,
                });
            }
        }
        if !events.is_empty() {
            self.sink.post(&events);
        }
    }
}

#[cfg(test)]
mod tests {
    use rd_proto::bin::ScrollPhase;
    use rd_proto::keymap::hid_for_code;

    use super::*;

    #[derive(Default)]
    struct Recorder {
        posted: Vec<Vec<Synth>>,
        texts: Vec<String>,
        caps: bool,
        cursor: Option<(i32, i32)>,
    }

    impl EventSink for Recorder {
        fn post(&mut self, events: &[Synth]) {
            for e in events {
                if let Synth::Key {
                    key: Key::Scan(SC_CAPS_LOCK),
                    down: true,
                } = e
                {
                    self.caps = !self.caps;
                }
            }
            self.posted.push(events.to_vec());
        }
        fn text(&mut self, text: &str) {
            self.texts.push(text.to_owned());
        }
        fn caps_lock(&self) -> Option<bool> {
            Some(self.caps)
        }
        fn cursor(&self) -> Option<(i32, i32)> {
            self.cursor
        }
    }

    impl Recorder {
        fn take(&mut self) -> Vec<Synth> {
            std::mem::take(&mut self.posted).concat()
        }
    }

    const PRIMARY: Rect = Rect {
        x: 0,
        y: 0,
        w: 2560,
        h: 1440,
    };

    fn key(code: &str, down: bool) -> InputEvent {
        InputEvent::Key {
            down,
            repeat: false,
            hid: hid_for_code(code).unwrap(),
        }
    }

    #[test]
    fn text_goes_to_the_sink_and_not_the_key_path() {
        let mut state = InputState::new(PRIMARY);
        let mut r = Recorder::default();
        state.apply(InputEvent::Text("héllo 👋".into()), &mut r);
        assert_eq!(r.texts, vec!["héllo 👋".to_string()]);
        assert!(r.posted.is_empty());
    }

    #[test]
    fn abs_coordinates_map_onto_the_display() {
        let mut s = InputState::new(PRIMARY);
        let mut r = Recorder::default();
        s.apply(
            InputEvent::MouseAbs {
                buttons: 0,
                x: 0,
                y: 0,
            },
            &mut r,
        );
        s.apply(
            InputEvent::MouseAbs {
                buttons: 0,
                x: 65535,
                y: 65535,
            },
            &mut r,
        );
        s.apply(
            InputEvent::MouseAbs {
                buttons: 0,
                x: 32768,
                y: 32768,
            },
            &mut r,
        );
        assert_eq!(
            r.take(),
            [
                Synth::MoveTo { x: 0, y: 0 },
                Synth::MoveTo { x: 2559, y: 1439 },
                Synth::MoveTo { x: 1280, y: 720 },
            ]
        );
    }

    #[test]
    fn abs_coordinates_follow_the_letterbox_and_secondary_displays() {
        let left = Rect {
            x: -1920,
            y: 0,
            w: 1920,
            h: 1080,
        };
        let mut s = InputState::new(left);
        // 16:9 display in a 16:10 frame: bars top and bottom.
        s.layout = Some(CaptureLayout {
            display: 2,
            width: 1728,
            height: 1080,
            stretch: false,
        });
        let mut r = Recorder::default();
        // The top bar maps onto the display's top edge.
        s.apply(
            InputEvent::MouseAbs {
                buttons: 0,
                x: 32768,
                y: 100,
            },
            &mut r,
        );
        // The frame's centre is the display's centre.
        s.apply(
            InputEvent::MouseAbs {
                buttons: 0,
                x: 32768,
                y: 32768,
            },
            &mut r,
        );
        assert_eq!(
            r.take(),
            [
                Synth::MoveTo { x: -960, y: 0 },
                Synth::MoveTo { x: -960, y: 540 },
            ]
        );
    }

    #[test]
    fn buttons_move_first_and_only_toggle_once() {
        let mut s = InputState::new(PRIMARY);
        let mut r = Recorder::default();
        let btn = |down, x| InputEvent::MouseButton {
            button: MouseButton::Left,
            down,
            x,
            y: 0,
        };
        s.apply(btn(true, 32768), &mut r);
        s.apply(btn(true, 32768), &mut r); // duplicate down: nothing
        s.apply(btn(false, 32768), &mut r);
        assert_eq!(
            r.take(),
            [
                Synth::MoveTo { x: 1280, y: 0 },
                Synth::Button {
                    button: MouseButton::Left,
                    down: true
                },
                Synth::Button {
                    button: MouseButton::Left,
                    down: false
                },
            ]
        );
    }

    #[test]
    fn relative_motion_and_buttons_where_the_pointer_is() {
        let mut s = InputState::new(PRIMARY);
        let mut r = Recorder {
            cursor: Some((100, 100)),
            ..Default::default()
        };
        s.apply(InputEvent::MouseRel { dx: 5, dy: -3 }, &mut r);
        s.apply(
            InputEvent::MouseButton {
                button: MouseButton::Back,
                down: true,
                x: 0,
                y: 0,
            },
            &mut r,
        );
        assert_eq!(
            r.take(),
            [
                Synth::MoveBy { dx: 5, dy: -3 },
                Synth::Button {
                    button: MouseButton::Back,
                    down: true
                },
            ]
        );
    }

    #[test]
    fn wheel_converts_dom_signs_and_units_with_remainders() {
        let mut s = InputState::new(PRIMARY);
        let mut r = Recorder::default();
        let wheel = |mode, dx, dy| InputEvent::Wheel {
            mode,
            dx,
            dy,
            phase: ScrollPhase::None,
        };
        // One notch down (three lines): -120.
        s.apply(wheel(WheelMode::Line, 0, 3), &mut r);
        // Trackpad: 50 px up = +60; 1 px right = 1.2, then 4 more = 6 in total.
        s.apply(wheel(WheelMode::Pixel, 0, -50), &mut r);
        s.apply(wheel(WheelMode::Pixel, 1, 0), &mut r);
        s.apply(wheel(WheelMode::Pixel, 4, 0), &mut r);
        // Less than a unit: nothing yet.
        s.apply(
            InputEvent::Wheel {
                mode: WheelMode::Pixel,
                dx: 0,
                dy: 0,
                phase: ScrollPhase::Ended,
            },
            &mut r,
        );
        assert_eq!(
            r.take(),
            [
                Synth::Wheel {
                    vertical: -120,
                    horizontal: 0
                },
                Synth::Wheel {
                    vertical: 60,
                    horizontal: 0
                },
                Synth::Wheel {
                    vertical: 0,
                    horizontal: 1
                },
                Synth::Wheel {
                    vertical: 0,
                    horizontal: 5
                },
            ]
        );
    }

    #[test]
    fn keys_are_scan_codes_with_repeats() {
        let mut s = InputState::new(PRIMARY);
        let mut r = Recorder::default();
        s.apply(key("KeyA", true), &mut r);
        s.apply(key("KeyA", true), &mut r); // autorepeat
        s.apply(key("KeyA", false), &mut r);
        s.apply(key("KeyA", false), &mut r); // not held: nothing
        s.apply(key("ArrowUp", true), &mut r);
        let a = Key::Scan(0x1e);
        assert_eq!(
            r.take(),
            [
                Synth::Key { key: a, down: true },
                Synth::Key { key: a, down: true },
                Synth::Key {
                    key: a,
                    down: false
                },
                Synth::Key {
                    key: Key::Scan(0xe048),
                    down: true
                },
            ]
        );
    }

    #[test]
    fn pause_and_num_lock_are_injected_differently_from_the_table() {
        let mut s = InputState::new(PRIMARY);
        let mut r = Recorder::default();
        s.apply(key("Pause", true), &mut r);
        s.apply(key("NumLock", true), &mut r);
        assert_eq!(
            r.take(),
            [
                Synth::Key {
                    key: Key::Virtual {
                        vk: VK_PAUSE.0,
                        scan: 0x45
                    },
                    down: true
                },
                Synth::Key {
                    key: Key::Scan(0x45),
                    down: true
                },
            ]
        );
    }

    #[test]
    fn caps_lock_starts_off_and_is_cleared_by_release_all() {
        let mut s = InputState::new(PRIMARY);
        let mut r = Recorder {
            caps: true,
            ..Default::default()
        };
        s.apply(key("KeyA", true), &mut r);
        assert!(!r.caps, "switched off before the first key");
        // The viewer turns it on (a press per toggle)...
        s.apply(key("CapsLock", true), &mut r);
        s.apply(key("CapsLock", false), &mut r);
        assert!(r.caps);
        r.take();
        // ...and RELEASE_ALL turns it off again, after releasing KeyA.
        s.apply(InputEvent::ReleaseAll, &mut r);
        let caps = Key::Scan(SC_CAPS_LOCK);
        assert_eq!(
            r.take(),
            [
                Synth::Key {
                    key: Key::Scan(0x1e),
                    down: false
                },
                Synth::Key {
                    key: caps,
                    down: true
                },
                Synth::Key {
                    key: caps,
                    down: false
                },
            ]
        );
        assert!(!r.caps);
    }

    #[test]
    fn release_all_releases_keys_in_reverse_then_buttons() {
        let mut s = InputState::new(PRIMARY);
        let mut r = Recorder::default();
        s.apply(key("ControlLeft", true), &mut r);
        s.apply(key("KeyC", true), &mut r);
        s.apply(
            InputEvent::MouseButton {
                button: MouseButton::Right,
                down: true,
                x: 0,
                y: 0,
            },
            &mut r,
        );
        r.take();
        s.apply(InputEvent::ReleaseAll, &mut r);
        assert_eq!(
            r.take(),
            [
                Synth::Key {
                    key: Key::Scan(0x2e),
                    down: false
                },
                Synth::Key {
                    key: Key::Scan(0x1d),
                    down: false
                },
                Synth::Button {
                    button: MouseButton::Right,
                    down: false
                },
            ]
        );
        // Nothing left to release.
        s.apply(InputEvent::ReleaseAll, &mut r);
        assert!(r.take().is_empty());
    }

    #[test]
    fn media_keys_without_scan_codes_are_ignored() {
        let mut s = InputState::new(PRIMARY);
        let mut r = Recorder::default();
        s.apply(key("MediaPlay", true), &mut r);
        assert!(r.take().is_empty());
    }

    #[test]
    fn normalised_coordinates_hit_pixel_centres() {
        // Windows maps n back to pixel n * len / 65536.
        for (len, origin) in [(2560u32, 0i32), (4480, -1920), (1, 0)] {
            for p in [0, 1, len as i32 / 2, len as i32 - 1]
                .into_iter()
                .filter(|&p| p < len as i32)
            {
                let px = p + origin;
                let n = normalise(px, origin, len);
                assert_eq!(
                    i64::from(n) * i64::from(len) / 65536,
                    i64::from(p),
                    "{len} {p}"
                );
            }
        }
    }

    /// Moves the real pointer (and puts it back): absolute moves land on the exact pixel on
    /// every display. Opt-in, it touches the desktop: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn absolute_moves_land_on_exact_pixels() {
        display::ensure_dpi_aware();
        let mut sink = SendInputSink {
            screen: virtual_screen(),
            blocked: 0,
        };
        let home = sink.cursor().unwrap();
        for m in display::monitors() {
            let Rect { x, y, w, h } = m.rect;
            for (px, py) in [
                (x, y),
                (x + w as i32 - 1, y + h as i32 - 1),
                (x + 123, y + 45),
            ] {
                sink.post(&[Synth::MoveTo { x: px, y: py }]);
                assert_eq!(sink.cursor(), Some((px, py)), "{}", m.device);
            }
        }
        sink.post(&[Synth::MoveTo {
            x: home.0,
            y: home.1,
        }]);
        assert_eq!(sink.blocked, 0);
    }

    #[test]
    fn sendinput_translation() {
        let sink = SendInputSink {
            screen: Rect {
                x: -1920,
                y: 0,
                w: 4480,
                h: 1440,
            },
            blocked: 0,
        };
        // SAFETY (reads below): the union member matching each INPUT's type.
        let key = sink.input(&Synth::Key {
            key: Key::Scan(0xe05b),
            down: false,
        });
        let ki = unsafe { key[0].Anonymous.ki };
        assert_eq!(ki.wScan, 0x5b);
        assert_eq!(
            ki.dwFlags,
            KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
        );
        let wheel = sink.input(&Synth::Wheel {
            vertical: -120,
            horizontal: 0,
        });
        assert_eq!(wheel.len(), 1);
        let mi = unsafe { wheel[0].Anonymous.mi };
        assert_eq!(mi.mouseData as i32, -120);
        assert_eq!(mi.dwFlags, MOUSEEVENTF_WHEEL);
        let back = sink.input(&Synth::Button {
            button: MouseButton::Back,
            down: true,
        });
        let mi = unsafe { back[0].Anonymous.mi };
        assert_eq!((mi.dwFlags, mi.mouseData), (MOUSEEVENTF_XDOWN, XBUTTON1));
        // The primary display's origin is 1920 px into this virtual screen.
        let mv = sink.input(&Synth::MoveTo { x: 0, y: 0 });
        let mi = unsafe { mv[0].Anonymous.mi };
        assert_eq!(i64::from(mi.dx) * 4480 / 65536, 1920);
    }
}
