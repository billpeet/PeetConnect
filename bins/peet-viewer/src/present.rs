//! Direct3D presentation through DirectComposition.
//!
//! GPUI's Windows window has no redirection surface (`WS_EX_NOREDIRECTIONBITMAP`) and draws
//! via its own DirectComposition target, so ordinary child windows never appear inside it.
//! Instead we attach a second composition target to the same HWND, below GPUI's, holding one
//! visual whose content is a composition swap chain. GPUI leaves the video area transparent
//! and our frames show through; GPUI can still draw chrome over the video.
//!
//! Decoded NV12 textures are converted by the video processor and copied into the back
//! buffer, so the display path never touches system memory.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use peet_capture::windows::D3dDevice;
use peet_codec::windows::{DecodedFrame, GpuConverter};
use windows::core::Interface;
use windows::Win32::Foundation::{HANDLE, HWND, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain2, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::Threading::WaitForSingleObject;

/// Where the video goes, in physical pixels relative to the window's client area. Written by
/// the UI thread on every layout, read by the presenter on every frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub type SharedPlacement = Arc<Mutex<Placement>>;

/// Presents decoded frames into a DirectComposition visual on a GPUI window.
pub struct Presenter {
    device: D3dDevice,
    comp: IDCompositionDevice,
    _target: IDCompositionTarget,
    visual: IDCompositionVisual,
    swapchain: IDXGISwapChain2,
    waitable: HANDLE,
    placement: SharedPlacement,
    size: (u32, u32),
    offset: (i32, i32),
    converter: Option<GpuConverter>,
    vsync: bool,
}

impl std::fmt::Debug for Presenter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Presenter")
            .field("size", &self.size)
            .field("offset", &self.offset)
            .field("vsync", &self.vsync)
            .finish_non_exhaustive()
    }
}

impl Presenter {
    /// `hwnd` is the GPUI window. Can be called from any thread; the presenter then belongs to
    /// the calling thread.
    pub fn new(
        device: D3dDevice,
        hwnd: HWND,
        placement: SharedPlacement,
        vsync: bool,
    ) -> Result<Self> {
        let initial = *placement.lock().expect("placement poisoned");
        let size = (
            initial.width.max(1).cast_unsigned(),
            initial.height.max(1).cast_unsigned(),
        );
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.0,
            Height: size.1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT
                .0
                .cast_unsigned(),
        };
        // SAFETY: plain COM calls on live objects; everything created is retained in `self`.
        let (comp, target, visual, swapchain, waitable) = unsafe {
            let dxgi_device: IDXGIDevice = device.device.cast()?;
            let comp: IDCompositionDevice = DCompositionCreateDevice(&dxgi_device)
                .context("creating DirectComposition device")?;
            // GPUI owns the topmost target; ours sits underneath and shows through the
            // transparent video area.
            let target = comp
                .CreateTargetForHwnd(hwnd, false)
                .context("creating composition target (is another one already attached?)")?;
            let visual = comp.CreateVisual()?;
            let factory: IDXGIFactory2 = device.adapter.GetParent()?;
            let swapchain = factory
                .CreateSwapChainForComposition(&device.device, &desc, None)
                .context("creating composition swap chain")?;
            let swapchain: IDXGISwapChain2 = swapchain.cast()?;
            swapchain.SetMaximumFrameLatency(1)?;
            let waitable = swapchain.GetFrameLatencyWaitableObject();
            visual.SetContent(&swapchain)?;
            visual.SetOffsetX2(initial.x as f32)?;
            visual.SetOffsetY2(initial.y as f32)?;
            target.SetRoot(&visual)?;
            comp.Commit()?;
            (comp, target, visual, swapchain, waitable)
        };
        tracing::info!(?initial, "composition presenter attached");
        Ok(Self {
            device,
            comp,
            _target: target,
            visual,
            swapchain,
            waitable,
            placement,
            size,
            offset: (initial.x, initial.y),
            converter: None,
            vsync,
        })
    }

    fn sync_placement(&mut self) -> Result<bool> {
        let p = *self.placement.lock().expect("placement poisoned");
        if p.width <= 0 || p.height <= 0 {
            return Ok(false);
        }
        let size = (p.width.cast_unsigned(), p.height.cast_unsigned());
        if size != self.size {
            self.converter = None;
            // SAFETY: no back buffer references are held across this call.
            unsafe {
                self.swapchain.ResizeBuffers(
                    0,
                    size.0,
                    size.1,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
                )?;
            }
            self.size = size;
            tracing::debug!(?size, "video surface resized");
        }
        if (p.x, p.y) != self.offset {
            // SAFETY: visual and device are live.
            unsafe {
                self.visual.SetOffsetX2(p.x as f32)?;
                self.visual.SetOffsetY2(p.y as f32)?;
                self.comp.Commit()?;
            }
            self.offset = (p.x, p.y);
        }
        Ok(true)
    }

    /// Converts and presents one frame, letterboxed to preserve aspect ratio. Blocks until
    /// the swap chain has room, which with a latency of one means roughly one vsync.
    pub fn present(&mut self, frame: &DecodedFrame) -> Result<()> {
        if !self.sync_placement()? {
            return Ok(());
        }
        let in_size = (frame.width, frame.height);
        if self
            .converter
            .as_ref()
            .is_none_or(|c| c.in_size() != in_size)
        {
            let converter = GpuConverter::nv12_to_bgra(
                &self.device.device,
                &self.device.context,
                in_size,
                self.size,
            )?;
            converter.set_dest_rect(Some(letterbox(in_size, self.size)));
            self.converter = Some(converter);
        }
        let converter = self.converter.as_mut().expect("converter just ensured");
        let bgra_frame = converter
            .convert(&frame.texture, frame.subresource)?
            .clone();

        // SAFETY: waitable handle and swap chain are live; the back buffer is used only here.
        unsafe {
            let wait = WaitForSingleObject(self.waitable, 1000);
            if wait != WAIT_OBJECT_0 {
                tracing::debug!(?wait, "swap chain wait did not signal");
            }
            let back_buffer: ID3D11Texture2D = self.swapchain.GetBuffer(0)?;
            self.device.context.CopyResource(&back_buffer, &bgra_frame);
            let hr = self
                .swapchain
                .Present(u32::from(self.vsync), DXGI_PRESENT(0));
            if hr != windows::core::HRESULT(0) {
                tracing::warn!(
                    code = format!("{:#010x}", hr.0),
                    "Present returned a non-S_OK status"
                );
            }
            hr.ok()?;
        }
        Ok(())
    }
}

/// Largest centred rectangle of `src` aspect inside `dst`.
fn letterbox(src: (u32, u32), dst: (u32, u32)) -> RECT {
    let (sw, sh) = (f64::from(src.0), f64::from(src.1));
    let (dw, dh) = (f64::from(dst.0), f64::from(dst.1));
    let scale = (dw / sw).min(dh / sh);
    let w = (sw * scale).round();
    let h = (sh * scale).round();
    let x = ((dw - w) / 2.0).round();
    let y = ((dh - h) / 2.0).round();
    RECT {
        left: x as i32,
        top: y as i32,
        right: (x + w) as i32,
        bottom: (y + h) as i32,
    }
}
