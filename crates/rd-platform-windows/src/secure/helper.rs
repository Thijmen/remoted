//! The secure-desktop helper: SYSTEM, in the console session, launched by the service.
//!
//! Opens the input desktop (`Winlogon` while the PC is locked or a UAC prompt is up), captures
//! it with GDI into the size the agent asks for, and injects the viewer's input with
//! `SendInput`. The Windows account password is typed at the real lock screen; nothing is
//! stored here.

use std::ffi::c_void;
use std::mem::size_of;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail, ensure};
use rd_agent_core::clock::now_us;
use rd_agent_core::platform::InputInjector;
use rd_proto::bin::{self, BinMsg};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HALFTONE, HDC, HGDIOBJ, ReleaseDC, SRCCOPY,
    SelectObject, SetStretchBltMode, StretchBlt,
};
use windows::Win32::Security::Authentication::Identity::SendSAS;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, DESKTOP_ENUMERATE,
    DESKTOP_READOBJECTS, DESKTOP_SWITCHDESKTOP, DESKTOP_WRITEOBJECTS, GetUserObjectInformationW,
    HDESK, OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo,
    GetSystemMetrics, HICON, ICONINFO, SM_CXSCREEN, SM_CYSCREEN,
};

use super::{
    MSG_CONFIG, MSG_ERROR, MSG_FRAME, MSG_INPUT, MSG_READY, MSG_SAS, MSG_STOP, Pipe, read_message,
    write_message,
};

/// `rd-agent-sec.exe --pipe <name>`: connect to the agent's data pipe and serve the secure
/// desktop until the pipe closes or the agent says stop.
pub fn run(pipe_name: &str) -> anyhow::Result<()> {
    // Physical pixels: the agent's frame and the lock screen coordinates must agree.
    // SAFETY: a process-wide DPI setting; safe to call once.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }.ok();

    let pipe = Pipe::open(pipe_name).context("connecting to the agent")?;
    tracing::info!("connected to the agent");
    // The session reports "locked" a moment before Winlogon becomes the input desktop (and
    // UAC's prompt takes a moment to appear): give it a few seconds.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut attached = loop {
        match Attached::open() {
            Ok(attached) => break attached,
            Err(e) if Instant::now() < deadline => {
                tracing::debug!("waiting for the secure desktop: {e:#}");
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => {
                let _ = write_message(&pipe, MSG_ERROR, format!("{e:#}").as_bytes());
                return Err(e);
            }
        }
    };
    let mut surface = Surface::new(attached.screen, attached.width, attached.height)?;
    let mut injector = crate::input::SendInputInjector::new()
        .context("creating the secure desktop input injector")?;
    let _ = write_message(&pipe, MSG_READY, &[]);

    let mut fps = 10u32;
    let mut next = Instant::now();
    let mut last_sent = Instant::now() - Duration::from_secs(2);
    // Since when captures fail; the desktop may be in a transition (lock, UAC, unlock, a
    // Remote Desktop resize). Give up after FAIL_FOR.
    let mut failing_since: Option<Instant> = None;
    const FAIL_FOR: Duration = Duration::from_secs(10);
    let result = (|| -> anyhow::Result<()> {
        loop {
            // Input from the agent: drain what is there before the next capture.
            loop {
                let available = pipe.peek()?;
                if available == 0 {
                    break;
                }
                let (kind, payload) = read_message(&pipe)?;
                match kind {
                    MSG_STOP => return Ok(()),
                    MSG_INPUT => match bin::parse(&payload) {
                        Ok(BinMsg::Input(event)) => injector.inject(event),
                        Ok(_) => {}
                        Err(_) => {}
                    },
                    MSG_SAS => {
                        // The policy is checked by the agent; the helper is the documented
                        // service-side path (SeTcbPrivilege).
                        // SAFETY: SendSAS takes a BOOL; asuser=false is the service path.
                        unsafe { SendSAS(false) };
                    }
                    MSG_CONFIG => {
                        let (width, height, new_fps) = config(&payload)?;
                        surface = Surface::new(attached.screen, width, height)?;
                        fps = new_fps;
                        next = Instant::now();
                    }
                    other => {
                        let _ = other;
                    }
                }
            }

            let now = Instant::now();
            if now >= next {
                next = now + Duration::from_secs_f64(1.0 / f64::from(fps.clamp(1, 10)));
                match surface.capture(&attached) {
                    Ok(()) => {
                        if let Some(since) = failing_since.take() {
                            tracing::info!(
                                after_ms = since.elapsed().as_millis() as u64,
                                "secure desktop capture works again"
                            );
                        }
                    }
                    Err(e) => {
                        let since = *failing_since.get_or_insert_with(|| {
                            tracing::warn!(
                                width = attached.width,
                                height = attached.height,
                                "secure desktop capture failed, retrying: {e:#}"
                            );
                            now
                        });
                        if now.duration_since(since) >= FAIL_FOR {
                            let msg = format!("capturing the secure desktop: {e:#}");
                            let _ = write_message(&pipe, MSG_ERROR, msg.as_bytes());
                            bail!(msg);
                        }
                        // Attach again: the input desktop or the screen size may have changed.
                        match Attached::open() {
                            Ok(fresh) => {
                                tracing::debug!(
                                    width = fresh.width,
                                    height = fresh.height,
                                    "attached to the secure desktop again"
                                );
                                attached = fresh;
                                surface =
                                    Surface::new(attached.screen, surface.width, surface.height)?;
                            }
                            Err(e) => tracing::debug!("re-attaching: {e:#}"),
                        }
                        next = now + Duration::from_millis(250);
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                }
                if surface.changed || now.duration_since(last_sent) >= Duration::from_secs(1) {
                    let mut payload = Vec::with_capacity(16 + surface.pixels().len());
                    payload.extend_from_slice(&now_us().to_le_bytes());
                    payload.extend_from_slice(&surface.width.to_le_bytes());
                    payload.extend_from_slice(&surface.height.to_le_bytes());
                    payload.extend_from_slice(surface.pixels());
                    write_message(&pipe, MSG_FRAME, &payload)?;
                    surface.changed = false;
                    last_sent = now;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();

    injector.release_all();
    drop(surface);
    drop(attached);
    result
}

/// The calling thread attached to the secure desktop, with that desktop's screen DC.
struct Attached {
    desktop: HDESK,
    screen: HDC,
    width: u32,
    height: u32,
}

impl Attached {
    fn open() -> anyhow::Result<Self> {
        let desktop = open_secure_desktop()?;
        // SAFETY: the thread's desktop is now Winlogon; GetDC(NULL) is its screen.
        let screen = unsafe { GetDC(None) };
        // SAFETY: plain metric queries, for the desktop this thread is on.
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        let attached = Attached {
            desktop,
            screen,
            width: w.max(0) as u32,
            height: h.max(0) as u32,
        };
        ensure!(
            !screen.is_invalid(),
            "GetDC(NULL) failed on the secure desktop"
        );
        ensure!(w > 0 && h > 0, "the secure desktop has no screen ({w}x{h})");
        tracing::info!(width = w, height = h, "attached to the secure desktop");
        Ok(attached)
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        if !self.screen.is_invalid() {
            // SAFETY: balances GetDC in `open`.
            unsafe { ReleaseDC(None, self.screen) };
        }
        // SAFETY: the desktop handle is owned by this value. The thread stays attached to it
        // until the next SetThreadDesktop; closing a thread's own desktop simply fails.
        unsafe { CloseDesktop(self.desktop) }.ok();
    }
}

fn config(payload: &[u8]) -> anyhow::Result<(u32, u32, u32)> {
    ensure!(payload.len() >= 12, "short secure desktop config");
    let width = u32::from_le_bytes(payload[0..4].try_into().expect("4 bytes"));
    let height = u32::from_le_bytes(payload[4..8].try_into().expect("4 bytes"));
    let fps = u32::from_le_bytes(payload[8..12].try_into().expect("4 bytes"));
    ensure!(
        (64..=7680).contains(&width)
            && (64..=4320).contains(&height)
            && width.is_multiple_of(2)
            && height.is_multiple_of(2),
        "unsupported secure desktop size {width}x{height}"
    );
    Ok((width, height, fps.clamp(1, 10)))
}

/// Opens the current input desktop and requires it to be Winlogon. Returns with the calling
/// thread attached to it, so `GetDC(NULL)` and `SendInput` see the secure desktop.
fn open_secure_desktop() -> anyhow::Result<HDESK> {
    // SAFETY: a handle query; the desktop is closed by the caller.
    let desktop = unsafe {
        OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(
                DESKTOP_READOBJECTS.0
                    | DESKTOP_WRITEOBJECTS.0
                    | DESKTOP_SWITCHDESKTOP.0
                    | DESKTOP_ENUMERATE.0,
            ),
        )
    }
    .context("opening the input desktop")?;

    let mut name = [0u16; 64];
    let mut needed = 0u32;
    // SAFETY: `name` is a writable buffer of the given size.
    let read = unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            (name.len() * 2) as u32,
            Some(&mut needed),
        )
    };
    if let Err(e) = read {
        // SAFETY: closes the desktop handle.
        unsafe { CloseDesktop(desktop) }.ok();
        return Err(e).context("reading the input desktop name");
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    let name = String::from_utf16_lossy(&name[..len]);
    if !name.eq_ignore_ascii_case("Winlogon") {
        // SAFETY: closes the desktop handle.
        unsafe { CloseDesktop(desktop) }.ok();
        bail!("the secure desktop is not up (input desktop is {name:?})");
    }
    // SAFETY: attaches this thread to the secure desktop; no windows exist yet.
    if let Err(e) = unsafe { SetThreadDesktop(desktop) } {
        // SAFETY: closes the desktop handle.
        unsafe { CloseDesktop(desktop) }.ok();
        return Err(e).context("attaching to the secure desktop");
    }
    Ok(desktop)
}

/// A top-down 32-bit BGRA frame on a memory DC, filled by `StretchBlt` from the secure desktop.
struct Surface {
    width: u32,
    height: u32,
    memory: HDC,
    bitmap: windows::Win32::Graphics::Gdi::HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    len: usize,
    previous: Vec<u8>,
    changed: bool,
    /// The screen at its own size, when the frame is scaled.
    grab: Option<Box<Surface>>,
}

impl Surface {
    fn new(screen: HDC, width: u32, height: u32) -> anyhow::Result<Self> {
        let width = width.max(64) & !1;
        let height = height.max(64) & !1;
        // SAFETY: a memory DC compatible with the screen.
        let memory = unsafe { CreateCompatibleDC(Some(screen)) };
        ensure!(!memory.is_invalid(), "CreateCompatibleDC failed");
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // Negative: a top-down DIB, so the bytes are in screen order.
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        // SAFETY: `info` describes a 32-bit DIB; `bits` receives its pixels, owned by `bitmap`.
        let bitmap = match unsafe {
            CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0)
        } {
            Ok(bitmap) => bitmap,
            Err(e) => {
                // SAFETY: the DC was created above.
                let _ = unsafe { DeleteDC(memory) }.ok();
                return Err(e).context("CreateDIBSection");
            }
        };
        if bits.is_null() {
            // SAFETY: the bitmap and DC were created above.
            let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) }.ok();
            // SAFETY: the DC was created above.
            let _ = unsafe { DeleteDC(memory) }.ok();
            bail!("CreateDIBSection returned no pixels");
        }
        // SAFETY: selects the DIB into the memory DC; the old object is restored in Drop.
        let old = unsafe { SelectObject(memory, HGDIOBJ(bitmap.0)) };
        Ok(Surface {
            width,
            height,
            memory,
            bitmap,
            old,
            bits: bits.cast(),
            len: width as usize * height as usize * 4,
            previous: Vec::new(),
            changed: true,
            grab: None,
        })
    }

    fn pixels(&self) -> &[u8] {
        // SAFETY: the DIB owns `len` bytes until this Surface is dropped.
        unsafe { std::slice::from_raw_parts(self.bits, self.len) }
    }

    /// Fits the whole secure desktop into the frame and tells whether it changed.
    ///
    /// Reads the screen with a plain `BitBlt` at its own size (what the normal GDI capture
    /// does) and scales memory to memory, so the screen DC never has to stretch. On failure
    /// the error carries Windows' reason.
    fn capture(&mut self, attached: &Attached) -> anyhow::Result<()> {
        let (screen_width, screen_height) = (attached.width, attached.height);
        if (self.width, self.height) == (screen_width, screen_height) {
            blit_screen(self.memory, attached)?;
        } else {
            let grab = match self.grab.take() {
                Some(grab) if (grab.width, grab.height) == (screen_width, screen_height) => grab,
                _ => Box::new(Surface::new(attached.screen, screen_width, screen_height)?),
            };
            blit_screen(grab.memory, attached)?;
            // SAFETY: both are memory DCs with DIBs selected; the rectangles are their sizes.
            unsafe {
                SetStretchBltMode(self.memory, HALFTONE);
                StretchBlt(
                    self.memory,
                    0,
                    0,
                    self.width as i32,
                    self.height as i32,
                    Some(grab.memory),
                    0,
                    0,
                    screen_width as i32,
                    screen_height as i32,
                    SRCCOPY,
                )
            }
            .ok()
            .context("scaling the secure desktop")?;
            self.grab = Some(grab);
        }
        // GDI omits the cursor; draw it into the frame (UAC needs to be clickable).
        self.draw_cursor(screen_width, screen_height);
        // Safety: the DIB owns `len` bytes until this Surface is dropped. The raw slice
        // does not borrow `self.previous`.
        let pixels = unsafe { std::slice::from_raw_parts(self.bits, self.len) };
        if self.previous.len() != pixels.len() {
            self.previous.resize(pixels.len(), 0);
            self.previous.copy_from_slice(pixels);
            self.changed = true;
        } else if self.previous != pixels {
            self.previous.copy_from_slice(pixels);
            self.changed = true;
        }
        Ok(())
    }

    fn draw_cursor(&self, screen_width: u32, screen_height: u32) {
        let mut info = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` has cbSize set; the cursor belongs to the system.
        if unsafe { GetCursorInfo(&mut info) }.is_err() || info.flags != CURSOR_SHOWING {
            return;
        }
        let x =
            i64::from(info.ptScreenPos.x) * i64::from(self.width) / i64::from(screen_width.max(1));
        let y = i64::from(info.ptScreenPos.y) * i64::from(self.height)
            / i64::from(screen_height.max(1));
        let mut icon = ICONINFO::default();
        // SAFETY: the cursor handle is valid; the icon bitmaps are deleted below.
        if unsafe { GetIconInfo(HICON(info.hCursor.0), &mut icon) }.is_err() {
            return;
        }
        // SAFETY: drawing into the memory DC; `DI_NORMAL` composites the cursor.
        let _ = unsafe {
            DrawIconEx(
                self.memory,
                x as i32,
                y as i32,
                HICON(info.hCursor.0),
                0,
                0,
                0,
                None,
                DI_NORMAL,
            )
        };
        // SAFETY: GetIconInfo created these bitmaps.
        if !icon.hbmColor.is_invalid() {
            let _ = unsafe { DeleteObject(HGDIOBJ(icon.hbmColor.0)) }.ok();
        }
        if !icon.hbmMask.is_invalid() {
            let _ = unsafe { DeleteObject(HGDIOBJ(icon.hbmMask.0)) }.ok();
        }
    }
}

/// Copies the whole screen into `target` (a memory DC at the screen's size). Tries without
/// CAPTUREBLT too: some display drivers refuse layered-window capture on the secure desktop.
fn blit_screen(target: HDC, attached: &Attached) -> anyhow::Result<()> {
    let (w, h) = (attached.width as i32, attached.height as i32);
    // SAFETY: `target` holds a DIB of the screen's size; the screen DC is valid.
    let with = unsafe {
        BitBlt(
            target,
            0,
            0,
            w,
            h,
            Some(attached.screen),
            0,
            0,
            SRCCOPY | CAPTUREBLT,
        )
    };
    let Err(first) = with else {
        return Ok(());
    };
    // SAFETY: as above.
    unsafe { BitBlt(target, 0, 0, w, h, Some(attached.screen), 0, 0, SRCCOPY) }.map_err(|e| {
        anyhow::anyhow!("BitBlt from the secure desktop: {e} (with CAPTUREBLT: {first})")
    })
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: restores the old bitmap, then deletes the DIB and the DC.
        unsafe {
            SelectObject(self.memory, self.old);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0)).ok();
            let _ = DeleteDC(self.memory).ok();
        }
    }
}
