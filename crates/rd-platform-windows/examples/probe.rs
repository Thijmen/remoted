//! Lists what the Windows platform sees: monitors, DXGI adapters and outputs, H.264 encoders.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use rd_platform_windows::{d3d, display, mf};

    let _com = mf::ComThread::init();
    let _mf = mf::MfLib::start()?;
    println!("monitors:");
    for m in display::monitors() {
        println!("  {m:?}");
    }
    println!("adapters:");
    for (adapter, desc) in d3d::adapters()? {
        let name = String::from_utf16_lossy(&desc.Description);
        println!(
            "  {} luid={:#x} flags={:#x}",
            name.trim_end_matches('\0'),
            d3d::luid_u64(desc.AdapterLuid),
            desc.Flags
        );
        for j in 0.. {
            let Ok(output) = (unsafe { adapter.EnumOutputs(j) }) else {
                break;
            };
            let od = unsafe { output.GetDesc() }?;
            println!(
                "    output {} {:?} attached={}",
                String::from_utf16_lossy(&od.DeviceName).trim_end_matches('\0'),
                od.DesktopCoordinates,
                od.AttachedToDesktop.as_bool()
            );
        }
        for e in mf::h264_encoders(Some(desc.AdapterLuid))? {
            println!("    encoder {} (hardware {})", e.name, e.hardware);
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {}
