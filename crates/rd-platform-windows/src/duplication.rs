//! DXGI Desktop Duplication of one output.
//!
//! [`Duplication::next`] waits for the next desktop update, copies the desktop image into a
//! texture of ours (Desktop Duplication's own surface must be released quickly) and tracks the
//! pointer, which is delivered separately (see [`crate::cursor`]).
//!
//! A duplication ends whenever Windows switches desktops (lock screen, UAC prompt,
//! Ctrl+Alt+Del), changes the display mode or a full-screen app takes over the output. The
//! capture then opens a new one; while the secure desktop is up, that is refused
//! ([`DuplError::Denied`]) and retried until the user is back.

use anyhow::Context as _;
use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_IDENTITY,
    DXGI_MODE_ROTATION_UNSPECIFIED, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_DENIED, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_SESSION_DISCONNECTED, DXGI_ERROR_UNSUPPORTED,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO, IDXGIOutput,
    IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface as _;

use crate::cursor::{Pointer, Shape};
use crate::d3d::Gpu;

/// Why a duplication could not be opened or continued.
#[derive(Debug)]
pub enum DuplError {
    /// Windows refuses duplication right now: the secure desktop (lock screen, UAC,
    /// Ctrl+Alt+Del) is up, the session is disconnected, or too many apps duplicate the output.
    /// Retry later.
    Denied(windows::core::Error),
    /// The duplication ended (desktop switch, mode change); open a new one.
    Lost(windows::core::Error),
    /// The D3D device is gone (driver update, GPU reset); rebuild everything.
    DeviceLost(windows::core::Error),
    /// This output cannot be duplicated on this device (e.g. a hybrid-GPU laptop's other GPU).
    Unsupported(windows::core::Error),
}

impl std::fmt::Display for DuplError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DuplError::Denied(e) => write!(f, "desktop duplication refused for now: {e}"),
            DuplError::Lost(e) => write!(f, "desktop duplication lost: {e}"),
            DuplError::DeviceLost(e) => write!(f, "the D3D device was removed: {e}"),
            DuplError::Unsupported(e) => write!(f, "desktop duplication unsupported: {e}"),
        }
    }
}

impl std::error::Error for DuplError {}

fn classify(e: windows::core::Error) -> DuplError {
    let code = e.code();
    if code == E_ACCESSDENIED
        || code == DXGI_ERROR_ACCESS_DENIED
        || code == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE
        || code == DXGI_ERROR_SESSION_DISCONNECTED
    {
        DuplError::Denied(e)
    } else if code == DXGI_ERROR_DEVICE_REMOVED || code == DXGI_ERROR_DEVICE_RESET {
        DuplError::DeviceLost(e)
    } else if code == DXGI_ERROR_UNSUPPORTED {
        DuplError::Unsupported(e)
    } else {
        DuplError::Lost(e)
    }
}

/// What [`Duplication::next`] saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// Nothing changed within the timeout.
    Timeout,
    /// `desktop`: the image changed (and was copied); `pointer`: its position, visibility or
    /// shape changed.
    Frame { desktop: bool, pointer: bool },
}

/// An open duplication of one output.
pub struct Duplication {
    dupl: IDXGIOutputDuplication,
    /// Desktop image size, pixels.
    pub width: u32,
    pub height: u32,
    pub rotation: DXGI_MODE_ROTATION,
    pub pointer: Pointer,
    shape_buf: Vec<u8>,
    /// A frame is acquired and must be released before the next acquire.
    holding: bool,
}

impl Duplication {
    /// Duplicates `output` on `gpu` (which must be on the output's adapter).
    pub fn open(gpu: &Gpu, output: &IDXGIOutput) -> Result<Self, DuplError> {
        // SAFETY: valid output and device. DuplicateOutput1 needs per-monitor DPI awareness
        // (set by the platform); it also lets Windows convert HDR desktops to 8-bit BGRA.
        let dupl = match output.cast::<IDXGIOutput5>() {
            Ok(o5) => unsafe { o5.DuplicateOutput1(&gpu.device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]) },
            Err(_) => {
                let o1: IDXGIOutput1 = output.cast().map_err(DuplError::Unsupported)?;
                unsafe { o1.DuplicateOutput(&gpu.device) }
            }
        }
        .map_err(classify)?;
        // SAFETY: valid duplication.
        let desc = unsafe { dupl.GetDesc() };
        Ok(Duplication {
            dupl,
            width: desc.ModeDesc.Width,
            height: desc.ModeDesc.Height,
            rotation: desc.Rotation,
            pointer: Pointer::default(),
            shape_buf: Vec::new(),
            holding: false,
        })
    }

    /// The display is rotated (portrait): its image arrives in scan-out orientation.
    pub fn rotated(&self) -> bool {
        self.rotation != DXGI_MODE_ROTATION_IDENTITY
            && self.rotation != DXGI_MODE_ROTATION_UNSPECIFIED
    }

    /// Waits up to `timeout_ms` for an update; copies a changed desktop image into `desktop`
    /// (BGRA, this duplication's size).
    pub fn next(
        &mut self,
        timeout_ms: u32,
        desktop: &ID3D11Texture2D,
        gpu: &Gpu,
    ) -> Result<Update, DuplError> {
        if self.holding {
            // SAFETY: a frame is held.
            let _ = unsafe { self.dupl.ReleaseFrame() };
            self.holding = false;
        }
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: out pointers to locals.
        match unsafe {
            self.dupl
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Update::Timeout),
            Err(e) => return Err(classify(e)),
        }
        self.holding = true;
        let result = self.take(&info, resource, desktop, gpu);
        // SAFETY: the frame acquired above.
        let released = unsafe { self.dupl.ReleaseFrame() };
        self.holding = false;
        let update = result?;
        released.map_err(classify)?;
        Ok(update)
    }

    fn take(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
        desktop: &ID3D11Texture2D,
        gpu: &Gpu,
    ) -> Result<Update, DuplError> {
        let mut changed_desktop = false;
        if info.LastPresentTime != 0
            && let Some(resource) = resource
        {
            let image: ID3D11Texture2D = resource.cast().map_err(DuplError::Lost)?;
            // SAFETY: both BGRA textures of the duplication's size on `gpu`.
            unsafe { gpu.context.CopyResource(desktop, &image) };
            changed_desktop = true;
        }
        let mut changed_pointer = false;
        if info.LastMouseUpdateTime != 0 {
            let pos = info.PointerPosition;
            let visible = pos.Visible.as_bool();
            let (x, y) = (pos.Position.x, pos.Position.y);
            if visible != self.pointer.visible
                || (visible && (x, y) != (self.pointer.x, self.pointer.y))
            {
                changed_pointer = true;
            }
            self.pointer.visible = visible;
            if visible {
                self.pointer.x = x;
                self.pointer.y = y;
            }
        }
        if info.PointerShapeBufferSize > 0 {
            self.shape_buf
                .resize(info.PointerShapeBufferSize as usize, 0);
            let mut required = 0u32;
            let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            // SAFETY: the buffer has the size the frame info announced.
            unsafe {
                self.dupl.GetFramePointerShape(
                    self.shape_buf.len() as u32,
                    self.shape_buf.as_mut_ptr().cast(),
                    &mut required,
                    &mut shape,
                )
            }
            .map_err(classify)?;
            let data =
                self.shape_buf[..required.min(self.shape_buf.len() as u32) as usize].to_vec();
            let shape = Shape::new(shape.Type, shape.Width, shape.Height, shape.Pitch, data);
            if shape != self.pointer.shape {
                self.pointer.shape = shape;
                changed_pointer = true;
            }
        }
        Ok(Update::Frame {
            desktop: changed_desktop,
            pointer: changed_pointer,
        })
    }
}

impl Drop for Duplication {
    fn drop(&mut self) {
        if self.holding {
            // SAFETY: a frame is held.
            let _ = unsafe { self.dupl.ReleaseFrame() };
        }
    }
}

/// A BGRA texture the video processor can read and the pointer can be drawn into.
pub fn desktop_texture(gpu: &Gpu, width: u32, height: u32) -> anyhow::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: valid description; out pointer to a local.
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .with_context(|| format!("creating a {width}x{height} BGRA texture"))?;
    texture.context("CreateTexture2D returned no texture")
}
