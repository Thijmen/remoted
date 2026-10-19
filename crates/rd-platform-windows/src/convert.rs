//! BGRA desktop -> NV12 frame on the GPU: the D3D11 video processor scales the desktop into
//! the frame (fitted into the content rect, black bars around it), converting full-range RGB
//! to BT.709 video-range Y'CbCr on the way.

use std::mem::ManuallyDrop;

use anyhow::{Context as _, ensure};
use tracing::debug;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0,
    D3D11_VIDEO_COLOR_YCbCrA, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoContext1,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL,
};
use windows::core::Interface as _;

use crate::d3d::Gpu;

/// `D3D11_VIDEO_PROCESSOR_COLOR_SPACE` bits: `YCbCr_Matrix` (1 = BT.709) and `Nominal_Range`
/// (bits 4-5; 1 = 16-235). `RGB_Range` 0 = full range.
const COLOR_SPACE_YCBCR_709_STUDIO: u32 = (1 << 2) | (1 << 4);
const COLOR_SPACE_RGB_FULL: u32 = 0;

/// Scaler for one input size and one output size.
pub struct Converter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// Views by texture (raw COM pointer, compared only). Inputs: the desktop copies;
    /// outputs: the encoder's surface pool.
    inputs: Vec<(usize, ID3D11VideoProcessorInputView)>,
    outputs: Vec<(usize, ID3D11VideoProcessorOutputView)>,
}

impl Converter {
    /// `input`: desktop size; `output`: frame size; `content`: where the desktop goes in the
    /// frame, `(left, top, right, bottom)` pixels.
    pub fn new(
        gpu: &Gpu,
        input: (u32, u32),
        output: (u32, u32),
        content: (i32, i32, i32, i32),
        fps: u32,
    ) -> anyhow::Result<Self> {
        let video_device: ID3D11VideoDevice = gpu
            .device
            .cast()
            .context("the D3D11 device has no video support")?;
        let video_context: ID3D11VideoContext = gpu
            .context
            .cast()
            .context("the D3D11 context has no video support")?;
        let rate = DXGI_RATIONAL {
            Numerator: fps.max(1),
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: input.0,
            InputHeight: input.1,
            OutputFrameRate: rate,
            OutputWidth: output.0,
            OutputHeight: output.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        // SAFETY (whole function): COM calls on valid objects; out pointers to locals.
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&desc) }
            .context("CreateVideoProcessorEnumerator")?;
        let supports = |format, flag: i32| {
            unsafe { enumerator.CheckVideoProcessorFormat(format) }
                .is_ok_and(|bits| bits & flag as u32 != 0)
        };
        ensure!(
            supports(
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0
            ),
            "the video processor cannot read BGRA"
        );
        ensure!(
            supports(
                DXGI_FORMAT_NV12,
                D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0
            ),
            "the video processor cannot write NV12"
        );
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("CreateVideoProcessor")?;
        let full_in = RECT {
            left: 0,
            top: 0,
            right: input.0 as i32,
            bottom: input.1 as i32,
        };
        let full_out = RECT {
            left: 0,
            top: 0,
            right: output.0 as i32,
            bottom: output.1 as i32,
        };
        let dest = RECT {
            left: content.0,
            top: content.1,
            right: content.2,
            bottom: content.3,
        };
        // Video-range black for the bars.
        let black = D3D11_VIDEO_COLOR {
            Anonymous: D3D11_VIDEO_COLOR_0 {
                YCbCr: D3D11_VIDEO_COLOR_YCbCrA {
                    Y: 16.0 / 255.0,
                    Cb: 0.5,
                    Cr: 0.5,
                    A: 1.0,
                },
            },
        };
        unsafe {
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&full_in));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dest));
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&full_out));
            video_context.VideoProcessorSetOutputBackgroundColor(&processor, true, &black);
            // No driver "enhancements" (denoise, edge sharpening, ...): desktop content.
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            match video_context.cast::<ID3D11VideoContext1>() {
                Ok(ctx1) => {
                    ctx1.VideoProcessorSetStreamColorSpace1(
                        &processor,
                        0,
                        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                    );
                    ctx1.VideoProcessorSetOutputColorSpace1(
                        &processor,
                        DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                    );
                }
                Err(_) => {
                    debug!("no ID3D11VideoContext1; using the legacy colour space API");
                    video_context.VideoProcessorSetStreamColorSpace(
                        &processor,
                        0,
                        &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                            _bitfield: COLOR_SPACE_RGB_FULL,
                        },
                    );
                    video_context.VideoProcessorSetOutputColorSpace(
                        &processor,
                        &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                            _bitfield: COLOR_SPACE_YCBCR_709_STUDIO,
                        },
                    );
                }
            }
        }
        Ok(Converter {
            video_device,
            video_context,
            enumerator,
            processor,
            inputs: Vec::new(),
            outputs: Vec::new(),
        })
    }

    /// Scales `input` (BGRA, the desktop size) into `output` (NV12, the frame size).
    pub fn convert(
        &mut self,
        input: &ID3D11Texture2D,
        output: &ID3D11Texture2D,
    ) -> anyhow::Result<()> {
        let input_view = self.input_view(input)?;
        let output_view = self.output_view(output)?;
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(input_view)),
            ..Default::default()
        };
        let mut streams = [stream];
        // SAFETY: valid processor and views; one enabled stream.
        let result = unsafe {
            self.video_context
                .VideoProcessorBlt(&self.processor, &output_view, 0, &streams)
        };
        // SAFETY: releases the reference taken for the stream above, exactly once.
        unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        result.context("VideoProcessorBlt")
    }

    fn input_view(
        &mut self,
        texture: &ID3D11Texture2D,
    ) -> anyhow::Result<ID3D11VideoProcessorInputView> {
        let key = texture.as_raw() as usize;
        if let Some((_, view)) = self.inputs.iter().find(|(k, _)| *k == key) {
            return Ok(view.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut view = None;
        // SAFETY: valid texture and enumerator; out pointer to a local.
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                texture,
                &self.enumerator,
                &desc,
                Some(&mut view),
            )
        }
        .context("CreateVideoProcessorInputView")?;
        let view = view.context("no input view")?;
        // The view keeps its texture alive, so the key cannot be reused while cached.
        self.inputs.push((key, view.clone()));
        Ok(view)
    }

    fn output_view(
        &mut self,
        texture: &ID3D11Texture2D,
    ) -> anyhow::Result<ID3D11VideoProcessorOutputView> {
        let key = texture.as_raw() as usize;
        if let Some((_, view)) = self.outputs.iter().find(|(k, _)| *k == key) {
            return Ok(view.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut view = None;
        // SAFETY: valid texture and enumerator; out pointer to a local.
        unsafe {
            self.video_device.CreateVideoProcessorOutputView(
                texture,
                &self.enumerator,
                &desc,
                Some(&mut view),
            )
        }
        .context("CreateVideoProcessorOutputView")?;
        let view = view.context("no output view")?;
        // A surface pool has a handful of textures; an encoder rebuild brings a new pool.
        if self.outputs.len() >= 32 {
            self.outputs.clear();
        }
        self.outputs.push((key, view.clone()));
        Ok(view)
    }
}
