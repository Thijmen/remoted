//! ScreenCaptureKit -> VideoToolbox pipeline. TO BE IMPLEMENTED.

use std::sync::Arc;

use rd_agent_core::{FrameGate, FrameSink, VideoConfig, VideoPipeline};

pub struct ScreenPipeline {}

impl ScreenPipeline {
    pub fn new() -> anyhow::Result<Self> {
        Ok(ScreenPipeline {})
    }
}

impl VideoPipeline for ScreenPipeline {
    fn start(
        &mut self,
        cfg: VideoConfig,
        gate: Arc<dyn FrameGate>,
        sink: FrameSink,
    ) -> anyhow::Result<()> {
        let _ = (cfg, gate, sink);
        todo!("rd-platform-macos: ScreenPipeline::start")
    }
    fn reconfigure(&mut self, cfg: VideoConfig) -> anyhow::Result<()> {
        let _ = cfg;
        todo!()
    }
    fn set_bitrate(&self, bps: u32) {
        let _ = bps;
        todo!()
    }
    fn stop(&mut self) {
        todo!()
    }
}
