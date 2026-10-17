//! Low-level keyboard hook events -> DOM `KeyboardEvent.code`, via `rd_proto::keymap`.
//!
//! Platform independent so it is unit-tested everywhere; only the Windows hook uses it.
#![cfg_attr(not(windows), allow(dead_code))]

/// `VK_RSHIFT`.
const VK_RSHIFT: u32 = 0xA1;

/// The set-1 scancode (`0xE0xx` = extended) for a hook event's `scanCode` and `LLKHF_EXTENDED`,
/// or `None` when the event is not a physical key of its own.
pub fn scancode(raw: u32, extended: bool, vk: u32) -> Option<u16> {
    // 0: no scancode at all. Above 0xFF: an event Windows synthesizes itself, e.g. the fake LCtrl
    // (scanCode 0x21D) that AltGr layouts send before Right Alt.
    if raw == 0 || raw > 0xFF {
        return None;
    }
    let sc = raw as u16;
    Some(match if extended { 0xE000 | sc } else { sc } {
        // Right Shift is not an extended key, but layouts flag it KBDEXT, so the hook reports it
        // as extended.
        0xE036 if vk == VK_RSHIFT => 0x0036,
        // E0 2A / E0 36: the fake shifts a keyboard sends around the navigation keys when Num Lock
        // is on (or Shift is held). Not keys; forwarding them would unshift Shift+Arrow.
        0xE02A | 0xE036 => return None,
        // Alt+PrintScreen (SysRq) arrives as 0x54.
        0x0054 => 0xE037,
        // Ctrl+Pause (Break) arrives as E0 46.
        0xE046 => 0x0045,
        // NumLock is 0x45 + LLKHF_EXTENDED = 0xE045 and Pause is 0x45 (its E1 prefix is not
        // reported): both already match the table.
        sc => sc,
    })
}

/// Index into a per-key down-state table (512 entries) for a scancode from [`scancode`].
pub fn slot(sc: u16) -> usize {
    (sc & 0xFF) as usize | if sc & 0xFF00 == 0xE000 { 0x100 } else { 0 }
}

/// Number of slots [`slot`] can return.
pub const SLOTS: usize = 0x200;

/// The DOM code for a scancode from [`scancode`], if the key has one.
pub fn code(sc: u16) -> Option<&'static str> {
    rd_proto::keymap::by_win_scancode(sc).map(|k| k.code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dom(raw: u32, extended: bool, vk: u32) -> Option<&'static str> {
        scancode(raw, extended, vk).and_then(code)
    }

    #[test]
    fn ordinary_and_extended_keys() {
        assert_eq!(dom(0x0f, false, 0x09), Some("Tab"));
        assert_eq!(dom(0x1e, false, 0x41), Some("KeyA"));
        assert_eq!(dom(0x01, false, 0x1b), Some("Escape"));
        assert_eq!(dom(0x38, false, 0xa4), Some("AltLeft"));
        assert_eq!(dom(0x38, true, 0xa5), Some("AltRight"));
        assert_eq!(dom(0x1d, false, 0xa2), Some("ControlLeft"));
        assert_eq!(dom(0x1d, true, 0xa3), Some("ControlRight"));
        assert_eq!(dom(0x5b, true, 0x5b), Some("MetaLeft"));
        assert_eq!(dom(0x5c, true, 0x5c), Some("MetaRight"));
        assert_eq!(dom(0x5d, true, 0x5d), Some("ContextMenu"));
        assert_eq!(dom(0x48, true, 0x26), Some("ArrowUp"));
        assert_eq!(dom(0x48, false, 0x68), Some("Numpad8"));
        assert_eq!(dom(0x1c, false, 0x0d), Some("Enter"));
        assert_eq!(dom(0x1c, true, 0x0d), Some("NumpadEnter"));
        assert_eq!(dom(0x35, true, 0x6f), Some("NumpadDivide"));
        assert_eq!(dom(0x3b, false, 0x70), Some("F1"));
    }

    #[test]
    fn quirks() {
        // Right Shift, reported with or without the extended flag.
        assert_eq!(dom(0x36, true, VK_RSHIFT), Some("ShiftRight"));
        assert_eq!(dom(0x36, false, VK_RSHIFT), Some("ShiftRight"));
        assert_eq!(dom(0x2a, false, 0xa0), Some("ShiftLeft"));
        // Fake shifts.
        assert_eq!(scancode(0x2a, true, 0xa0), None);
        assert_eq!(scancode(0x36, true, 0xff), None);
        // NumLock vs Pause, Break.
        assert_eq!(dom(0x45, true, 0x90), Some("NumLock"));
        assert_eq!(dom(0x45, false, 0x13), Some("Pause"));
        assert_eq!(dom(0x46, true, 0x03), Some("Pause"));
        assert_eq!(dom(0x46, false, 0x91), Some("ScrollLock"));
        // PrintScreen, SysRq.
        assert_eq!(dom(0x37, true, 0x2c), Some("PrintScreen"));
        assert_eq!(dom(0x54, false, 0x2c), Some("PrintScreen"));
        assert_eq!(dom(0x37, false, 0x6a), Some("NumpadMultiply"));
        // AltGr's fake LCtrl, no scancode.
        assert_eq!(scancode(0x21d, false, 0xa2), None);
        assert_eq!(scancode(0, false, 0x41), None);
    }

    #[test]
    fn slots_are_distinct() {
        assert_eq!(slot(0x0038), 0x38);
        assert_eq!(slot(0xe038), 0x138);
        assert!(slot(0xe0ff) < SLOTS);
        for k in rd_proto::keymap::KEYS.iter() {
            if let Some(sc) = k.win {
                assert!(slot(sc) < SLOTS, "{}", k.code);
            }
        }
    }

    #[test]
    fn codes_are_plain_identifiers() {
        // The forwarder still JSON-escapes them, but nothing odd should ever be in the table.
        for k in rd_proto::keymap::KEYS.iter() {
            assert!(
                k.code.bytes().all(|b| b.is_ascii_alphanumeric()),
                "{:?}",
                k.code
            );
        }
    }
}
