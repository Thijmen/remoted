//! Test aid: a small always-on-top window on the primary display that changes colour every
//! frame for a few seconds, so screen-capture measurements have a desktop that changes at the
//! display's refresh rate (`--seconds N`, default 6). It moves the pointer across it too.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, PAINTSTRUCT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        MSG, PM_REMOVE, PeekMessageW, RegisterClassW, SW_SHOWNOACTIVATE, SetCursorPos, ShowWindow,
        TranslateMessage, WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
        WS_POPUP,
    };
    use windows::core::w;

    static mut FRAME: u32 = 0;

    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_PAINT {
            unsafe {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let f = FRAME;
                let colour =
                    COLORREF((f * 7 % 256) | ((f * 3 % 256) << 8) | ((255 - f % 256) << 16));
                let brush = CreateSolidBrush(colour);
                FillRect(hdc, &ps.rcPaint, brush);
                let _ = DeleteObject(brush.into());
                let _ = EndPaint(hwnd, &ps);
            }
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    let seconds: u64 = std::env::args()
        .skip_while(|a| a != "--seconds")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);
    rd_platform_windows::display::ensure_dpi_aware();
    let primary = rd_platform_windows::display::resolve(0)
        .expect("primary display")
        .rect;
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(proc),
            hInstance: instance.into(),
            lpszClassName: w!("rd-animate"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let (x, y) = (primary.x + 100, primary.y + 100);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            w!("rd-animate"),
            w!("rd animate"),
            WS_POPUP,
            x,
            y,
            480,
            320,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let end = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < end {
            FRAME += 1;
            let _ = InvalidateRect(Some(hwnd), None::<*const RECT>, false);
            let f = FRAME as i32;
            let _ = SetCursorPos(x + 40 + (f * 5) % 400, y + 160);
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        let _ = DestroyWindow(hwnd);
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {}
