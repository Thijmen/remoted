//! CGEvent input injection.
//!
//! [`CgInjector`] turns viewer [`InputEvent`]s into Quartz events and posts them at the HID
//! event tap from a `HIDSystemState` event source, so they behave like hardware input: they
//! move the real cursor, update the system-wide modifier state and reach every app.
//!
//! All translation (coordinates, drag and click state, modifier flags, held keys and buttons)
//! is pure and lives in `InputState`. It describes each event as a `Synth` and hands it to an
//! `EventSink`. Only `CgSink` touches Quartz. Tests record into a sink instead, so they never
//! post anything, and [`CgInjector::dry_run`] logs instead of posting.
//!
//! Conventions:
//! - MOUSE_ABS / MOUSE_BTN `x`, `y` are 0..=65535 across the video frame. With a
//!   [`CaptureGeometry`] they map through the rect the captured display occupies in that frame
//!   (letterbox aware, see [`crate::geometry`]) onto its `CGDisplayBounds` (global points,
//!   origin top-left), clamped to the display. Without one the frame is the whole display.
//! - WHEEL deltas use DOM signs: `dy > 0` is `deltaY > 0` (scroll down, content moves up) and
//!   `dx > 0` scrolls right. Quartz is the other way round (`wheel1 > 0` scrolls up,
//!   `wheel2 > 0` scrolls left), so both axes are negated.
//! - KEY carries HID usages, mapped to `kVK_*` codes with [`keymap::mac_keycode`]. Keys without a
//!   macOS keycode (most media keys) are ignored.

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use objc2::msg_send;
use objc2::runtime::AnyClass;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayChangeSummaryFlags,
    CGDisplayRegisterReconfigurationCallback, CGError, CGEvent, CGEventField, CGEventFlags,
    CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType, CGKeyCode,
    CGMainDisplayID, CGMouseButton, CGPreflightPostEventAccess, CGScrollEventUnit, CGScrollPhase,
};
use rd_agent_core::{InputEvent, InputInjector};
use rd_proto::bin::{MouseButton, ScrollPhase, WheelMode};
use rd_proto::keymap;
use tracing::{debug, info, warn};

use crate::geometry::{self, CaptureGeometry, CaptureLayout};

/// Largest normalised MOUSE_ABS / MOUSE_BTN coordinate.
const NORM_MAX: f64 = 65535.0;
/// Clicks further apart than this (points) do not count as a double/triple click.
const CLICK_RADIUS: f64 = 4.0;
/// Click state counts 1 -> 2 -> 3 and then stays at 3 (triple click = select line/paragraph).
const MAX_CLICK_STATE: i64 = 3;
/// macOS's default, used if `NSEvent.doubleClickInterval` is unavailable.
const DEFAULT_DOUBLE_CLICK: Duration = Duration::from_millis(500);
/// Display bounds are re-read at least this often, in case a reconfiguration callback is
/// never delivered (Quartz delivers them on the main run loop).
const BOUNDS_MAX_AGE: Duration = Duration::from_secs(2);

/// `kVK_*` virtual keycodes (HIToolbox `Events.h`) that need special handling.
mod kvk {
    use objc2_core_graphics::CGKeyCode;

    pub const COMMAND: CGKeyCode = 0x37;
    pub const RIGHT_COMMAND: CGKeyCode = 0x36;
    pub const SHIFT: CGKeyCode = 0x38;
    pub const RIGHT_SHIFT: CGKeyCode = 0x3c;
    pub const OPTION: CGKeyCode = 0x3a;
    pub const RIGHT_OPTION: CGKeyCode = 0x3d;
    pub const CONTROL: CGKeyCode = 0x3b;
    pub const RIGHT_CONTROL: CGKeyCode = 0x3e;
    pub const FUNCTION: CGKeyCode = 0x3f;
    pub const CAPS_LOCK: CGKeyCode = 0x39;

    pub const HELP: CGKeyCode = 0x72;
    pub const HOME: CGKeyCode = 0x73;
    pub const PAGE_UP: CGKeyCode = 0x74;
    pub const FORWARD_DELETE: CGKeyCode = 0x75;
    pub const END: CGKeyCode = 0x77;
    pub const PAGE_DOWN: CGKeyCode = 0x79;
    pub const LEFT_ARROW: CGKeyCode = 0x7b;
    pub const RIGHT_ARROW: CGKeyCode = 0x7c;
    pub const DOWN_ARROW: CGKeyCode = 0x7d;
    pub const UP_ARROW: CGKeyCode = 0x7e;

    /// F1 ..= F20.
    pub const FUNCTION_KEYS: &[CGKeyCode] = &[
        0x7a, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6d, 0x67, 0x6f, 0x69, 0x6b, 0x71,
        0x6a, 0x40, 0x4f, 0x50, 0x5a,
    ];
    /// Keypad: decimal, multiply, plus, clear, divide, enter, minus, equals, 0 ..= 9, JIS comma.
    pub const KEYPAD: &[CGKeyCode] = &[
        0x41, 0x43, 0x45, 0x47, 0x4b, 0x4c, 0x4e, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58,
        0x59, 0x5b, 0x5c, 0x5f,
    ];
}

/// A modifier key. It is posted as `FlagsChanged` and contributes `flags` to every event while
/// it is held.
struct Modifier {
    keycode: CGKeyCode,
    flags: CGEventFlags,
}

/// The device-independent mask plus the device-dependent left/right bit (IOKit
/// `NX_DEVICE*KEYMASK`) that real keyboards set, so apps can tell left from right.
const fn modifier(keycode: CGKeyCode, mask: CGEventFlags, device_bit: u64) -> Modifier {
    Modifier {
        keycode,
        flags: CGEventFlags(mask.bits() | device_bit),
    }
}

const MODIFIERS: [Modifier; 9] = [
    modifier(kvk::CONTROL, CGEventFlags::MaskControl, 0x0001),
    modifier(kvk::SHIFT, CGEventFlags::MaskShift, 0x0002),
    modifier(kvk::RIGHT_SHIFT, CGEventFlags::MaskShift, 0x0004),
    modifier(kvk::COMMAND, CGEventFlags::MaskCommand, 0x0008),
    modifier(kvk::RIGHT_COMMAND, CGEventFlags::MaskCommand, 0x0010),
    modifier(kvk::OPTION, CGEventFlags::MaskAlternate, 0x0020),
    modifier(kvk::RIGHT_OPTION, CGEventFlags::MaskAlternate, 0x0040),
    modifier(kvk::RIGHT_CONTROL, CGEventFlags::MaskControl, 0x2000),
    modifier(kvk::FUNCTION, CGEventFlags::MaskSecondaryFn, 0),
];

/// Flags macOS puts on keys by where they sit: `NumericPad` on the keypad and arrows,
/// `SecondaryFn` on keys that live on the Fn layer of Apple keyboards (F-keys, arrows,
/// navigation keys, forward delete, help).
fn key_class_flags(keycode: CGKeyCode) -> CGEventFlags {
    let arrow = matches!(
        keycode,
        kvk::LEFT_ARROW | kvk::RIGHT_ARROW | kvk::DOWN_ARROW | kvk::UP_ARROW
    );
    let navigation = matches!(
        keycode,
        kvk::HOME | kvk::END | kvk::PAGE_UP | kvk::PAGE_DOWN | kvk::FORWARD_DELETE | kvk::HELP
    );
    let mut flags = CGEventFlags::empty();
    if arrow || kvk::KEYPAD.contains(&keycode) {
        flags |= CGEventFlags::MaskNumericPad;
    }
    if arrow || navigation || kvk::FUNCTION_KEYS.contains(&keycode) {
        flags |= CGEventFlags::MaskSecondaryFn;
    }
    flags
}

/// Buttons in drag-priority order (a drag with several buttons held reports the first).
const BUTTONS: [MouseButton; 5] = [
    MouseButton::Left,
    MouseButton::Right,
    MouseButton::Middle,
    MouseButton::Back,
    MouseButton::Forward,
];

/// `kCGMouseEventButtonNumber`: 0 left, 1 right, 2 middle, 3 back, 4 forward.
fn button_number(button: MouseButton) -> CGMouseButton {
    match button {
        MouseButton::Left => CGMouseButton::Left,
        MouseButton::Right => CGMouseButton::Right,
        MouseButton::Middle => CGMouseButton::Center,
        MouseButton::Back => CGMouseButton(3),
        MouseButton::Forward => CGMouseButton(4),
    }
}

struct ButtonTypes {
    down: CGEventType,
    up: CGEventType,
    dragged: CGEventType,
}

fn button_types(button: MouseButton) -> ButtonTypes {
    match button {
        MouseButton::Left => ButtonTypes {
            down: CGEventType::LeftMouseDown,
            up: CGEventType::LeftMouseUp,
            dragged: CGEventType::LeftMouseDragged,
        },
        MouseButton::Right => ButtonTypes {
            down: CGEventType::RightMouseDown,
            up: CGEventType::RightMouseUp,
            dragged: CGEventType::RightMouseDragged,
        },
        MouseButton::Middle | MouseButton::Back | MouseButton::Forward => ButtonTypes {
            down: CGEventType::OtherMouseDown,
            up: CGEventType::OtherMouseUp,
            dragged: CGEventType::OtherMouseDragged,
        },
    }
}

fn scroll_phase(phase: ScrollPhase) -> Option<CGScrollPhase> {
    match phase {
        ScrollPhase::None => None,
        ScrollPhase::Began => Some(CGScrollPhase::Began),
        ScrollPhase::Changed => Some(CGScrollPhase::Changed),
        ScrollPhase::Ended => Some(CGScrollPhase::Ended),
    }
}

/// One Quartz event, fully described. [`build_event`] turns it into a `CGEvent`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Synth {
    Mouse {
        ty: CGEventType,
        button: CGMouseButton,
        pos: CGPoint,
        /// `kCGMouseEventClickState`, set on button down/up only.
        click_state: Option<i64>,
        /// `kCGMouseEventDeltaX/Y`, set for relative motion.
        delta: Option<(i64, i64)>,
        flags: CGEventFlags,
    },
    Key {
        keycode: CGKeyCode,
        down: bool,
        autorepeat: bool,
        flags: CGEventFlags,
    },
    FlagsChanged {
        keycode: CGKeyCode,
        flags: CGEventFlags,
    },
    Scroll {
        unit: CGScrollEventUnit,
        /// Vertical, positive = scroll up.
        wheel1: i32,
        /// Horizontal, positive = scroll left.
        wheel2: i32,
        continuous: bool,
        phase: Option<CGScrollPhase>,
        flags: CGEventFlags,
    },
}

/// Where synthesized events go.
trait EventSink: Send {
    fn post(&mut self, event: &Synth);
    /// The current global cursor position, if this sink can tell.
    fn cursor_location(&self) -> Option<CGPoint>;
}

#[derive(Debug, Clone, Copy)]
struct Click {
    button: MouseButton,
    at: Instant,
    pos: CGPoint,
    count: i64,
}

/// Input translation state: what is held, where the pointer is, which modifiers apply.
struct InputState {
    /// Target display, global points.
    bounds: CGRect,
    /// How the display sits in the video frame; `None` = it fills the frame.
    layout: Option<CaptureLayout>,
    double_click: Duration,
    /// Last posted pointer position.
    pos: CGPoint,
    /// The last motion was MOUSE_REL (pointer lock). The cursor is then driven by deltas, so
    /// button events act where the cursor is, not at the viewer's stale absolute position.
    relative: bool,
    /// Held buttons, as [`MouseButton::mask`] bits.
    buttons: u8,
    last_click: Option<Click>,
    /// Held non-modifier keys, in press order.
    keys: Vec<CGKeyCode>,
    /// Held modifiers: bit `i` is `MODIFIERS[i]`.
    modifiers: u16,
    caps_lock: bool,
    caps_lock_down: bool,
}

impl InputState {
    fn new(bounds: CGRect, double_click: Duration, pos: CGPoint) -> Self {
        Self {
            bounds,
            layout: None,
            double_click,
            pos,
            relative: false,
            buttons: 0,
            last_click: None,
            keys: Vec::new(),
            modifiers: 0,
            caps_lock: false,
            caps_lock_down: false,
        }
    }

    fn apply(&mut self, ev: InputEvent, now: Instant, sink: &mut dyn EventSink) {
        match ev {
            // `buttons` mirrors the viewer's view. Drags follow the buttons actually pressed
            // here, so the host never sees a drag without its mouse-down.
            InputEvent::MouseAbs { x, y, .. } => {
                self.relative = false;
                self.pos = self.map_abs(x, y);
                self.post_motion(None, sink);
            }
            InputEvent::MouseRel { dx, dy } => {
                let from = sink.cursor_location().unwrap_or(self.pos);
                self.relative = true;
                self.pos = self.clamp(CGPoint::new(from.x + f64::from(dx), from.y + f64::from(dy)));
                self.post_motion(Some((i64::from(dx), i64::from(dy))), sink);
            }
            InputEvent::MouseButton { button, down, x, y } => {
                self.pos = if self.relative {
                    self.clamp(sink.cursor_location().unwrap_or(self.pos))
                } else {
                    self.map_abs(x, y)
                };
                if down {
                    self.button_down(button, now, sink);
                } else {
                    self.button_up(button, sink);
                }
            }
            InputEvent::Wheel {
                mode,
                dx,
                dy,
                phase,
            } => {
                if let Some(event) = self.scroll(mode, dx, dy, phase) {
                    sink.post(&event);
                }
            }
            InputEvent::Key { down, hid, .. } => self.key(hid, down, sink),
            InputEvent::ReleaseAll => self.release_all(sink),
        }
    }

    /// Release everything held (keys, then buttons, then modifiers) and clear Caps Lock.
    fn release_all(&mut self, sink: &mut dyn EventSink) {
        for keycode in std::mem::take(&mut self.keys).into_iter().rev() {
            sink.post(&self.key_event(keycode, false, false));
        }
        for button in BUTTONS {
            self.button_up(button, sink);
        }
        self.last_click = None;
        for (i, m) in MODIFIERS.iter().enumerate() {
            let bit = 1 << i;
            if self.modifiers & bit != 0 {
                self.modifiers &= !bit;
                sink.post(&self.flags_changed(m.keycode));
            }
        }
        if self.caps_lock {
            self.caps_lock = false;
            sink.post(&self.flags_changed(kvk::CAPS_LOCK));
        }
        self.caps_lock_down = false;
    }

    /// Modifier flags currently in effect.
    fn flags(&self) -> CGEventFlags {
        let mut flags = if self.caps_lock {
            CGEventFlags::MaskAlphaShift
        } else {
            CGEventFlags::empty()
        };
        for (i, m) in MODIFIERS.iter().enumerate() {
            if self.modifiers & (1 << i) != 0 {
                flags |= m.flags;
            }
        }
        flags
    }

    fn map_abs(&self, x: u16, y: u16) -> CGPoint {
        let (u, v) = (f64::from(x) / NORM_MAX, f64::from(y) / NORM_MAX);
        // A point on a letterbox bar lands on the nearest display edge.
        self.clamp(geometry::frame_to_display(
            u,
            v,
            self.layout.as_ref(),
            self.bounds,
        ))
    }

    /// Clamp onto the display; the last addressable point is one point inside the far edge.
    fn clamp(&self, p: CGPoint) -> CGPoint {
        let CGRect { origin, size } = self.bounds;
        CGPoint::new(
            p.x.clamp(origin.x, origin.x + (size.width - 1.0).max(0.0)),
            p.y.clamp(origin.y, origin.y + (size.height - 1.0).max(0.0)),
        )
    }

    fn is_down(&self, button: MouseButton) -> bool {
        self.buttons & button.mask() != 0
    }

    fn post_motion(&self, delta: Option<(i64, i64)>, sink: &mut dyn EventSink) {
        let (ty, button) = match BUTTONS.into_iter().find(|&b| self.is_down(b)) {
            Some(b) => (button_types(b).dragged, button_number(b)),
            None => (CGEventType::MouseMoved, CGMouseButton::Left),
        };
        sink.post(&Synth::Mouse {
            ty,
            button,
            pos: self.pos,
            click_state: None,
            delta,
            flags: self.flags(),
        });
    }

    fn button_down(&mut self, button: MouseButton, now: Instant, sink: &mut dyn EventSink) {
        if self.is_down(button) {
            return;
        }
        self.buttons |= button.mask();
        let count = match self.last_click {
            Some(c)
                if c.button == button
                    && now.saturating_duration_since(c.at) <= self.double_click
                    && (c.pos.x - self.pos.x).hypot(c.pos.y - self.pos.y) <= CLICK_RADIUS =>
            {
                (c.count + 1).min(MAX_CLICK_STATE)
            }
            _ => 1,
        };
        self.last_click = Some(Click {
            button,
            at: now,
            pos: self.pos,
            count,
        });
        sink.post(&self.button_event(button_types(button).down, button, count));
    }

    fn button_up(&mut self, button: MouseButton, sink: &mut dyn EventSink) {
        if !self.is_down(button) {
            return;
        }
        self.buttons &= !button.mask();
        // The up carries the click state of its down.
        let count = self
            .last_click
            .filter(|c| c.button == button)
            .map_or(1, |c| c.count);
        sink.post(&self.button_event(button_types(button).up, button, count));
    }

    fn button_event(&self, ty: CGEventType, button: MouseButton, click_state: i64) -> Synth {
        Synth::Mouse {
            ty,
            button: button_number(button),
            pos: self.pos,
            click_state: Some(click_state),
            delta: None,
            flags: self.flags(),
        }
    }

    fn scroll(&self, mode: WheelMode, dx: i16, dy: i16, phase: ScrollPhase) -> Option<Synth> {
        // DOM signs on the wire, Quartz signs on the event (see the module docs).
        let wheel1 = -i32::from(dy);
        let wheel2 = -i32::from(dx);
        let (unit, continuous, phase) = match mode {
            WheelMode::Pixel => (CGScrollEventUnit::Pixel, true, scroll_phase(phase)),
            // Discrete wheel notches have no gesture phases.
            WheelMode::Line => (CGScrollEventUnit::Line, false, None),
        };
        // A gesture phase (e.g. the final "ended") matters even without movement.
        if wheel1 == 0 && wheel2 == 0 && phase.is_none() {
            return None;
        }
        Some(Synth::Scroll {
            unit,
            wheel1,
            wheel2,
            continuous,
            phase,
            flags: self.flags(),
        })
    }

    fn key(&mut self, hid: u32, down: bool, sink: &mut dyn EventSink) {
        let Some(keycode) = keymap::mac_keycode(hid) else {
            debug!(
                hid = format_args!("{hid:#010x}"),
                "key has no macOS keycode; ignored"
            );
            return;
        };
        if keycode == kvk::CAPS_LOCK {
            // A toggle: each press flips the lock and posts one FlagsChanged; releases post
            // nothing. The viewer sends a down and an up for every physical press.
            if down && !self.caps_lock_down {
                self.caps_lock = !self.caps_lock;
                sink.post(&self.flags_changed(keycode));
            }
            self.caps_lock_down = down;
        } else if let Some(i) = MODIFIERS.iter().position(|m| m.keycode == keycode) {
            let bit = 1 << i;
            // Ignore repeats, duplicate downs and releases of modifiers that aren't held.
            if (self.modifiers & bit != 0) != down {
                self.modifiers ^= bit;
                sink.post(&self.flags_changed(keycode));
            }
        } else if down {
            // A key that is already down can only repeat (this covers the viewer's repeat
            // flag). A repeat for a key that isn't held, e.g. after RELEASE_ALL, starts a fresh
            // press so the target sees a proper key-down first.
            let autorepeat = self.keys.contains(&keycode);
            if !autorepeat {
                self.keys.push(keycode);
            }
            sink.post(&self.key_event(keycode, true, autorepeat));
        } else if let Some(i) = self.keys.iter().position(|&k| k == keycode) {
            self.keys.remove(i);
            sink.post(&self.key_event(keycode, false, false));
        }
    }

    fn key_event(&self, keycode: CGKeyCode, down: bool, autorepeat: bool) -> Synth {
        Synth::Key {
            keycode,
            down,
            autorepeat,
            // Hardware key events always carry NonCoalesced.
            flags: self.flags() | key_class_flags(keycode) | CGEventFlags::MaskNonCoalesced,
        }
    }

    fn flags_changed(&self, keycode: CGKeyCode) -> Synth {
        Synth::FlagsChanged {
            keycode,
            flags: self.flags() | CGEventFlags::MaskNonCoalesced,
        }
    }
}

/// Create the `CGEvent` for `synth` (without posting it).
fn build_event(source: Option<&CGEventSource>, synth: &Synth) -> Option<CFRetained<CGEvent>> {
    let set = |event: &CGEvent, field: CGEventField, value: i64| {
        CGEvent::set_integer_value_field(Some(event), field, value);
    };
    let (event, flags) = match *synth {
        Synth::Mouse {
            ty,
            button,
            pos,
            click_state,
            delta,
            flags,
        } => {
            let event = CGEvent::new_mouse_event(source, ty, pos, button)?;
            set(
                &event,
                CGEventField::MouseEventButtonNumber,
                i64::from(button.0),
            );
            if let Some(clicks) = click_state {
                set(&event, CGEventField::MouseEventClickState, clicks);
            }
            if let Some((dx, dy)) = delta {
                set(&event, CGEventField::MouseEventDeltaX, dx);
                set(&event, CGEventField::MouseEventDeltaY, dy);
            }
            (event, flags)
        }
        Synth::Key {
            keycode,
            down,
            autorepeat,
            flags,
        } => {
            let event = CGEvent::new_keyboard_event(source, keycode, down)?;
            if autorepeat {
                set(&event, CGEventField::KeyboardEventAutorepeat, 1);
            }
            (event, flags)
        }
        Synth::FlagsChanged { keycode, flags } => {
            let event = CGEvent::new_keyboard_event(source, keycode, true)?;
            CGEvent::set_type(Some(&event), CGEventType::FlagsChanged);
            (event, flags)
        }
        Synth::Scroll {
            unit,
            wheel1,
            wheel2,
            continuous,
            phase,
            flags,
        } => {
            let event = CGEvent::new_scroll_wheel_event2(source, unit, 2, wheel1, wheel2, 0)?;
            if continuous {
                set(&event, CGEventField::ScrollWheelEventIsContinuous, 1);
            }
            if let Some(phase) = phase {
                set(
                    &event,
                    CGEventField::ScrollWheelEventScrollPhase,
                    i64::from(phase.0),
                );
            }
            (event, flags)
        }
    };
    CGEvent::set_flags(Some(&event), flags);
    Some(event)
}

/// The HID-system event source.
struct EventSource(CFRetained<CGEventSource>);

// SAFETY: CoreFoundation objects may be retained, released and used from any thread as long as
// they are not used concurrently. The source is owned by exactly one sink, which only uses it
// from whichever thread currently owns the injector.
unsafe impl Send for EventSource {}

/// Posts to the HID event tap.
struct CgSink {
    source: EventSource,
}

impl CgSink {
    fn new() -> anyhow::Result<Self> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .context("CGEventSourceCreate(HIDSystemState) failed")?;
        // Don't suppress the local keyboard and mouse after every injected event.
        CGEventSource::set_local_events_suppression_interval(Some(&source), 0.0);
        Ok(Self {
            source: EventSource(source),
        })
    }
}

impl EventSink for CgSink {
    fn post(&mut self, synth: &Synth) {
        match build_event(Some(&self.source.0), synth) {
            Some(event) => CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event)),
            None => warn!(event = ?synth, "could not create CGEvent"),
        }
    }

    fn cursor_location(&self) -> Option<CGPoint> {
        CGEvent::new(None).map(|event| CGEvent::location(Some(&event)))
    }
}

/// Logs instead of posting.
struct LogSink;

impl EventSink for LogSink {
    fn post(&mut self, synth: &Synth) {
        debug!(event = ?synth, "input (dry run)");
    }

    fn cursor_location(&self) -> Option<CGPoint> {
        None
    }
}

/// Bumped by the display reconfiguration callback; injectors re-read their bounds when it
/// changes.
static RECONFIGURATIONS: AtomicU64 = AtomicU64::new(0);

extern "C-unwind" fn on_reconfiguration(
    _display: CGDirectDisplayID,
    flags: CGDisplayChangeSummaryFlags,
    _user_info: *mut c_void,
) {
    // The "begin" notification arrives before anything has changed.
    if !flags.contains(CGDisplayChangeSummaryFlags::BeginConfigurationFlag) {
        RECONFIGURATIONS.fetch_add(1, Ordering::Release);
    }
}

/// Register [`on_reconfiguration`] once for the life of the process.
fn watch_reconfigurations() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        // SAFETY: the callback has the required signature, only touches a static and takes no
        // user info. It is never removed, so it can't outlive anything it uses.
        let err = unsafe {
            CGDisplayRegisterReconfigurationCallback(Some(on_reconfiguration), ptr::null_mut())
        };
        if err != CGError::Success {
            warn!(?err, "CGDisplayRegisterReconfigurationCallback failed");
        }
    });
}

/// Bounds of a display in global points, `None` if it is gone.
fn display_bounds(display: CGDirectDisplayID) -> Option<CGRect> {
    let bounds = CGDisplayBounds(display);
    (bounds.size.width > 0.0 && bounds.size.height > 0.0).then_some(bounds)
}

// `NSEvent` lives in AppKit; link it so the class is registered with the runtime.
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

/// The user's double-click interval (`NSEvent.doubleClickInterval`).
fn double_click_interval() -> Duration {
    let Some(class) = AnyClass::get(c"NSEvent") else {
        return DEFAULT_DOUBLE_CLICK;
    };
    // SAFETY: `+[NSEvent doubleClickInterval]` takes no arguments, returns an NSTimeInterval
    // (double) and may be called from any thread.
    let seconds: f64 = unsafe { msg_send![class, doubleClickInterval] };
    Duration::try_from_secs_f64(seconds)
        .ok()
        .filter(|interval| !interval.is_zero())
        .unwrap_or(DEFAULT_DOUBLE_CLICK)
}

/// The display MOUSE_ABS coordinates map onto.
struct DisplayTarget {
    id: CGDirectDisplayID,
    /// [`RECONFIGURATIONS`] and time at the last read; `None` = never read.
    read: Option<(u64, Instant)>,
}

/// Injects viewer input with Quartz events (see the module docs).
pub struct CgInjector {
    state: InputState,
    sink: Box<dyn EventSink>,
    /// `None` keeps fixed bounds (dry run).
    display: Option<DisplayTarget>,
    /// The capture pipeline's layout, followed for every pointer event.
    capture: Option<Arc<CaptureGeometry>>,
}

impl CgInjector {
    /// Injector posting real events, targeting the main display. Without the Accessibility
    /// permission macOS silently drops the events; this only checks and warns, it never
    /// prompts.
    pub fn new() -> anyhow::Result<Self> {
        if !CGPreflightPostEventAccess() {
            warn!("no permission to post events (Accessibility); injected input will be dropped");
        }
        let sink = CgSink::new()?;
        watch_reconfigurations();
        let mut injector = Self::with_sink(
            Box::new(sink),
            CGRect::default(),
            double_click_interval(),
            Some(CGMainDisplayID()),
        );
        injector.refresh_bounds(Instant::now());
        if let Some(pos) = injector.sink.cursor_location() {
            injector.state.pos = pos;
        }
        Ok(injector)
    }

    /// Injector that logs the events it would post instead of posting them, mapping
    /// coordinates onto fixed `bounds` (global points).
    pub fn dry_run(bounds: CGRect) -> Self {
        Self::with_sink(Box::new(LogSink), bounds, DEFAULT_DOUBLE_CLICK, None)
    }

    /// Map pointer coordinates through the layout the capture pipeline publishes in
    /// `geometry`, onto the display it captures.
    pub fn with_capture_geometry(mut self, geometry: Arc<CaptureGeometry>) -> Self {
        self.capture = Some(geometry);
        self
    }

    /// Map MOUSE_ABS coordinates onto `display` from now on. Its bounds are re-read after
    /// display reconfigurations; if it disappears the main display is used.
    pub fn set_display(&mut self, display: CGDirectDisplayID) {
        self.display = Some(DisplayTarget {
            id: display,
            read: None,
        });
        self.refresh_bounds(Instant::now());
    }

    fn with_sink(
        sink: Box<dyn EventSink>,
        bounds: CGRect,
        double_click: Duration,
        display: Option<CGDirectDisplayID>,
    ) -> Self {
        Self {
            state: InputState::new(bounds, double_click, bounds.mid()),
            sink,
            display: display.map(|id| DisplayTarget { id, read: None }),
            capture: None,
        }
    }

    /// Picks up the current capture layout; retargets when another display is captured.
    fn follow_capture(&mut self) {
        let Some(geometry) = &self.capture else {
            return;
        };
        let layout = geometry.current();
        if let (Some(layout), Some(target)) = (layout, self.display.as_mut())
            && target.id != layout.display
        {
            info!(
                display = layout.display,
                "input follows the captured display"
            );
            target.id = layout.display;
            target.read = None;
        }
        if layout != self.state.layout {
            debug!(?layout, "input capture layout");
            self.state.layout = layout;
        }
    }

    /// Re-read the target display's bounds after a reconfiguration, or when they are stale.
    fn refresh_bounds(&mut self, now: Instant) {
        let Some(target) = &mut self.display else {
            return;
        };
        let generation = RECONFIGURATIONS.load(Ordering::Acquire);
        if let Some((seen, at)) = target.read
            && seen == generation
            && now.saturating_duration_since(at) < BOUNDS_MAX_AGE
        {
            return;
        }
        target.read = Some((generation, now));
        let bounds = display_bounds(target.id).or_else(|| display_bounds(CGMainDisplayID()));
        match bounds {
            Some(bounds) if bounds != self.state.bounds => {
                info!(display = target.id, ?bounds, "input target bounds");
                self.state.bounds = bounds;
            }
            Some(_) => {}
            None => warn!(
                display = target.id,
                "no display bounds; keeping the old ones"
            ),
        }
    }
}

impl InputInjector for CgInjector {
    fn inject(&mut self, ev: InputEvent) {
        let now = Instant::now();
        if matches!(
            ev,
            InputEvent::MouseAbs { .. }
                | InputEvent::MouseRel { .. }
                | InputEvent::MouseButton { .. }
        ) {
            self.follow_capture();
            self.refresh_bounds(now);
        }
        self.state.apply(ev, now, self.sink.as_mut());
    }

    fn release_all(&mut self) {
        self.state.release_all(self.sink.as_mut());
    }
}

impl Drop for CgInjector {
    /// Never leave keys or buttons stuck down on the host.
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    //! Everything here records or inspects events; nothing is ever posted.

    use std::sync::Arc;

    use objc2_core_foundation::CGSize;
    use parking_lot::Mutex;
    use rd_proto::keymap::hid_for_code;

    use super::*;

    const BOUNDS: CGRect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1728.0, 1117.0));
    const INTERVAL: Duration = Duration::from_millis(500);

    #[derive(Default, Clone)]
    struct RecordingSink {
        events: Arc<Mutex<Vec<Synth>>>,
        cursor: Arc<Mutex<Option<CGPoint>>>,
    }

    impl RecordingSink {
        fn take(&self) -> Vec<Synth> {
            std::mem::take(&mut *self.events.lock())
        }
        fn set_cursor(&self, p: Option<CGPoint>) {
            *self.cursor.lock() = p;
        }
    }

    impl EventSink for RecordingSink {
        fn post(&mut self, event: &Synth) {
            self.events.lock().push(*event);
        }
        fn cursor_location(&self) -> Option<CGPoint> {
            *self.cursor.lock()
        }
    }

    struct Harness {
        state: InputState,
        sink: RecordingSink,
        now: Instant,
    }

    impl Harness {
        fn new() -> Self {
            Self::with_bounds(BOUNDS)
        }
        fn with_bounds(bounds: CGRect) -> Self {
            Self {
                state: InputState::new(bounds, INTERVAL, bounds.mid()),
                sink: RecordingSink::default(),
                now: Instant::now(),
            }
        }
        fn send(&mut self, ev: InputEvent) -> Vec<Synth> {
            self.state.apply(ev, self.now, &mut self.sink);
            self.sink.take()
        }
        fn one(&mut self, ev: InputEvent) -> Synth {
            let events = self.send(ev);
            assert_eq!(events.len(), 1, "{ev:?} -> {events:?}");
            events[0]
        }
        fn key(&mut self, code: &str, down: bool) -> Vec<Synth> {
            self.key_ev(code, down, false)
        }
        fn key_ev(&mut self, code: &str, down: bool, repeat: bool) -> Vec<Synth> {
            let hid = hid_for_code(code).unwrap_or_else(|| panic!("no HID for {code}"));
            self.send(InputEvent::Key { down, repeat, hid })
        }
        fn button(&mut self, button: MouseButton, down: bool, x: u16, y: u16) -> Vec<Synth> {
            self.send(InputEvent::MouseButton { button, down, x, y })
        }
        fn advance(&mut self, ms: u64) {
            self.now += Duration::from_millis(ms);
        }
    }

    /// The fields of a [`Synth::Mouse`], minus flags.
    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Mouse {
        ty: CGEventType,
        button: CGMouseButton,
        pos: CGPoint,
        click_state: Option<i64>,
        delta: Option<(i64, i64)>,
    }

    fn mouse(s: Synth) -> Mouse {
        match s {
            Synth::Mouse {
                ty,
                button,
                pos,
                click_state,
                delta,
                ..
            } => Mouse {
                ty,
                button,
                pos,
                click_state,
                delta,
            },
            other => panic!("not a mouse event: {other:?}"),
        }
    }

    fn flags_of(s: &Synth) -> CGEventFlags {
        match *s {
            Synth::Mouse { flags, .. }
            | Synth::Key { flags, .. }
            | Synth::FlagsChanged { flags, .. }
            | Synth::Scroll { flags, .. } => flags,
        }
    }

    fn key_down(keycode: CGKeyCode, autorepeat: bool, flags: CGEventFlags) -> Synth {
        Synth::Key {
            keycode,
            down: true,
            autorepeat,
            flags: flags | CGEventFlags::MaskNonCoalesced,
        }
    }

    fn key_up(keycode: CGKeyCode, flags: CGEventFlags) -> Synth {
        Synth::Key {
            keycode,
            down: false,
            autorepeat: false,
            flags: flags | CGEventFlags::MaskNonCoalesced,
        }
    }

    fn flags_changed(keycode: CGKeyCode, flags: CGEventFlags) -> Synth {
        Synth::FlagsChanged {
            keycode,
            flags: flags | CGEventFlags::MaskNonCoalesced,
        }
    }

    const LSHIFT: CGEventFlags = CGEventFlags(CGEventFlags::MaskShift.bits() | 0x02);
    const RSHIFT: CGEventFlags = CGEventFlags(CGEventFlags::MaskShift.bits() | 0x04);
    const LCMD: CGEventFlags = CGEventFlags(CGEventFlags::MaskCommand.bits() | 0x08);

    #[test]
    fn abs_coordinates_map_onto_display_bounds() {
        let mut h = Harness::new();
        let pos =
            |h: &mut Harness, x, y| mouse(h.one(InputEvent::MouseAbs { buttons: 0, x, y })).pos;
        assert_eq!(pos(&mut h, 0, 0), CGPoint::new(0.0, 0.0));
        // The far edge clamps to the last point on the display.
        assert_eq!(pos(&mut h, 65535, 65535), CGPoint::new(1727.0, 1116.0));
        let mid = pos(&mut h, 32768, 32768);
        assert!(
            (mid.x - 864.0).abs() < 0.1 && (mid.y - 558.5).abs() < 0.1,
            "{mid:?}"
        );

        // A display left of the main one has a negative origin.
        let mut h = Harness::with_bounds(CGRect::new(
            CGPoint::new(-1920.0, 100.0),
            CGSize::new(1920.0, 1080.0),
        ));
        assert_eq!(pos(&mut h, 0, 0), CGPoint::new(-1920.0, 100.0));
        assert_eq!(pos(&mut h, 65535, 65535), CGPoint::new(-1.0, 1179.0));
    }

    #[test]
    fn abs_coordinates_map_through_the_capture_letterbox() {
        // A 16:9 display captured into a 1668x1080 frame: ~71 px bars top and bottom.
        let display = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1920.0, 1080.0));
        let mut h = Harness::with_bounds(display);
        h.state.layout = Some(CaptureLayout {
            display: 1,
            width: 1668,
            height: 1080,
            stretch: false,
        });
        let pos =
            |h: &mut Harness, x, y| mouse(h.one(InputEvent::MouseAbs { buttons: 0, x, y })).pos;
        let norm = |px: f64, size: f64| (px / size * NORM_MAX).round() as u16;
        // The menu bar as seen in the video, ~80 px below the frame top, is ~10 pt down.
        let p = pos(&mut h, norm(834.0, 1668.0), norm(80.0, 1080.0));
        assert!(
            (p.x - 960.0).abs() < 1.0 && (p.y - 10.5).abs() < 0.5,
            "{p:?}"
        );
        // Clicks on the bars land on the nearest edge.
        assert_eq!(pos(&mut h, 0, 0), CGPoint::new(0.0, 0.0));
        assert_eq!(pos(&mut h, 65535, 65535), CGPoint::new(1919.0, 1079.0));
        let mid = pos(&mut h, 32768, 32768);
        assert!((mid.x - 960.0).abs() < 0.1 && (mid.y - 540.0).abs() < 0.1);

        // A stretched capture maps the whole frame, like no layout at all.
        h.state.layout = Some(CaptureLayout {
            display: 1,
            width: 1920,
            height: 1082,
            stretch: true,
        });
        assert_eq!(pos(&mut h, 0, 0), CGPoint::new(0.0, 0.0));
        let p = pos(&mut h, 0, norm(80.0, 1082.0));
        assert!((p.y - 80.0 * 1080.0 / 1082.0).abs() < 0.1, "{p:?}");
    }

    #[test]
    fn injector_follows_the_published_capture_layout() {
        let sink = RecordingSink::default();
        let geometry = Arc::new(CaptureGeometry::default());
        let mut injector = CgInjector::with_sink(Box::new(sink.clone()), BOUNDS, INTERVAL, None)
            .with_capture_geometry(geometry.clone());
        let move_to = |injector: &mut CgInjector, x, y| {
            injector.inject(InputEvent::MouseAbs { buttons: 0, x, y });
            mouse(sink.take()[0]).pos
        };
        // Nothing published: the frame is the display.
        assert_eq!(move_to(&mut injector, 0, 0), CGPoint::new(0.0, 0.0));
        // Letterboxed into a square frame: y = 0 is on the bar, the content starts lower.
        geometry.publish(
            7,
            CaptureLayout {
                display: 1,
                width: 1200,
                height: 1200,
                stretch: false,
            },
        );
        let content_h = 1117.0 / 1728.0;
        let top = ((1.0 - content_h) / 2.0 * NORM_MAX).ceil() as u16;
        let p = move_to(&mut injector, 32768, top);
        assert!(p.y < 1.0 && (p.x - 864.0).abs() < 0.1, "{p:?}");
        assert_eq!(move_to(&mut injector, 32768, 0).y, 0.0, "clamped");
        geometry.retract(7);
        let p = move_to(&mut injector, 32768, top);
        assert!(p.y > 100.0, "{p:?}");
    }

    #[test]
    fn moves_become_drags_while_a_button_is_held() {
        let mut h = Harness::new();
        let mv = InputEvent::MouseAbs {
            buttons: 0,
            x: 100,
            y: 100,
        };
        assert_eq!(mouse(h.one(mv)).ty, CGEventType::MouseMoved);

        let cases = [
            (MouseButton::Left, CGEventType::LeftMouseDragged, 0),
            (MouseButton::Right, CGEventType::RightMouseDragged, 1),
            (MouseButton::Middle, CGEventType::OtherMouseDragged, 2),
            (MouseButton::Back, CGEventType::OtherMouseDragged, 3),
            (MouseButton::Forward, CGEventType::OtherMouseDragged, 4),
        ];
        for (button, dragged, number) in cases {
            h.button(button, true, 100, 100);
            let m = mouse(h.one(mv));
            assert_eq!(
                (m.ty, m.button),
                (dragged, CGMouseButton(number)),
                "{button:?}"
            );
            h.button(button, false, 100, 100);
            assert_eq!(mouse(h.one(mv)).ty, CGEventType::MouseMoved);
        }

        // Left wins over right when both are held.
        h.button(MouseButton::Right, true, 100, 100);
        h.button(MouseButton::Left, true, 100, 100);
        assert_eq!(mouse(h.one(mv)).ty, CGEventType::LeftMouseDragged);
    }

    #[test]
    fn buttons_down_up_and_numbers() {
        let mut h = Harness::new();
        let cases = [
            (
                MouseButton::Left,
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseUp,
                0,
            ),
            (
                MouseButton::Right,
                CGEventType::RightMouseDown,
                CGEventType::RightMouseUp,
                1,
            ),
            (
                MouseButton::Middle,
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                2,
            ),
            (
                MouseButton::Back,
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                3,
            ),
            (
                MouseButton::Forward,
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                4,
            ),
        ];
        for (button, down, up, number) in cases {
            let m = mouse(h.button(button, true, 0, 65535)[0]);
            assert_eq!(
                (m.ty, m.button, m.click_state),
                (down, CGMouseButton(number), Some(1))
            );
            assert_eq!(m.pos, CGPoint::new(0.0, 1116.0));
            // A second down for a held button is ignored.
            assert!(h.button(button, true, 0, 65535).is_empty());
            let m = mouse(h.button(button, false, 0, 65535)[0]);
            assert_eq!((m.ty, m.button), (up, CGMouseButton(number)));
            // So is an up for a button that isn't held.
            assert!(h.button(button, false, 0, 65535).is_empty());
            h.advance(1000);
        }
    }

    #[test]
    fn click_state_counts_within_interval_and_radius() {
        let mut h = Harness::new();
        let click = |h: &mut Harness, button, x| {
            let down = mouse(h.button(button, true, x, 1000)[0]).click_state;
            let up = mouse(h.button(button, false, x, 1000)[0]).click_state;
            assert_eq!(down, up, "an up carries its down's click state");
            h.advance(100);
            down.unwrap()
        };
        let left = MouseButton::Left;
        assert_eq!(click(&mut h, left, 1000), 1);
        assert_eq!(click(&mut h, left, 1000), 2);
        assert_eq!(click(&mut h, left, 1000), 3);
        assert_eq!(click(&mut h, left, 1000), 3, "saturates at triple click");

        h.advance(INTERVAL.as_millis() as u64);
        assert_eq!(click(&mut h, left, 1000), 1, "too slow");
        assert_eq!(click(&mut h, left, 1000), 2);

        // 1000 normalised units are ~26 pt on a 1728 pt wide display.
        assert_eq!(click(&mut h, left, 2000), 1, "too far");
        // ~2.6 pt is close enough.
        assert_eq!(click(&mut h, left, 2100), 2);

        assert_eq!(click(&mut h, MouseButton::Right, 2100), 1, "other button");
        assert_eq!(click(&mut h, left, 2100), 1);
    }

    #[test]
    fn relative_motion_sets_delta_and_clamped_position() {
        let mut h = Harness::new();
        h.sink.set_cursor(Some(CGPoint::new(500.0, 500.0)));
        let m = mouse(h.one(InputEvent::MouseRel { dx: 7, dy: -3 }));
        assert_eq!(m.ty, CGEventType::MouseMoved);
        assert_eq!(m.pos, CGPoint::new(507.0, 497.0));
        assert_eq!(m.delta, Some((7, -3)));

        // Clamped to the display, but the full delta is still reported.
        h.sink.set_cursor(Some(CGPoint::new(1720.0, 2.0)));
        let m = mouse(h.one(InputEvent::MouseRel { dx: 50, dy: -50 }));
        assert_eq!(m.pos, CGPoint::new(1727.0, 0.0));
        assert_eq!(m.delta, Some((50, -50)));

        // Without a cursor position, continue from the last posted one.
        h.sink.set_cursor(None);
        let m = mouse(h.one(InputEvent::MouseRel { dx: -27, dy: 10 }));
        assert_eq!(m.pos, CGPoint::new(1700.0, 10.0));

        // In relative mode a button acts at the cursor, not at the viewer's x/y; with a
        // button held relative motion drags.
        h.sink.set_cursor(Some(CGPoint::new(300.0, 400.0)));
        let m = mouse(h.button(MouseButton::Left, true, 0, 0)[0]);
        assert_eq!(
            (m.ty, m.pos),
            (CGEventType::LeftMouseDown, CGPoint::new(300.0, 400.0))
        );
        let m = mouse(h.one(InputEvent::MouseRel { dx: 1, dy: 1 }));
        assert_eq!(m.ty, CGEventType::LeftMouseDragged);
        assert_eq!((m.pos, m.delta), (CGPoint::new(301.0, 401.0), Some((1, 1))));

        // An absolute move leaves relative mode again.
        h.one(InputEvent::MouseAbs {
            buttons: 1,
            x: 0,
            y: 0,
        });
        let m = mouse(h.button(MouseButton::Left, false, 65535, 0)[0]);
        assert_eq!(
            (m.ty, m.pos),
            (CGEventType::LeftMouseUp, CGPoint::new(1727.0, 0.0))
        );
    }

    #[test]
    fn wheel_converts_dom_signs_units_and_phases() {
        let mut h = Harness::new();
        let wheel = |mode, dx, dy, phase| InputEvent::Wheel {
            mode,
            dx,
            dy,
            phase,
        };
        // DOM deltaY > 0 (scroll down) is a negative Quartz wheel1; deltaX > 0 (scroll right)
        // a negative wheel2.
        assert_eq!(
            h.one(wheel(WheelMode::Pixel, 3, 120, ScrollPhase::Changed)),
            Synth::Scroll {
                unit: CGScrollEventUnit::Pixel,
                wheel1: -120,
                wheel2: -3,
                continuous: true,
                phase: Some(CGScrollPhase::Changed),
                flags: CGEventFlags::empty(),
            }
        );
        assert_eq!(
            h.one(wheel(WheelMode::Line, -1, -3, ScrollPhase::Began)),
            Synth::Scroll {
                unit: CGScrollEventUnit::Line,
                wheel1: 3,
                wheel2: 1,
                continuous: false,
                phase: None,
                flags: CGEventFlags::empty(),
            }
        );
        let phase_of = |s| match s {
            Synth::Scroll { phase, .. } => phase,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            phase_of(h.one(wheel(WheelMode::Pixel, 0, 1, ScrollPhase::Began))),
            Some(CGScrollPhase::Began)
        );
        assert_eq!(
            phase_of(h.one(wheel(WheelMode::Pixel, 0, 1, ScrollPhase::None))),
            None
        );
        // The end of a gesture is posted even without movement; nothing else is.
        assert_eq!(
            phase_of(h.one(wheel(WheelMode::Pixel, 0, 0, ScrollPhase::Ended))),
            Some(CGScrollPhase::Ended)
        );
        assert!(
            h.send(wheel(WheelMode::Pixel, 0, 0, ScrollPhase::None))
                .is_empty()
        );
        assert!(
            h.send(wheel(WheelMode::Line, 0, 0, ScrollPhase::None))
                .is_empty()
        );
        // i16::MIN must not overflow when negated.
        match h.one(wheel(
            WheelMode::Pixel,
            i16::MIN,
            i16::MIN,
            ScrollPhase::None,
        )) {
            Synth::Scroll { wheel1, wheel2, .. } => assert_eq!((wheel1, wheel2), (32768, 32768)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn keys_down_repeat_up() {
        let mut h = Harness::new();
        let none = CGEventFlags::empty();
        assert_eq!(h.key("KeyA", true), [key_down(0x00, false, none)]);
        // Browser autorepeat, and a duplicate down, are posted as autorepeat.
        assert_eq!(h.key_ev("KeyA", true, true), [key_down(0x00, true, none)]);
        assert_eq!(h.key("KeyA", true), [key_down(0x00, true, none)]);
        assert_eq!(h.key("KeyA", false), [key_up(0x00, none)]);
        // Releasing a key that isn't held posts nothing.
        assert!(h.key("KeyA", false).is_empty());
        // A repeat for a key that isn't held starts a fresh press.
        assert_eq!(h.key_ev("Enter", true, true), [key_down(0x24, false, none)]);

        // Unknown HID usages and keys without a macOS keycode are ignored.
        assert!(
            h.send(InputEvent::Key {
                down: true,
                repeat: false,
                hid: 0x0007_00ff
            })
            .is_empty()
        );
        assert!(h.key("MediaPlayPause", true).is_empty());
    }

    #[test]
    fn modifiers_are_flags_changed_and_apply_to_other_events() {
        let mut h = Harness::new();
        assert_eq!(
            h.key("ShiftLeft", true),
            [flags_changed(kvk::SHIFT, LSHIFT)]
        );
        // Modifier repeats post nothing.
        assert!(h.key_ev("ShiftLeft", true, true).is_empty());
        assert_eq!(h.key("KeyA", true), [key_down(0x00, false, LSHIFT)]);
        assert_eq!(
            h.key("ShiftRight", true),
            [flags_changed(kvk::RIGHT_SHIFT, LSHIFT | RSHIFT)]
        );
        // Releasing one shift keeps Shift set because the other is still down.
        assert_eq!(
            h.key("ShiftLeft", false),
            [flags_changed(kvk::SHIFT, RSHIFT)]
        );
        assert_eq!(h.key("KeyA", false), [key_up(0x00, RSHIFT)]);
        assert_eq!(
            h.key("ShiftRight", false),
            [flags_changed(kvk::RIGHT_SHIFT, CGEventFlags::empty())]
        );
        assert!(h.key("ShiftRight", false).is_empty());

        // Command + Option + Control, and Fn as SecondaryFn.
        h.key("MetaLeft", true);
        h.key("AltRight", true);
        h.key("ControlRight", true);
        let [Synth::FlagsChanged { keycode, flags }] = h.key("Fn", true)[..] else {
            panic!("expected one FlagsChanged");
        };
        assert_eq!(keycode, kvk::FUNCTION);
        let expected = LCMD
            | CGEventFlags(CGEventFlags::MaskAlternate.bits() | 0x40)
            | CGEventFlags(CGEventFlags::MaskControl.bits() | 0x2000)
            | CGEventFlags::MaskSecondaryFn;
        assert_eq!(flags, expected | CGEventFlags::MaskNonCoalesced);

        // Mouse and wheel events carry the modifiers too (Cmd-click, Shift-scroll).
        assert_eq!(
            flags_of(&h.button(MouseButton::Left, true, 0, 0)[0]),
            expected
        );
        let wheel = InputEvent::Wheel {
            mode: WheelMode::Line,
            dx: 0,
            dy: 1,
            phase: ScrollPhase::None,
        };
        assert_eq!(flags_of(&h.one(wheel)), expected);
    }

    #[test]
    fn keypad_arrow_and_function_keys_get_class_flags() {
        let numpad = CGEventFlags::MaskNumericPad;
        let fn_ = CGEventFlags::MaskSecondaryFn;
        let cases = [
            ("ArrowUp", numpad | fn_),
            ("ArrowLeft", numpad | fn_),
            ("Numpad1", numpad),
            ("NumpadEnter", numpad),
            ("F1", fn_),
            ("F5", fn_),
            ("F19", fn_),
            ("Home", fn_),
            ("End", fn_),
            ("PageUp", fn_),
            ("PageDown", fn_),
            ("Delete", fn_),
            ("Insert", fn_), // kVK_Help
            ("KeyA", CGEventFlags::empty()),
            ("Backspace", CGEventFlags::empty()),
        ];
        let mut h = Harness::new();
        for (code, class) in cases {
            let ev = h.key(code, true);
            assert_eq!(
                flags_of(&ev[0]),
                class | CGEventFlags::MaskNonCoalesced,
                "{code}"
            );
            h.key(code, false);
        }
    }

    #[test]
    fn caps_lock_toggles_on_each_press() {
        let mut h = Harness::new();
        let caps = CGEventFlags::MaskAlphaShift;
        assert_eq!(
            h.key("CapsLock", true),
            [flags_changed(kvk::CAPS_LOCK, caps)]
        );
        assert!(h.key_ev("CapsLock", true, true).is_empty());
        assert!(h.key("CapsLock", false).is_empty());
        assert_eq!(h.key("KeyA", true), [key_down(0x00, false, caps)]);
        h.key("KeyA", false);
        assert_eq!(
            h.key("CapsLock", true),
            [flags_changed(kvk::CAPS_LOCK, CGEventFlags::empty())]
        );
        assert!(h.key("CapsLock", false).is_empty());
    }

    #[test]
    fn release_all_releases_everything_once() {
        let mut h = Harness::new();
        h.key("ShiftLeft", true);
        h.key("MetaRight", true);
        h.key("CapsLock", true);
        h.key("KeyA", true);
        h.key("KeyB", true);
        h.button(MouseButton::Left, true, 0, 0);
        h.button(MouseButton::Back, true, 0, 0);

        let held = LSHIFT | CGEventFlags(CGEventFlags::MaskCommand.bits() | 0x10);
        let caps = CGEventFlags::MaskAlphaShift;
        let events = h.send(InputEvent::ReleaseAll);
        assert_eq!(events.len(), 7, "{events:?}");
        // Keys first (newest first), still with the modifiers applied.
        assert_eq!(events[0], key_up(0x0b, held | caps));
        assert_eq!(events[1], key_up(0x00, held | caps));
        // Then buttons.
        let (left, back) = (mouse(events[2]), mouse(events[3]));
        assert_eq!(
            (left.ty, left.button),
            (CGEventType::LeftMouseUp, CGMouseButton::Left)
        );
        assert_eq!(flags_of(&events[2]), held | caps);
        assert_eq!(
            (back.ty, back.button),
            (CGEventType::OtherMouseUp, CGMouseButton(3))
        );
        // Then modifiers, and finally Caps Lock.
        assert_eq!(
            events[4],
            flags_changed(
                kvk::SHIFT,
                CGEventFlags(CGEventFlags::MaskCommand.bits() | 0x10) | caps
            )
        );
        assert_eq!(events[5], flags_changed(kvk::RIGHT_COMMAND, caps));
        assert_eq!(
            events[6],
            flags_changed(kvk::CAPS_LOCK, CGEventFlags::empty())
        );

        assert!(h.send(InputEvent::ReleaseAll).is_empty());
        // Nothing is considered held any more.
        assert_eq!(
            h.key("KeyA", true),
            [key_down(0x00, false, CGEventFlags::empty())]
        );
        assert_eq!(
            mouse(h.button(MouseButton::Left, true, 0, 0)[0]).click_state,
            Some(1),
            "click sequence restarts"
        );
    }

    #[test]
    fn injector_releases_held_input_on_drop() {
        let sink = RecordingSink::default();
        let mut injector = CgInjector::with_sink(Box::new(sink.clone()), BOUNDS, INTERVAL, None);
        injector.inject(InputEvent::Key {
            down: true,
            repeat: false,
            hid: 0x0007_0004,
        });
        injector.inject(InputEvent::MouseButton {
            button: MouseButton::Right,
            down: true,
            x: 0,
            y: 0,
        });
        assert_eq!(sink.take().len(), 2);
        drop(injector);
        let released = sink.take();
        assert_eq!(released.len(), 2, "{released:?}");
        assert_eq!(released[0], key_up(0x00, CGEventFlags::empty()));
        assert_eq!(mouse(released[1]).ty, CGEventType::RightMouseUp);
    }

    #[test]
    fn injector_rereads_bounds_after_reconfiguration() {
        let main = CGMainDisplayID();
        let real = display_bounds(main).expect("main display bounds");
        let bogus = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(10.0, 10.0));
        let mut injector = CgInjector::with_sink(
            Box::new(RecordingSink::default()),
            bogus,
            INTERVAL,
            Some(main),
        );
        let now = Instant::now();
        injector.refresh_bounds(now);
        assert_eq!(injector.state.bounds, real, "first use reads the bounds");

        injector.state.bounds = bogus;
        injector.refresh_bounds(now);
        assert_eq!(
            injector.state.bounds, bogus,
            "fresh and no reconfiguration: no re-read"
        );

        on_reconfiguration(
            main,
            CGDisplayChangeSummaryFlags::BeginConfigurationFlag,
            ptr::null_mut(),
        );
        injector.refresh_bounds(now);
        assert_eq!(
            injector.state.bounds, bogus,
            "'begin' notifications are ignored"
        );

        on_reconfiguration(
            main,
            CGDisplayChangeSummaryFlags::SetModeFlag,
            ptr::null_mut(),
        );
        injector.refresh_bounds(now);
        assert_eq!(
            injector.state.bounds, real,
            "re-read after a reconfiguration"
        );

        injector.state.bounds = bogus;
        injector.refresh_bounds(now + BOUNDS_MAX_AGE);
        assert_eq!(injector.state.bounds, real, "re-read when stale");
    }

    #[test]
    fn double_click_interval_comes_from_appkit() {
        let interval = double_click_interval();
        assert!(
            interval > Duration::ZERO && interval <= Duration::from_secs(5),
            "{interval:?}"
        );
    }

    /// The Quartz events built from each [`Synth`] carry the intended fields. Events are only
    /// created and inspected, never posted.
    #[test]
    fn build_event_sets_quartz_fields() {
        let get = |e: &CGEvent, f| CGEvent::integer_value_field(Some(e), f);

        let flags = CGEventFlags::MaskShift | CGEventFlags::MaskCommand;
        let e = build_event(
            None,
            &Synth::Mouse {
                ty: CGEventType::OtherMouseDown,
                button: CGMouseButton(4),
                pos: CGPoint::new(12.0, 34.5),
                click_state: Some(2),
                delta: Some((-5, 9)),
                flags,
            },
        )
        .unwrap();
        assert_eq!(CGEvent::r#type(Some(&e)), CGEventType::OtherMouseDown);
        assert_eq!(CGEvent::location(Some(&e)), CGPoint::new(12.0, 34.5));
        assert_eq!(CGEvent::flags(Some(&e)), flags);
        assert_eq!(get(&e, CGEventField::MouseEventButtonNumber), 4);
        assert_eq!(get(&e, CGEventField::MouseEventClickState), 2);
        assert_eq!(get(&e, CGEventField::MouseEventDeltaX), -5);
        assert_eq!(get(&e, CGEventField::MouseEventDeltaY), 9);

        let flags = CGEventFlags::MaskNumericPad | CGEventFlags::MaskNonCoalesced;
        let e = build_event(
            None,
            &Synth::Key {
                keycode: kvk::UP_ARROW,
                down: true,
                autorepeat: true,
                flags,
            },
        )
        .unwrap();
        assert_eq!(CGEvent::r#type(Some(&e)), CGEventType::KeyDown);
        assert_eq!(
            get(&e, CGEventField::KeyboardEventKeycode),
            i64::from(kvk::UP_ARROW)
        );
        assert_eq!(get(&e, CGEventField::KeyboardEventAutorepeat), 1);
        assert_eq!(CGEvent::flags(Some(&e)), flags);

        let e = build_event(None, &key_up(0x00, CGEventFlags::empty())).unwrap();
        assert_eq!(CGEvent::r#type(Some(&e)), CGEventType::KeyUp);
        assert_eq!(get(&e, CGEventField::KeyboardEventAutorepeat), 0);

        let e = build_event(None, &flags_changed(kvk::SHIFT, LSHIFT)).unwrap();
        assert_eq!(CGEvent::r#type(Some(&e)), CGEventType::FlagsChanged);
        assert_eq!(
            get(&e, CGEventField::KeyboardEventKeycode),
            i64::from(kvk::SHIFT)
        );
        assert_eq!(
            CGEvent::flags(Some(&e)),
            LSHIFT | CGEventFlags::MaskNonCoalesced
        );

        let e = build_event(
            None,
            &Synth::Scroll {
                unit: CGScrollEventUnit::Pixel,
                wheel1: -40,
                wheel2: 7,
                continuous: true,
                phase: Some(CGScrollPhase::Began),
                flags: CGEventFlags::empty(),
            },
        )
        .unwrap();
        assert_eq!(CGEvent::r#type(Some(&e)), CGEventType::ScrollWheel);
        assert_eq!(get(&e, CGEventField::ScrollWheelEventIsContinuous), 1);
        assert_eq!(get(&e, CGEventField::ScrollWheelEventScrollPhase), 1);
        assert_eq!(get(&e, CGEventField::ScrollWheelEventPointDeltaAxis1), -40);
        assert_eq!(get(&e, CGEventField::ScrollWheelEventPointDeltaAxis2), 7);

        let e = build_event(
            None,
            &Synth::Scroll {
                unit: CGScrollEventUnit::Line,
                wheel1: 3,
                wheel2: 0,
                continuous: false,
                phase: None,
                flags: CGEventFlags::empty(),
            },
        )
        .unwrap();
        assert_eq!(get(&e, CGEventField::ScrollWheelEventIsContinuous), 0);
        assert_eq!(get(&e, CGEventField::ScrollWheelEventDeltaAxis1), 3);
        assert_eq!(get(&e, CGEventField::ScrollWheelEventScrollPhase), 0);
    }
}
