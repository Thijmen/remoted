# Third-party notices

rd includes material from the third-party projects listed below. Rust crate and npm
dependencies are not vendored and carry their own licenses; this file covers code and data
copied into this repository, and the npm packages bundled into the viewer that the gateway
serves to browsers.

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

## xterm.js: the viewer's terminal

- **What:** the npm packages `@xterm/xterm` 6.0.0, `@xterm/addon-fit` 0.11.0,
  `@xterm/addon-search` 0.16.0, `@xterm/addon-unicode11` 0.9.0, `@xterm/addon-web-links`
  0.12.0 and `@xterm/addon-webgl` 0.19.0 (pinned in `viewer/package.json`).
- **Used for:** the terminal in the viewer (`viewer/src/ui/terminal-view.ts`, docs/terminal.md);
  Vite bundles them into the viewer's build, which the gateway serves.
- **Source:** https://github.com/xtermjs/xterm.js
- **License:** MIT. The add-ons carry the same license with "Copyright (c) 2017–2019, The
  xterm.js authors".

```
Copyright (c) 2017-2019, The xterm.js authors (https://github.com/xtermjs/xterm.js)
Copyright (c) 2014-2016, SourceLair Private Company (https://www.sourcelair.com)
Copyright (c) 2012-2013, Christopher Jeffrey (https://github.com/chjj/)

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```
