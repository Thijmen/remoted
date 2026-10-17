# Third-party notices

rd includes material from the third-party projects listed below. Rust crate and npm
dependencies are not vendored and carry their own licenses; this file covers only code and data
copied into this repository.

## Chromium: DOM key code table

- **What:** `ui/events/keycodes/dom/dom_code_data.inc` from Chromium, vendored unmodified as
  `tools/gen-keymap/dom_code_data.inc` (revision `23aa628a3e283ee1d4d9e438077bfb39b137148d`).
- **Used for:** the generated key tables `crates/rd-proto/src/keymap_gen.rs` and
  `viewer/src/input/keymap.gen.ts`, which are derived from it (USB HID usage, DOM
  `KeyboardEvent.code`, macOS virtual keycode and Windows scancode columns).
- **Source:** https://chromium.googlesource.com/chromium/src/+/23aa628a3e283ee1d4d9e438077bfb39b137148d/ui/events/keycodes/dom/dom_code_data.inc
- **License:** BSD-3-Clause.

```
Copyright 2015 The Chromium Authors

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

   * Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.
   * Redistributions in binary form must reproduce the above
copyright notice, this list of conditions and the following disclaimer
in the documentation and/or other materials provided with the
distribution.
   * Neither the name of Google LLC nor the names of its
contributors may be used to endorse or promote products derived from
this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```
