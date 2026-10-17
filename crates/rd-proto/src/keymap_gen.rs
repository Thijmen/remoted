// PLACEHOLDER — will be replaced by tools/gen-keymap output (generated from Chromium
// ui/events/keycodes/dom/dom_code_data.inc). Only a handful of keys for bootstrapping.
use super::KeyEntry;

pub static KEYS: &[KeyEntry] = &[
    KeyEntry {
        hid: 0x0007_0004,
        code: "KeyA",
        mac: Some(0x00),
        win: Some(0x001e),
    },
    KeyEntry {
        hid: 0x0007_0028,
        code: "Enter",
        mac: Some(0x24),
        win: Some(0x001c),
    },
    KeyEntry {
        hid: 0x0007_00e3,
        code: "MetaLeft",
        mac: Some(0x37),
        win: Some(0xe05b),
    },
    KeyEntry {
        hid: 0x0007_0052,
        code: "ArrowUp",
        mac: Some(0x7e),
        win: Some(0xe048),
    },
];
