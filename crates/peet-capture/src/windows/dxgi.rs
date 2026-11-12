use std::time::{Duration, Instant};

use peet_proto::Rect;
use windows::core::Interface;
use windows::Win32::Foundation::{E_ACCESSDENIED, RECT};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_BIND_FLAG, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CPU_ACCESS_FLAG, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};

use super::device::D3dDevice;
use crate::{
    CaptureError, CpuFrame, CursorShapeData, CursorShapeKind, CursorState, FrameMeta, MonitorDesc,
    MoveRect,
};

/// Captures one monitor via DXGI Desktop Duplication.
///
/// Each successful [`acquire`](Self::acquire) copies the desktop image into a texture owned by
/// this capturer and releases the duplication frame immediately, so the compositor is never
/// blocked on us. The texture is available through [`texture`](Self::texture) for GPU
/// consumers, or [`read_cpu`](Self::read_cpu) for a system-memory copy.
pub struct DxgiCapturer {
    device: D3dDevice,
    monitor: MonitorDesc,
    duplication: IDXGIOutputDuplication,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    frame: ID3D11Texture2D,
    staging: Option<ID3D11Texture2D>,
    /// Scratch for move/dirty rect metadata. `u32`-backed so the casts below are aligned.
    metadata: Vec<u32>,
    shape_buf: Vec<u8>,
}

impl std::fmt::Debug for DxgiCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DxgiCapturer")
            .field("monitor", &self.monitor.name)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl DxgiCapturer {
    /// Starts duplicating `monitor` on `device`. The device must sit on the monitor's adapter.
    pub fn new(device: D3dDevice, monitor: &MonitorDesc) -> Result<Self, CaptureError> {
        if device.adapter_index != monitor.adapter_index {
            return Err(CaptureError::Unsupported(format!(
                "device is on adapter {} but monitor {} is on adapter {}",
                device.adapter_index, monitor.name, monitor.adapter_index
            )));
        }
        // SAFETY: plain COM calls on live interfaces.
        let duplication = unsafe {
            let output = device
                .adapter
                .EnumOutputs(monitor.output_index)
                .map_err(|_| CaptureError::MonitorNotFound(monitor.id))?;
            // Prefer the newer entry point, which also handles HDR and 10-bit desktops and
            // lets us name the formats we can consume. Fall back to the original API.
            let via_output5 = output
                .cast::<IDXGIOutput5>()
                .and_then(|o5| o5.DuplicateOutput1(&device.device, 0, &SUPPORTED_FORMATS));
            match via_output5 {
                Ok(dup) => dup,
                Err(err) => {
                    tracing::debug!(%err, "DuplicateOutput1 failed; using DuplicateOutput");
                    let output1: IDXGIOutput1 = output.cast()?;
                    output1
                        .DuplicateOutput(&device.device)
                        .map_err(map_dup_error)?
                }
            }
        };
        // SAFETY: GetDesc fills a plain struct.
        let desc = unsafe { duplication.GetDesc() };
        let width = desc.ModeDesc.Width;
        let height = desc.ModeDesc.Height;
        let format = desc.ModeDesc.Format;
        if format != DXGI_FORMAT_B8G8R8A8_UNORM {
            tracing::warn!(
                ?format,
                "desktop is not 8-bit BGRA (HDR?); CPU readback unavailable"
            );
        }
        let frame = device.create_texture(
            width,
            height,
            format,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
            D3D11_CPU_ACCESS_FLAG(0),
        )?;
        tracing::debug!(monitor = %monitor.name, width, height, ?format, "desktop duplication started");
        Ok(Self {
            device,
            monitor: monitor.clone(),
            duplication,
            width,
            height,
            format,
            frame,
            staging: None,
            metadata: Vec::new(),
            shape_buf: Vec::new(),
        })
    }

    pub fn device(&self) -> &D3dDevice {
        &self.device
    }

    pub fn monitor(&self) -> &MonitorDesc {
        &self.monitor
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The most recently acquired desktop image on the GPU, in [`format`](Self::format).
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.frame
    }

    /// Pixel format of [`texture`](Self::texture); BGRA8 on SDR desktops.
    pub fn format(&self) -> DXGI_FORMAT {
        self.format
    }

    /// Waits up to `timeout` for the desktop or cursor to change, then copies the new image
    /// into [`texture`](Self::texture).
    pub fn acquire(&mut self, timeout: Duration) -> Result<FrameMeta, CaptureError> {
        let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: out-pointers are valid; on success ReleaseFrame is guaranteed by the guard.
        unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        }
        .map_err(map_acquire_error)?;
        let _release = ReleaseGuard(self.duplication.clone());
        let captured_at = Instant::now();

        let mut meta = FrameMeta {
            captured_at: Some(captured_at),
            desktop_updated: info.LastPresentTime != 0,
            accumulated_frames: info.AccumulatedFrames,
            ..Default::default()
        };

        if meta.desktop_updated {
            let resource =
                resource.ok_or_else(|| CaptureError::Unsupported("no desktop resource".into()))?;
            let texture: ID3D11Texture2D = resource.cast()?;
            // SAFETY: both textures are the same size and format on the same device.
            unsafe { self.device.context.CopyResource(&self.frame, &texture) };
            if info.TotalMetadataBufferSize > 0 {
                self.read_metadata(info.TotalMetadataBufferSize, &mut meta)?;
            }
        }

        if info.LastMouseUpdateTime != 0 {
            meta.cursor = Some(self.read_cursor(&info)?);
        }
        Ok(meta)
    }

    /// Copies the current texture into `out`, reallocating as needed.
    pub fn read_cpu(&mut self, out: &mut CpuFrame) -> Result<(), CaptureError> {
        if self.format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(CaptureError::Unsupported(format!(
                "CPU readback of {:?} desktops is not implemented",
                self.format
            )));
        }
        if self.staging.is_none() {
            self.staging = Some(self.device.create_texture(
                self.width,
                self.height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_USAGE_STAGING,
                D3D11_BIND_FLAG(0),
                D3D11_CPU_ACCESS_READ,
            )?);
        }
        let staging = self.staging.as_ref().expect("staging texture just created");
        let ctx = &self.device.context;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging matches frame in size and format; Map/Unmap are paired.
        unsafe {
            ctx.CopyResource(staging, &self.frame);
            ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let row_bytes = self.width as usize * 4;
        let src_pitch = mapped.RowPitch as usize;
        out.width = self.width;
        out.height = self.height;
        out.stride = row_bytes;
        out.bgra.resize(row_bytes * self.height as usize, 0);
        // SAFETY: the mapped region is RowPitch * height bytes and stays valid until Unmap.
        unsafe {
            let src = std::slice::from_raw_parts(
                mapped.pData.cast::<u8>(),
                src_pitch * self.height as usize,
            );
            for (y, dst_row) in out.bgra.chunks_exact_mut(row_bytes).enumerate() {
                let start = y * src_pitch;
                dst_row.copy_from_slice(&src[start..start + row_bytes]);
            }
            ctx.Unmap(staging, 0);
        }
        Ok(())
    }

    fn read_metadata(&mut self, total: u32, meta: &mut FrameMeta) -> Result<(), CaptureError> {
        self.metadata.resize(total.div_ceil(4) as usize, 0);
        let mut moved_bytes = 0u32;
        // SAFETY: buffer is `total` bytes, which DXGI promised is enough for both lists.
        unsafe {
            self.duplication.GetFrameMoveRects(
                total,
                self.metadata.as_mut_ptr().cast::<DXGI_OUTDUPL_MOVE_RECT>(),
                &mut moved_bytes,
            )?;
        }
        let move_count = moved_bytes as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
        // SAFETY: the first moved_bytes bytes hold move_count initialised structs.
        let moves = unsafe {
            std::slice::from_raw_parts(
                self.metadata.as_ptr().cast::<DXGI_OUTDUPL_MOVE_RECT>(),
                move_count,
            )
        };
        meta.moved = moves
            .iter()
            .map(|m| MoveRect {
                source_x: m.SourcePoint.x,
                source_y: m.SourcePoint.y,
                destination: rect_from(&m.DestinationRect),
            })
            .collect();

        let remaining = total - moved_bytes;
        let mut dirty_bytes = 0u32;
        // SAFETY: the tail of the buffer after the move rects has `remaining` bytes.
        unsafe {
            self.duplication.GetFrameDirtyRects(
                remaining,
                self.metadata
                    .as_mut_ptr()
                    .add(moved_bytes as usize / 4)
                    .cast::<RECT>(),
                &mut dirty_bytes,
            )?;
        }
        let dirty_count = dirty_bytes as usize / std::mem::size_of::<RECT>();
        // SAFETY: dirty_count RECTs were written at that offset.
        let dirty = unsafe {
            std::slice::from_raw_parts(
                self.metadata
                    .as_ptr()
                    .add(moved_bytes as usize / 4)
                    .cast::<RECT>(),
                dirty_count,
            )
        };
        meta.dirty = dirty.iter().map(rect_from).collect();
        Ok(())
    }

    fn read_cursor(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO) -> Result<CursorState, CaptureError> {
        let mut shape = None;
        if info.PointerShapeBufferSize > 0 {
            self.shape_buf
                .resize(info.PointerShapeBufferSize as usize, 0);
            let mut required = 0u32;
            let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            // SAFETY: buffer sized from PointerShapeBufferSize as DXGI requires.
            unsafe {
                self.duplication.GetFramePointerShape(
                    info.PointerShapeBufferSize,
                    self.shape_buf.as_mut_ptr().cast(),
                    &mut required,
                    &mut shape_info,
                )?;
            }
            let kind = match shape_info.Type {
                t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0.cast_unsigned() => {
                    CursorShapeKind::Monochrome
                }
                t if t
                    == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR
                        .0
                        .cast_unsigned() =>
                {
                    CursorShapeKind::MaskedColor
                }
                t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0.cast_unsigned() => {
                    CursorShapeKind::Color
                }
                _ => CursorShapeKind::Color,
            };
            shape = Some(CursorShapeData {
                width: shape_info.Width,
                height: shape_info.Height,
                hotspot_x: shape_info.HotSpot.x.max(0).cast_unsigned(),
                hotspot_y: shape_info.HotSpot.y.max(0).cast_unsigned(),
                kind,
                pitch: shape_info.Pitch,
                data: self.shape_buf[..required as usize].to_vec(),
            });
        }
        Ok(CursorState {
            x: info.PointerPosition.Position.x,
            y: info.PointerPosition.Position.y,
            visible: info.PointerPosition.Visible.as_bool(),
            shape,
        })
    }
}

/// Formats we can consume, in order of preference.
const SUPPORTED_FORMATS: [DXGI_FORMAT; 3] = [
    DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT,
];

struct ReleaseGuard(IDXGIOutputDuplication);

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        // SAFETY: paired with a successful AcquireNextFrame. Errors here (e.g. ACCESS_LOST)
        // surface on the next acquire, so they are safe to ignore.
        let _ = unsafe { self.0.ReleaseFrame() };
    }
}

fn rect_from(rc: &RECT) -> Rect {
    Rect {
        x: rc.left,
        y: rc.top,
        width: (rc.right - rc.left).max(0).cast_unsigned(),
        height: (rc.bottom - rc.top).max(0).cast_unsigned(),
    }
}

fn map_dup_error(err: windows::core::Error) -> CaptureError {
    let code = err.code();
    if code == E_ACCESSDENIED {
        CaptureError::AccessDenied
    } else if code == DXGI_ERROR_UNSUPPORTED {
        CaptureError::Unsupported("desktop duplication unsupported on this output".into())
    } else if code == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE {
        CaptureError::Unsupported("too many desktop duplication sessions".into())
    } else {
        err.into()
    }
}

fn map_acquire_error(err: windows::core::Error) -> CaptureError {
    let code = err.code();
    if code == DXGI_ERROR_WAIT_TIMEOUT {
        CaptureError::Timeout
    } else if code == DXGI_ERROR_ACCESS_LOST {
        CaptureError::AccessLost
    } else if code == E_ACCESSDENIED {
        CaptureError::AccessDenied
    } else {
        err.into()
    }
}
