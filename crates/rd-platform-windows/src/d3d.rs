//! Direct3D 11 devices and DXGI outputs.

use anyhow::{Context as _, bail};
use tracing::debug;
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP,
    D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_OUTPUT_DESC,
    IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput,
};
use windows::core::Interface as _;

/// A D3D11 device with its immediate context. The device is multithread protected: Media
/// Foundation uses it from its own threads.
#[derive(Clone)]
pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub luid: LUID,
    /// Adapter description, e.g. "NVIDIA GeForce RTX 3090".
    pub name: String,
    /// WARP or another software adapter.
    pub software: bool,
}

// SAFETY: D3D11 devices are free-threaded; the immediate context is not, but the device is
// multithread protected (`SetMultithreadProtected`), which serialises every context call.
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

impl std::fmt::Debug for Gpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gpu")
            .field("name", &self.name)
            .field("luid", &luid_u64(self.luid))
            .field("software", &self.software)
            .finish()
    }
}

pub fn luid_u64(luid: LUID) -> u64 {
    (u64::from(luid.HighPart as u32) << 32) | u64::from(luid.LowPart)
}

fn wide(s: &[u16]) -> String {
    let len = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..len])
}

/// Every adapter, in DXGI order (the first is the one with the primary display).
pub fn adapters() -> anyhow::Result<Vec<(IDXGIAdapter1, DXGI_ADAPTER_DESC1)>> {
    // SAFETY: plain factory creation.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    let mut list = Vec::new();
    for i in 0.. {
        // SAFETY: valid factory; DXGI_ERROR_NOT_FOUND ends the enumeration.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        // SAFETY: valid adapter.
        let desc = unsafe { adapter.GetDesc1() }.context("IDXGIAdapter1::GetDesc1")?;
        list.push((adapter, desc));
    }
    Ok(list)
}

/// The DXGI output showing GDI device `device` (e.g. `\\.\DISPLAY1`) and its adapter.
pub fn find_output(device: &str) -> anyhow::Result<(IDXGIAdapter1, IDXGIOutput, DXGI_OUTPUT_DESC)> {
    for (adapter, _) in adapters()? {
        for j in 0.. {
            // SAFETY: valid adapter; DXGI_ERROR_NOT_FOUND ends the enumeration.
            let Ok(output) = (unsafe { adapter.EnumOutputs(j) }) else {
                break;
            };
            // SAFETY: valid output.
            let desc = unsafe { output.GetDesc() }.context("IDXGIOutput::GetDesc")?;
            if wide(&desc.DeviceName).eq_ignore_ascii_case(device) {
                return Ok((adapter, output, desc));
            }
        }
    }
    bail!("no DXGI output for {device}")
}

/// A device on `adapter`, or on the default hardware adapter. Falls back to WARP when there
/// is no hardware device (only without an explicit adapter).
pub fn create_device(adapter: Option<&IDXGIAdapter1>) -> anyhow::Result<Gpu> {
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
    let result = match adapter {
        Some(a) => create(Some(a), D3D_DRIVER_TYPE_UNKNOWN, flags),
        None => create(None, D3D_DRIVER_TYPE_HARDWARE, flags).or_else(|e| {
            debug!("no hardware D3D11 device ({e:#}); trying WARP");
            create(None, D3D_DRIVER_TYPE_WARP, flags)
                .or_else(|_| create(None, D3D_DRIVER_TYPE_WARP, D3D11_CREATE_DEVICE_BGRA_SUPPORT))
        }),
    };
    let (device, context) = result?;
    // SAFETY: valid device.
    let dxgi_adapter: IDXGIAdapter = unsafe {
        device
            .cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice>()?
            .GetAdapter()
    }
    .context("IDXGIDevice::GetAdapter")?;
    let adapter1: IDXGIAdapter1 = dxgi_adapter.cast()?;
    // SAFETY: valid adapter.
    let desc = unsafe { adapter1.GetDesc1() }.context("IDXGIAdapter1::GetDesc1")?;
    let multithread: ID3D11Multithread = device.cast()?;
    // SAFETY: valid interface; the return value is the previous setting.
    let _ = unsafe { multithread.SetMultithreadProtected(true) };
    Ok(Gpu {
        device,
        context,
        luid: desc.AdapterLuid,
        name: wide(&desc.Description),
        software: desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
    })
}

fn create(
    adapter: Option<&IDXGIAdapter1>,
    driver: D3D_DRIVER_TYPE,
    flags: D3D11_CREATE_DEVICE_FLAG,
) -> anyhow::Result<(ID3D11Device, ID3D11DeviceContext)> {
    const LEVELS: [D3D_FEATURE_LEVEL; 4] = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    let adapter: Option<IDXGIAdapter> = adapter.map(|a| a.cast()).transpose()?;
    let mut device = None;
    let mut context = None;
    // SAFETY: out pointers to locals; an explicit adapter requires D3D_DRIVER_TYPE_UNKNOWN.
    unsafe {
        D3D11CreateDevice(
            adapter.as_ref(),
            driver,
            HMODULE::default(),
            flags,
            Some(&LEVELS),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .with_context(|| format!("D3D11CreateDevice({driver:?})"))?;
    Ok((
        device.context("D3D11CreateDevice returned no device")?,
        context.context("D3D11CreateDevice returned no context")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_device() {
        let gpu = create_device(None).expect("a D3D11 device (hardware or WARP)");
        assert!(!gpu.name.is_empty());
        println!("{gpu:?}");
    }

    #[test]
    fn luid_packs_both_halves() {
        let luid = LUID {
            LowPart: 0x1234,
            HighPart: 1,
        };
        assert_eq!(luid_u64(luid), 0x1_0000_1234);
    }
}
