//! macOS event-tap events -> DOM `KeyboardEvent.code`, and whether the tap swallows them.
//!
//! Platform independent so it is unit-tested everywhere; only the macOS tap uses it.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

/// Virtual keycodes (`kVK_*`) are below this.
pub const KEYCODES: usize = 128;

/// `kVK_*` values the tap treats specially.
pub mod kvk {
    pub const ESCAPE: u16 = 0x35;
    pub const RIGHT_COMMAND: u16 = 0x36;
    pub const COMMAND: u16 = 0x37;
    pub const SHIFT: u16 = 0x38;
    pub const CAPS_LOCK: u16 = 0x39;
    pub const OPTION: u16 = 0x3A;
    pub const CONTROL: u16 = 0x3B;
    pub const RIGHT_SHIFT: u16 = 0x3C;
    pub const RIGHT_OPTION: u16 = 0x3D;
    pub const RIGHT_CONTROL: u16 = 0x3E;
    pub const FUNCTION: u16 = 0x3F;
}

/// `CGEventFlags` bits.
pub mod flag {
    pub const SHIFT: u64 = 0x0002_0000;
    pub const CONTROL: u64 = 0x0004_0000;
    pub const OPTION: u64 = 0x0008_0000;
    pub const COMMAND: u64 = 0x0010_0000;
    pub const FUNCTION: u64 = 0x0080_0000;
    // Device-dependent bits (IOKit's NX_DEVICE*KEYMASK): which side is down.
    pub const LEFT_CONTROL: u64 = 0x0000_0001;
    pub const LEFT_SHIFT: u64 = 0x0000_0002;
    pub const RIGHT_SHIFT: u64 = 0x0000_0004;
    pub const LEFT_COMMAND: u64 = 0x0000_0008;
    pub const RIGHT_COMMAND: u64 = 0x0000_0010;
    pub const LEFT_OPTION: u64 = 0x0000_0020;
    pub const RIGHT_OPTION: u64 = 0x0000_0040;
    pub const RIGHT_CONTROL: u64 = 0x0000_2000;
    pub const DEVICE_SIDES: u64 = LEFT_CONTROL
        | LEFT_SHIFT
        | RIGHT_SHIFT
        | LEFT_COMMAND
        | RIGHT_COMMAND
        | LEFT_OPTION
        | RIGHT_OPTION
        | RIGHT_CONTROL;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Down,
    Up,
    /// A modifier (or Caps Lock, Fn) changed.
    FlagsChanged,
}

/// One keyboard event as the tap sees it.
#[derive(Clone, Copy, Debug)]
pub struct Input {
    pub kind: Kind,
    pub keycode: u16,
    pub flags: u64,
    pub autorepeat: bool,
    /// The character the key types in the current layout (lowercase), for shortcuts that are
    /// matched by character rather than position.
    pub ch: Option<char>,
}

/// A key to forward to the page (`rdNative.key`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub code: &'static str,
    pub down: bool,
    pub repeat: bool,
}

/// What to do with one event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// Drop the event, so macOS (the Dock, Spotlight, the menu bar, the app) never sees it.
    pub swallow: bool,
    pub forward: Option<Key>,
}

const PASS: Verdict = Verdict {
    swallow: false,
    forward: None,
};

/// Key state for one grab.
pub struct Tracker {
    /// Keys whose press the tap swallowed.
    down: [bool; KEYCODES],
    /// Keys macOS already had down when the grab started. Their repeats and release pass
    /// through (macOS saw the press); the release is forwarded as well.
    preheld: [bool; KEYCODES],
    /// Keys whose press was a local shortcut: their repeats and release stay local too.
    local: [bool; KEYCODES],
}

impl Tracker {
    pub fn new(preheld: [bool; KEYCODES]) -> Self {
        Self {
            down: [false; KEYCODES],
            preheld,
            local: [false; KEYCODES],
        }
    }

    pub fn on_event(&mut self, ev: &Input) -> Verdict {
        let k = ev.keycode as usize;
        if k >= KEYCODES {
            return PASS;
        }
        match ev.kind {
            Kind::FlagsChanged => self.on_flags(ev, k),
            Kind::Down => {
                if self.local[k] || self.preheld[k] {
                    return PASS;
                }
                if is_local_shortcut(ev) {
                    self.local[k] = true;
                    return PASS;
                }
                let was_down = std::mem::replace(&mut self.down[k], true);
                swallow(key(ev.keycode, true, ev.autorepeat || was_down))
            }
            Kind::Up => {
                if std::mem::take(&mut self.local[k]) {
                    return PASS;
                }
                if std::mem::take(&mut self.preheld[k]) {
                    return Verdict {
                        swallow: false,
                        forward: key(ev.keycode, false, false),
                    };
                }
                self.down[k] = false;
                swallow(key(ev.keycode, false, false))
            }
        }
    }

    fn on_flags(&mut self, ev: &Input, k: usize) -> Verdict {
        // macOS toggles the lock (and its light) before any tap sees the key, so swallowing it
        // would not keep the local lock as it was. It goes to the page as a DOM event, and the
        // page makes the remote lock follow the local one.
        if ev.keycode == kvk::CAPS_LOCK {
            return PASS;
        }
        let down = modifier_down(ev.keycode, ev.flags).unwrap_or(!self.down[k] && !self.preheld[k]);
        if self.preheld[k] {
            if down {
                return PASS;
            }
            self.preheld[k] = false;
            return Verdict {
                swallow: false,
                forward: key(ev.keycode, false, false),
            };
        }
        let was_down = std::mem::replace(&mut self.down[k], down);
        if was_down == down {
            // No change for this key (e.g. the other side of the same modifier).
            return swallow(None);
        }
        swallow(key(ev.keycode, down, false))
    }
}

fn swallow(forward: Option<Key>) -> Verdict {
    Verdict {
        swallow: true,
        forward,
    }
}

fn key(keycode: u16, down: bool, repeat: bool) -> Option<Key> {
    code(keycode).map(|code| Key { code, down, repeat })
}

/// The DOM code for a virtual keycode, if the key has one.
pub fn code(keycode: u16) -> Option<&'static str> {
    rd_proto::keymap::by_mac_keycode(keycode).map(|k| k.code)
}

/// Shortcuts that always go to this Mac, like Ctrl+Alt+Del on Windows:
/// - Ctrl+Cmd+Q (Lock Screen): locking the remote Mac would cut off its input until it is
///   unlocked through the recovery path.
/// - Cmd+Option+Esc (Force Quit): the way out if this app hangs.
fn is_local_shortcut(ev: &Input) -> bool {
    let has = |f| ev.flags & f != 0;
    // With Ctrl held the key may report its control code (^Q, U+0011) instead of 'q'.
    let q = matches!(ev.ch, Some('q' | '\u{11}'));
    (q && has(flag::CONTROL) && has(flag::COMMAND))
        || (ev.keycode == kvk::ESCAPE && has(flag::COMMAND) && has(flag::OPTION))
}

/// Whether a modifier key is down after a `FlagsChanged` event, or `None` when the flags do
/// not say (an event without side information while the other side may be down).
fn modifier_down(keycode: u16, flags: u64) -> Option<bool> {
    let (side, generic) = match keycode {
        kvk::CONTROL => (flag::LEFT_CONTROL, flag::CONTROL),
        kvk::RIGHT_CONTROL => (flag::RIGHT_CONTROL, flag::CONTROL),
        kvk::SHIFT => (flag::LEFT_SHIFT, flag::SHIFT),
        kvk::RIGHT_SHIFT => (flag::RIGHT_SHIFT, flag::SHIFT),
        kvk::COMMAND => (flag::LEFT_COMMAND, flag::COMMAND),
        kvk::RIGHT_COMMAND => (flag::RIGHT_COMMAND, flag::COMMAND),
        kvk::OPTION => (flag::LEFT_OPTION, flag::OPTION),
        kvk::RIGHT_OPTION => (flag::RIGHT_OPTION, flag::OPTION),
        kvk::FUNCTION => return Some(flags & flag::FUNCTION != 0),
        _ => return None,
    };
    if flags & generic == 0 {
        Some(false)
    } else if flags & flag::DEVICE_SIDES != 0 {
        Some(flags & side != 0)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: [bool; KEYCODES] = [false; KEYCODES];
    const TAB: u16 = 0x30;
    const KEY_A: u16 = 0x00;
    const KEY_Q: u16 = 0x0C;

    fn ev(kind: Kind, keycode: u16, flags: u64) -> Input {
        Input {
            kind,
            keycode,
            flags,
            autorepeat: false,
            ch: None,
        }
    }

    fn fwd(code: &'static str, down: bool, repeat: bool) -> Option<Key> {
        Some(Key { code, down, repeat })
    }

    #[test]
    fn cmd_tab_is_swallowed_and_forwarded() {
        let mut t = Tracker::new(NONE);
        let cmd = flag::COMMAND | flag::LEFT_COMMAND;
        let v = t.on_event(&ev(Kind::FlagsChanged, kvk::COMMAND, cmd));
        assert_eq!(v, swallow(fwd("MetaLeft", true, false)));
        assert_eq!(
            t.on_event(&ev(Kind::Down, TAB, cmd)),
            swallow(fwd("Tab", true, false))
        );
        assert_eq!(
            t.on_event(&ev(Kind::Up, TAB, cmd)),
            swallow(fwd("Tab", false, false))
        );
        assert_eq!(
            t.on_event(&ev(Kind::FlagsChanged, kvk::COMMAND, 0)),
            swallow(fwd("MetaLeft", false, false))
        );
    }

    #[test]
    fn modifier_sides() {
        let mut t = Tracker::new(NONE);
        let both = flag::SHIFT | flag::LEFT_SHIFT | flag::RIGHT_SHIFT;
        let right = flag::SHIFT | flag::RIGHT_SHIFT;
        let left = flag::SHIFT | flag::LEFT_SHIFT;
        let mut flags = |kc, f| t.on_event(&ev(Kind::FlagsChanged, kc, f)).forward;
        assert_eq!(flags(kvk::SHIFT, left), fwd("ShiftLeft", true, false));
        assert_eq!(
            flags(kvk::RIGHT_SHIFT, both),
            fwd("ShiftRight", true, false)
        );
        assert_eq!(flags(kvk::SHIFT, right), fwd("ShiftLeft", false, false));
        assert_eq!(flags(kvk::RIGHT_SHIFT, 0), fwd("ShiftRight", false, false));
        let opt = flag::OPTION | flag::RIGHT_OPTION;
        assert_eq!(flags(kvk::RIGHT_OPTION, opt), fwd("AltRight", true, false));
        assert_eq!(flags(kvk::RIGHT_OPTION, 0), fwd("AltRight", false, false));
        let ctl = flag::CONTROL | flag::RIGHT_CONTROL;
        assert_eq!(
            flags(kvk::RIGHT_CONTROL, ctl),
            fwd("ControlRight", true, false)
        );
        // Fn has no side bits.
        assert_eq!(flags(kvk::FUNCTION, flag::FUNCTION), fwd("Fn", true, false));
        assert_eq!(flags(kvk::FUNCTION, 0), fwd("Fn", false, false));
    }

    #[test]
    fn modifiers_without_side_bits_toggle() {
        let mut t = Tracker::new(NONE);
        let v = t.on_event(&ev(Kind::FlagsChanged, kvk::COMMAND, flag::COMMAND));
        assert_eq!(v.forward, fwd("MetaLeft", true, false));
        // Generic Command still set (say, by the right side), no side bits: the left one toggles.
        let v = t.on_event(&ev(Kind::FlagsChanged, kvk::COMMAND, flag::COMMAND));
        assert_eq!(v.forward, fwd("MetaLeft", false, false));
    }

    #[test]
    fn caps_lock_passes_through() {
        let mut t = Tracker::new(NONE);
        assert_eq!(
            t.on_event(&ev(Kind::FlagsChanged, kvk::CAPS_LOCK, 0x0001_0000)),
            PASS
        );
    }

    #[test]
    fn repeats() {
        let mut t = Tracker::new(NONE);
        assert_eq!(
            t.on_event(&ev(Kind::Down, KEY_A, 0)).forward,
            fwd("KeyA", true, false)
        );
        let mut again = ev(Kind::Down, KEY_A, 0);
        again.autorepeat = true;
        assert_eq!(t.on_event(&again).forward, fwd("KeyA", true, true));
        // A second press without a release is a repeat as well.
        assert_eq!(
            t.on_event(&ev(Kind::Down, KEY_A, 0)).forward,
            fwd("KeyA", true, true)
        );
    }

    #[test]
    fn local_shortcuts_stay_local() {
        let mut t = Tracker::new(NONE);
        let flags = flag::CONTROL | flag::COMMAND | flag::LEFT_CONTROL | flag::LEFT_COMMAND;
        let mut q = ev(Kind::Down, KEY_Q, flags);
        q.ch = Some('q');
        assert_eq!(t.on_event(&q), PASS);
        let mut q_repeat = q;
        q_repeat.autorepeat = true;
        assert_eq!(t.on_event(&q_repeat), PASS);
        assert_eq!(t.on_event(&ev(Kind::Up, KEY_Q, flags)), PASS);
        // Reported as a control code.
        let mut ctrl_q = ev(Kind::Down, KEY_Q, flags);
        ctrl_q.ch = Some('\u{11}');
        assert_eq!(t.on_event(&ctrl_q), PASS);
        assert_eq!(t.on_event(&ev(Kind::Up, KEY_Q, flags)), PASS);
        // Matched by character: on AZERTY, Q is where A is on QWERTY.
        let mut azerty_q = ev(Kind::Down, KEY_A, flags);
        azerty_q.ch = Some('q');
        assert_eq!(t.on_event(&azerty_q), PASS);
        // Plain Cmd+Q goes to the remote Mac.
        let mut cmd_q = ev(Kind::Down, KEY_Q, flag::COMMAND | flag::LEFT_COMMAND);
        cmd_q.ch = Some('q');
        assert_eq!(t.on_event(&cmd_q), swallow(fwd("KeyQ", true, false)));
        // Force Quit.
        let fq = flag::COMMAND | flag::OPTION;
        assert_eq!(t.on_event(&ev(Kind::Down, kvk::ESCAPE, fq)), PASS);
        assert_eq!(t.on_event(&ev(Kind::Up, kvk::ESCAPE, fq)), PASS);
    }

    #[test]
    fn keys_held_before_the_grab() {
        let mut preheld = NONE;
        preheld[KEY_A as usize] = true;
        preheld[kvk::COMMAND as usize] = true;
        let mut t = Tracker::new(preheld);
        let mut repeat = ev(Kind::Down, KEY_A, 0);
        repeat.autorepeat = true;
        assert_eq!(t.on_event(&repeat), PASS);
        // The release goes to macOS (it saw the press) and to the page (which saw it too).
        let up = t.on_event(&ev(Kind::Up, KEY_A, 0));
        assert_eq!(
            up,
            Verdict {
                swallow: false,
                forward: fwd("KeyA", false, false)
            }
        );
        let up = t.on_event(&ev(Kind::FlagsChanged, kvk::COMMAND, 0));
        assert_eq!(
            up,
            Verdict {
                swallow: false,
                forward: fwd("MetaLeft", false, false)
            }
        );
        // From then on they are grabbed like any other key.
        assert_eq!(
            t.on_event(&ev(Kind::Down, KEY_A, 0)),
            swallow(fwd("KeyA", true, false))
        );
    }

    #[test]
    fn unknown_keycodes_pass() {
        let mut t = Tracker::new(NONE);
        assert_eq!(t.on_event(&ev(Kind::Down, 200, 0)), PASS);
        // In range but not in the table (0x34 is unused): swallowed, nothing forwarded.
        assert_eq!(t.on_event(&ev(Kind::Down, 0x34, 0)), swallow(None));
    }
}
