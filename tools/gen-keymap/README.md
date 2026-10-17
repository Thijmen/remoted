# gen-keymap

Generates rd's key tables from Chromium's DOM code table:

| Output | Contents |
|---|---|
| `crates/rd-proto/src/keymap_gen.rs` | `KEYS`: HID usage → DOM `code`, macOS `kVK_*`, Windows set-1 scancode |
| `viewer/src/input/keymap.gen.ts` | `CODE_TO_HID` / `hidForCode`: `KeyboardEvent.code` → HID usage |

```sh
bun tools/gen-keymap/gen.ts           # regenerate (only rewrites files that changed)
bun tools/gen-keymap/gen.ts --check   # exit 1 if an output is out of date (CI)
```

## Input

`dom_code_data.inc` is vendored unmodified from Chromium revision
`23aa628a3e283ee1d4d9e438077bfb39b137148d`, so generation works offline and is reproducible.
It is BSD-3-Clause; see `THIRD_PARTY_NOTICES.md`. To update it:

```sh
curl -sSfL 'https://chromium.googlesource.com/chromium/src/+/main/ui/events/keycodes/dom/dom_code_data.inc?format=TEXT' \
  | base64 -d > tools/gen-keymap/dom_code_data.inc
```

Then regenerate, record the new revision here and in `THIRD_PARTY_NOTICES.md`, and run
`cargo test -p rd-proto`.

## Rules

- Rows with a `NULL` (or empty) `code` are skipped: the viewer can never produce them.
- Only HID pages 0x01 (Generic Desktop: Sleep, WakeUp, Fn), 0x07 (Keyboard/Keypad) and 0x0C
  (Consumer: media and browser keys) are exported. Chromium's page-0 rows are not USB usages.
- Mac `0xffff` and Windows `0x0000` mean "no key" and become `None`.
- `MAC_OVERRIDES` in `gen.ts` adds what Chromium leaves out: Fn → `kVK_Function` (0x3F). The
  generator fails if Chromium starts mapping an overridden key itself.
- The generator fails on duplicate HID usages or codes, and if it cannot parse every
  `DOM_CODE(...)` row.
