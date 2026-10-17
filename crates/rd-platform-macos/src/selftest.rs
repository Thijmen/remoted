//! `rd-agent selftest`: capture+encode for N seconds, report fps/encode latency/IDR size/codec
//! string, assert SPS VUI `bitstream_restriction_flag == 1 && max_num_reorder_frames == 0`,
//! optionally record an `.rdv` file. TO BE IMPLEMENTED.

#[derive(Debug, Clone)]
pub struct SelftestOptions {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub seconds: u32,
    pub bitrate_kbps: u32,
    pub record: Option<std::path::PathBuf>,
}

/// Returns Ok(true) if every assertion passed.
pub fn run(opts: &SelftestOptions) -> anyhow::Result<bool> {
    let _ = opts;
    todo!("rd-platform-macos: selftest::run")
}
