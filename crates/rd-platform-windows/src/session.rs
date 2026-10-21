//! Lock state, OS description, permissions.

use rd_agent_core::{LockState, Perms};
use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK, WTS_SESSIONSTATE_UNLOCK,
    WTSConnectState, WTSDisconnected, WTSFreeMemory, WTSINFOEXW, WTSQuerySessionInformationW,
    WTSSessionInfoEx,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetUserObjectInformationW,
    OpenInputDesktop, UOI_NAME,
};
use windows::core::{HSTRING, PWSTR};

const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

fn reg_string(value: &str) -> Option<String> {
    let mut buf = [0u16; 256];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: `buf`/`size` describe a writable buffer.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(CURRENT_VERSION),
            &HSTRING::from(value),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

fn reg_dword(value: &str) -> Option<u32> {
    let mut data = 0u32;
    let mut size = 4u32;
    // SAFETY: `data`/`size` describe a writable DWORD.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(CURRENT_VERSION),
            &HSTRING::from(value),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut data).cast()),
            Some(&mut size),
        )
    };
    (rc == ERROR_SUCCESS).then_some(data)
}

/// e.g. "Windows 11 Pro 25H2 (26200.6899)".
pub fn os_description() -> String {
    let build: Option<u32> = reg_string("CurrentBuildNumber").and_then(|b| b.parse().ok());
    let product = reg_string("ProductName").unwrap_or_else(|| "Windows".into());
    describe(
        &product,
        reg_string("DisplayVersion").as_deref(),
        build,
        reg_dword("UBR"),
    )
}

/// Windows 11 still reports itself as "Windows 10" in `ProductName`; builds from 22000 on are
/// Windows 11.
fn describe(
    product: &str,
    display_version: Option<&str>,
    build: Option<u32>,
    ubr: Option<u32>,
) -> String {
    let product = match build {
        Some(b) if b >= 22000 => product.replacen("Windows 10", "Windows 11", 1),
        _ => product.to_owned(),
    };
    let mut s = product;
    if let Some(v) = display_version.filter(|v| !v.is_empty()) {
        s.push(' ');
        s.push_str(v);
    }
    match (build, ubr) {
        (Some(b), Some(u)) => s.push_str(&format!(" ({b}.{u})")),
        (Some(b), None) => s.push_str(&format!(" ({b})")),
        _ => {}
    }
    s
}

/// Nothing to grant on Windows: no privacy prompts guard screen capture or input injection
/// for a process in the user's own session. (Elevated windows ignore input from an
/// unelevated agent; see docs/setup-windows.md.)
pub fn permissions() -> Perms {
    Perms {
        screen: true,
        input: true,
    }
}

/// Locked when the session is locked, or when the input desktop is not the user's
/// (the secure desktop of a UAC prompt or Ctrl+Alt+Del): input cannot reach it and it cannot
/// be captured.
pub fn lock_state() -> LockState {
    match (session_locked(), on_default_desktop()) {
        (Some(true), _) | (_, Some(false)) => LockState::Locked,
        (Some(false), _) | (None, Some(true)) => LockState::Unlocked,
        (None, None) => LockState::Unknown,
    }
}

/// Whether this process's session is a disconnected one (a Remote Desktop client went away
/// without signing out). Windows draws nothing for it.
pub fn disconnected() -> bool {
    let mut buf = PWSTR::null();
    let mut bytes = 0u32;
    // SAFETY: WTS allocates the buffer; freed below.
    let ok = unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            WTS_CURRENT_SESSION,
            WTSConnectState,
            &mut buf,
            &mut bytes,
        )
    }
    .is_ok();
    if !ok {
        return false;
    }
    // SAFETY: for WTSConnectState the buffer is a WTS_CONNECTSTATE_CLASS (an i32).
    let state = if bytes as usize >= size_of::<i32>() {
        // SAFETY: as above; WTS returned at least an i32.
        unsafe { *buf.as_ptr().cast::<i32>() }
    } else {
        -1
    };
    // SAFETY: allocated by WTSQuerySessionInformationW.
    unsafe { WTSFreeMemory(buf.as_ptr().cast()) };
    state == WTSDisconnected.0
}

fn session_locked() -> Option<bool> {
    let mut buf = PWSTR::null();
    let mut bytes = 0u32;
    // SAFETY: WTS allocates the buffer; freed below.
    unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buf,
            &mut bytes,
        )
    }
    .ok()?;
    let locked = (bytes as usize >= size_of::<WTSINFOEXW>()).then(|| {
        // SAFETY: WTS returned a WTSINFOEXW of `bytes` bytes; level 1 is the only level.
        let info = unsafe { &*buf.as_ptr().cast::<WTSINFOEXW>() };
        let flags = unsafe { info.Data.WTSInfoExLevel1.SessionFlags } as u32;
        match flags {
            WTS_SESSIONSTATE_LOCK => Some(true),
            WTS_SESSIONSTATE_UNLOCK => Some(false),
            _ => None,
        }
    });
    // SAFETY: allocated by WTSQuerySessionInformationW.
    unsafe { WTSFreeMemory(buf.as_ptr().cast()) };
    locked.flatten()
}

/// Whether input goes to the `Default` desktop. `Some(false)` also when the input desktop
/// cannot even be opened (the secure desktop refuses ordinary processes).
fn on_default_desktop() -> Option<bool> {
    // SAFETY: plain handle query, closed below.
    let desk =
        match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) } {
            Ok(d) => d,
            Err(_) => return Some(false),
        };
    let mut name = [0u16; 64];
    let mut needed = 0u32;
    // SAFETY: `name` is a writable buffer of the given size.
    let ok = unsafe {
        GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            (name.len() * 2) as u32,
            Some(&mut needed),
        )
    };
    // SAFETY: opened above.
    let _ = unsafe { CloseDesktop(desk) };
    ok.ok()?;
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..len]).eq_ignore_ascii_case("Default"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_windows_11_by_build() {
        assert_eq!(
            describe("Windows 10 Pro", Some("25H2"), Some(26200), Some(6899)),
            "Windows 11 Pro 25H2 (26200.6899)"
        );
        assert_eq!(
            describe("Windows 10 Enterprise", Some("22H2"), Some(19045), None),
            "Windows 10 Enterprise 22H2 (19045)"
        );
        assert_eq!(describe("Windows", None, None, None), "Windows");
    }

    #[test]
    fn os_description_names_windows() {
        let s = os_description();
        assert!(s.starts_with("Windows"), "{s}");
        assert!(s.contains('(') && s.ends_with(')'), "{s}");
    }

    #[test]
    fn lock_state_smoke() {
        // Outside an interactive session (services, some CI) this is legitimately Locked or
        // Unknown.
        println!("lock state: {:?}", lock_state());
    }
}
