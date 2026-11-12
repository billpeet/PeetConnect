//! Direct3D presentation through DirectComposition.
//!
//! GPUI's Windows window has no redirection surface (`WS_EX_NOREDIRECTIONBITMAP`) and draws
//! via its own DirectComposition target, so ordinary child windows never appear inside it.
//! Instead we attach a second composition target to the same HWND, below GPUI's, holding one
//! visual whose content is a composition swap chain. GPUI leaves the video area transparent
//! and our frames show through; GPUI can still draw chrome over the video.
//!
//! Several monitors share the one swap chain: each visible monitor gets its own converter
//! whose output lands in that monitor's tile, and every present composites the latest tile
//! of each monitor into the back buffer. Decoded NV12 textures are converted by the video
//! processor on the GPU, so the display path never touches system memory.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use peet_capture::windows::D3dDevice;
use peet_codec::windows::{DecodedFrame, GpuConverter};
use peet_proto::{MonitorId, MonitorInfo};
use windows::core::Interface;
use windows::Win32::Foundation::{HANDLE, HWND, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{ID3D11RenderTargetView, ID3D11Texture2D, D3D11_BOX};
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

/// Where one monitor is drawn: `rect` in physical pixels relative to the window's client
/// area, `bounds` the monitor's rectangle in the host's virtual desktop. Published by the
/// presenter so the UI can map pointer positions and place the cursor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tile {
    pub monitor: MonitorId,
    pub rect: RECT,
    pub bounds: peet_proto::Rect,
}

pub type SharedTiles = Arc<Mutex<Vec<Tile>>>;

/// Per-monitor conversion state.
struct Slot {
    converter: GpuConverter,
    /// Most recent converted output, composited on every present.
    latest: Option<ID3D11Texture2D>,
    /// Destination within the swap chain.
    rect: RECT,
}

/// Presents decoded frames into a DirectComposition visual on a GPUI window.
pub struct Presenter {
    device: D3dDevice,
    comp: IDCompositionDevice,
    _target: IDCompositionTarget,
    visual: IDCompositionVisual,
    swapchain: IDXGISwapChain2,
    waitable: HANDLE,
    placement: SharedPlacement,
    tiles: SharedTiles,
    size: (u32, u32),
    offset: (i32, i32),
    slots: HashMap<MonitorId, Slot>,
    /// Monitors (in order) the current layout was computed for.
    layout_for: Vec<MonitorId>,
    rects: HashMap<MonitorId, RECT>,
    vsync: bool,
}

impl std::fmt::Debug for Presenter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Presenter")
            .field("size", &self.size)
            .field("offset", &self.offset)
            .field("monitors", &self.layout_for)
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
        tiles: SharedTiles,
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
            tiles,
            size,
            offset: (initial.x, initial.y),
            slots: HashMap::new(),
            layout_for: Vec::new(),
            rects: HashMap::new(),
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
            self.slots.clear();
            self.layout_for.clear();
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
            self.layout_for.clear();
        }
        Ok(true)
    }

    /// Recomputes tiles when the set of visible monitors (or the surface) changed.
    fn sync_layout(&mut self, visible: &[MonitorInfo]) {
        let ids: Vec<MonitorId> = visible.iter().map(|m| m.id).collect();
        if ids == self.layout_for {
            return;
        }
        self.layout_for = ids;
        self.rects = layout(visible, self.size);
        self.slots.retain(|id, _| self.rects.contains_key(id));
        for (id, slot) in &mut self.slots {
            let rect = self.rects[id];
            if slot.rect != rect {
                slot.rect = rect;
                slot.converter.set_dest_rect(Some(rect));
                slot.latest = None;
            }
        }
        let (ox, oy) = self.offset;
        let tiles: Vec<Tile> = visible
            .iter()
            .filter_map(|m| {
                let r = self.rects.get(&m.id)?;
                Some(Tile {
                    monitor: m.id,
                    rect: RECT {
                        left: r.left + ox,
                        top: r.top + oy,
                        right: r.right + ox,
                        bottom: r.bottom + oy,
                    },
                    bounds: m.bounds,
                })
            })
            .collect();
        *self.tiles.lock().expect("tiles poisoned") = tiles;
    }

    /// Converts one monitor's frame into its tile and presents the composite of every visible
    /// monitor's latest tile. Blocks until the swap chain has room, which with a latency of
    /// one means roughly one vsync.
    pub fn present(
        &mut self,
        monitor: MonitorId,
        frame: &DecodedFrame,
        visible: &[MonitorInfo],
    ) -> Result<()> {
        if !self.sync_placement()? {
            return Ok(());
        }
        self.sync_layout(visible);
        let Some(&rect) = self.rects.get(&monitor) else {
            return Ok(());
        };
        let in_size = (frame.width, frame.height);
        if self
            .slots
            .get(&monitor)
            .is_some_and(|s| s.converter.in_size() != in_size)
        {
            self.slots.remove(&monitor);
        }
        if !self.slots.contains_key(&monitor) {
            let converter = GpuConverter::nv12_to_bgra(
                &self.device.device,
                &self.device.context,
                in_size,
                self.size,
            )?;
            converter.set_dest_rect(Some(rect));
            self.slots.insert(
                monitor,
                Slot {
                    converter,
                    latest: None,
                    rect,
                },
            );
        }
        let slot = self.slots.get_mut(&monitor).expect("slot just ensured");
        let converted = slot
            .converter
            .convert(&frame.texture, frame.subresource)?
            .clone();
        slot.latest = Some(converted);

        // SAFETY: waitable handle and swap chain are live; the back buffer is used only here
        // and every source texture is owned by a converter that outlives this call.
        unsafe {
            let wait = WaitForSingleObject(self.waitable, 1000);
            if wait != WAIT_OBJECT_0 {
                tracing::debug!(?wait, "swap chain wait did not signal");
            }
            let back_buffer: ID3D11Texture2D = self.swapchain.GetBuffer(0)?;
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            self.device
                .device
                .CreateRenderTargetView(&back_buffer, None, Some(&mut rtv))?;
            if let Some(rtv) = rtv {
                self.device
                    .context
                    .ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]);
            }
            for slot in self.slots.values() {
                let Some(texture) = &slot.latest else {
                    continue;
                };
                let r = slot.rect;
                let b = D3D11_BOX {
                    left: r.left.max(0).cast_unsigned(),
                    top: r.top.max(0).cast_unsigned(),
                    front: 0,
                    right: r.right.max(0).cast_unsigned(),
                    bottom: r.bottom.max(0).cast_unsigned(),
                    back: 1,
                };
                self.device.context.CopySubresourceRegion(
                    &back_buffer,
                    0,
                    b.left,
                    b.top,
                    0,
                    texture,
                    0,
                    Some(&b),
                );
            }
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

/// Lays the visible monitors out as they sit in the host's virtual desktop, scaled uniformly
/// to fit `out` and centred. With one monitor this is a plain letterbox.
fn layout(visible: &[MonitorInfo], out: (u32, u32)) -> HashMap<MonitorId, RECT> {
    let mut rects = HashMap::new();
    if visible.is_empty() {
        return rects;
    }
    let min_x = visible.iter().map(|m| m.bounds.x).min().unwrap_or(0);
    let min_y = visible.iter().map(|m| m.bounds.y).min().unwrap_or(0);
    let max_x = visible
        .iter()
        .map(|m| i64::from(m.bounds.x) + i64::from(m.bounds.width))
        .max()
        .unwrap_or(1);
    let max_y = visible
        .iter()
        .map(|m| i64::from(m.bounds.y) + i64::from(m.bounds.height))
        .max()
        .unwrap_or(1);
    let bw = (max_x - i64::from(min_x)).max(1) as f64;
    let bh = (max_y - i64::from(min_y)).max(1) as f64;
    let (dw, dh) = (f64::from(out.0), f64::from(out.1));
    let scale = (dw / bw).min(dh / bh);
    let off_x = ((dw - bw * scale) / 2.0).round();
    let off_y = ((dh - bh * scale) / 2.0).round();
    for m in visible {
        let left = off_x + f64::from(m.bounds.x - min_x) * scale;
        let top = off_y + f64::from(m.bounds.y - min_y) * scale;
        let right = left + f64::from(m.bounds.width) * scale;
        let bottom = top + f64::from(m.bounds.height) * scale;
        rects.insert(
            m.id,
            RECT {
                left: left.round() as i32,
                top: top.round() as i32,
                right: right.round() as i32,
                bottom: bottom.round() as i32,
            },
        );
    }
    rects
}
