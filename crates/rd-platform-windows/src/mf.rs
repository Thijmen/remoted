//! COM / Media Foundation lifetime and H.264 encoder (MFT) discovery.

use std::sync::OnceLock;

use anyhow::Context as _;
use tracing::warn;
use windows::Win32::Foundation::LUID;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, MF_VERSION, MFCreateAttributes, MFMediaType_Video, MFSTARTUP_LITE,
    MFShutdown, MFStartup, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_ADAPTER_LUID,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_FRIENDLY_NAME_Attribute, MFT_REGISTER_TYPE_INFO, MFTEnum2, MFVideoFormat_H264,
    MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoIncrementMTAUsage, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};

/// COM initialised (multithreaded apartment) on this thread until dropped.
pub struct ComThread {
    initialized: bool,
}

impl ComThread {
    pub fn init() -> Self {
        // SAFETY: balanced by CoUninitialize in Drop when it succeeded (S_OK or S_FALSE).
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() {
            // RPC_E_CHANGED_MODE: the thread is already in an STA; COM still works.
            warn!("CoInitializeEx(MTA): {hr:?}");
        }
        ComThread {
            initialized: hr.is_ok(),
        }
    }
}

impl Drop for ComThread {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: balances the successful CoInitializeEx in `init`, on the same thread.
            unsafe { CoUninitialize() };
        }
    }
}

/// Moves a COM object to another thread. Only for free-threaded objects created in the MTA
/// (Media Foundation objects, D3D11 devices), which `windows` does not mark `Send`.
pub struct Mta<T>(pub T);

// SAFETY: see the type docs; callers only wrap free-threaded MTA objects.
unsafe impl<T> Send for Mta<T> {}

/// Keeps the process's multithreaded apartment alive and Media Foundation started for the
/// rest of the process, so any thread (including the async runtime's) may use COM and MF
/// objects without initialising COM itself. Idempotent.
pub fn ensure_started() -> anyhow::Result<()> {
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
    STARTED
        .get_or_init(|| {
            // SAFETY: the cookie is leaked on purpose: the MTA stays up until the process exits.
            unsafe { CoIncrementMTAUsage() }.map_err(|e| format!("CoIncrementMTAUsage: {e}"))?;
            std::mem::forget(MfLib::start().map_err(|e| format!("{e:#}"))?);
            Ok(())
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

/// Media Foundation started until dropped (MFStartup / MFShutdown are reference counted).
pub struct MfLib(());

impl MfLib {
    pub fn start() -> anyhow::Result<Self> {
        // SAFETY: balanced by MFShutdown in Drop.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.context("MFStartup")?;
        Ok(MfLib(()))
    }
}

impl Drop for MfLib {
    fn drop(&mut self) {
        // SAFETY: balances the MFStartup in `start`.
        if let Err(e) = unsafe { MFShutdown() } {
            warn!("MFShutdown: {e}");
        }
    }
}

/// An H.264 encoder MFT that takes NV12.
pub struct EncoderInfo {
    pub activate: IMFActivate,
    pub name: String,
    pub hardware: bool,
}

// SAFETY: IMFActivate objects are free-threaded (MTA).
unsafe impl Send for EncoderInfo {}

/// NV12 -> H.264 encoders, best first: the hardware encoders of the adapter `luid` (all
/// hardware encoders without one), then the software ones. Needs [`MfLib`].
pub fn h264_encoders(luid: Option<LUID>) -> anyhow::Result<Vec<EncoderInfo>> {
    let mut list = enumerate(true, luid)?;
    list.extend(enumerate(false, None)?);
    Ok(list)
}

fn enumerate(hardware: bool, luid: Option<LUID>) -> anyhow::Result<Vec<EncoderInfo>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags = MFT_ENUM_FLAG_SORTANDFILTER
        | if hardware {
            MFT_ENUM_FLAG_HARDWARE
        } else {
            MFT_ENUM_FLAG_SYNCMFT
        };
    let attributes = match luid.filter(|_| hardware) {
        Some(luid) => {
            let mut attrs: Option<IMFAttributes> = None;
            // SAFETY: out pointer to a local.
            unsafe { MFCreateAttributes(&mut attrs, 1) }.context("MFCreateAttributes")?;
            let attrs = attrs.context("MFCreateAttributes returned nothing")?;
            // SAFETY: the LUID is copied into the attribute store.
            unsafe {
                attrs.SetBlob(
                    &MFT_ENUM_ADAPTER_LUID,
                    std::slice::from_raw_parts((&raw const luid).cast::<u8>(), size_of::<LUID>()),
                )
            }
            .context("setting MFT_ENUM_ADAPTER_LUID")?;
            Some(attrs)
        }
        None => None,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: valid type infos; the array is CoTaskMemAlloc'ed by MF and freed below.
    unsafe {
        MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            attributes.as_ref(),
            &mut activates,
            &mut count,
        )
    }
    .context("MFTEnum2")?;
    let mut list = Vec::with_capacity(count as usize);
    if !activates.is_null() {
        // SAFETY: MF returned `count` initialised entries; each is moved out exactly once.
        for slot in unsafe { std::slice::from_raw_parts_mut(activates, count as usize) } {
            if let Some(activate) = slot.take() {
                let name = friendly_name(&activate).unwrap_or_else(|| "unnamed MFT".into());
                list.push(EncoderInfo {
                    activate,
                    name,
                    hardware,
                });
            }
        }
        // SAFETY: allocated by MFTEnum2 with CoTaskMemAlloc; every entry was taken.
        unsafe { CoTaskMemFree(Some(activates.cast())) };
    }
    Ok(list)
}

fn friendly_name(activate: &IMFActivate) -> Option<String> {
    let mut ptr = windows::core::PWSTR::null();
    let mut len = 0u32;
    // SAFETY: MF allocates the string with CoTaskMemAlloc; freed below.
    unsafe { activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut ptr, &mut len) }
        .ok()?;
    // SAFETY: a valid NUL-terminated string from MF.
    let name = unsafe { ptr.to_string() }.ok();
    // SAFETY: allocated by GetAllocatedString.
    unsafe { CoTaskMemFree(Some(ptr.as_ptr().cast())) };
    name
}
