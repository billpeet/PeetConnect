//! GPU colour conversion and scaling with the D3D11 video processor.
//!
//! One [`GpuConverter`] handles one (input size, output size, output format) combination:
//! BGRA to NV12 for encoders, NV12 to BGRA for display or readback, and BGRA to BGRA for
//! scaling. It can write into its own ring of output textures or into any caller-provided
//! texture such as a swap chain back buffer.

use std::mem::ManuallyDrop;

use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoContext1,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_TYPE,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

use crate::CodecError;

/// Number of owned output textures rotated through by [`GpuConverter::convert`], so a
/// consumer can still be reading one while the next is written.
const RING: usize = 4;

/// Video processor based converter/scaler. Colour is BT.709; YCbCr is limited range.
pub struct GpuConverter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    in_size: (u32, u32),
    out_size: (u32, u32),
    out_format: DXGI_FORMAT,
}

impl std::fmt::Debug for GpuConverter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuConverter")
            .field("in_size", &self.in_size)
            .field("out_size", &self.out_size)
            .field("out_format", &self.out_format)
            .finish_non_exhaustive()
    }
}

fn color_space_for(format: DXGI_FORMAT) -> DXGI_COLOR_SPACE_TYPE {
    if format == DXGI_FORMAT_NV12 {
        DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709
    } else {
        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
    }
}

impl GpuConverter {
    /// BGRA8 in, NV12 out, same size. The encoder input path.
    pub fn bgra_to_nv12(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> Result<Self, CodecError> {
        Self::new(
            device,
            context,
            (width, height),
            DXGI_FORMAT_B8G8R8A8_UNORM,
            (width, height),
            DXGI_FORMAT_NV12,
        )
    }

    /// NV12 in, BGRA8 out, optionally scaled. The display/readback path.
    pub fn nv12_to_bgra(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        in_size: (u32, u32),
        out_size: (u32, u32),
    ) -> Result<Self, CodecError> {
        Self::new(
            device,
            context,
            in_size,
            DXGI_FORMAT_NV12,
            out_size,
            DXGI_FORMAT_B8G8R8A8_UNORM,
        )
    }

    /// BGRA8 in and out at different sizes. A pure scaler.
    pub fn bgra_scaler(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        in_size: (u32, u32),
        out_size: (u32, u32),
    ) -> Result<Self, CodecError> {
        Self::new(
            device,
            context,
            in_size,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            out_size,
            DXGI_FORMAT_B8G8R8A8_UNORM,
        )
    }

    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        in_size: (u32, u32),
        in_format: DXGI_FORMAT,
        out_size: (u32, u32),
        out_format: DXGI_FORMAT,
    ) -> Result<Self, CodecError> {
        for (w, h) in [in_size, out_size] {
            if w == 0 || h == 0 {
                return Err(CodecError::InvalidInput("zero-sized conversion".into()));
            }
        }
        if out_format == DXGI_FORMAT_NV12
            && (!out_size.0.is_multiple_of(2) || !out_size.1.is_multiple_of(2))
        {
            return Err(CodecError::InvalidInput(format!(
                "NV12 needs even dimensions, got {}x{}",
                out_size.0, out_size.1
            )));
        }
        let video_device: ID3D11VideoDevice = device.cast()?;
        let video_context: ID3D11VideoContext = context.cast()?;
        let rate = DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: in_size.0,
            InputHeight: in_size.1,
            OutputFrameRate: rate,
            OutputWidth: out_size.0,
            OutputHeight: out_size.1,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: plain COM calls with valid descriptors.
        let (enumerator, processor) = unsafe {
            let enumerator = video_device.CreateVideoProcessorEnumerator(&desc)?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0)?;
            if let Ok(ctx1) = video_context.cast::<ID3D11VideoContext1>() {
                ctx1.VideoProcessorSetStreamColorSpace1(&processor, 0, color_space_for(in_format));
                ctx1.VideoProcessorSetOutputColorSpace1(&processor, color_space_for(out_format));
            }
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 {
                    RGBA: D3D11_VIDEO_COLOR_RGBA {
                        R: 0.0,
                        G: 0.0,
                        B: 0.0,
                        A: 1.0,
                    },
                },
            };
            video_context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
            (enumerator, processor)
        };

        let mut converter = Self {
            video_device,
            video_context,
            enumerator,
            processor,
            outputs: Vec::new(),
            next: 0,
            in_size,
            out_size,
            out_format,
        };
        for _ in 0..RING {
            let texture = converter.create_output(device)?;
            let view = converter.output_view(&texture)?;
            converter.outputs.push((texture, view));
        }
        Ok(converter)
    }

    pub fn in_size(&self) -> (u32, u32) {
        self.in_size
    }

    pub fn out_size(&self) -> (u32, u32) {
        self.out_size
    }

    /// Restricts where the input lands in the output, e.g. for letterboxing. `None` fills
    /// the whole output.
    pub fn set_dest_rect(&self, rect: Option<RECT>) {
        // SAFETY: the processor is live; a null rect disables the override.
        unsafe {
            self.video_context.VideoProcessorSetStreamDestRect(
                &self.processor,
                0,
                rect.is_some(),
                rect.as_ref().map(std::ptr::from_ref),
            );
        }
    }

    /// Converts `source` into the next owned output texture and returns it. The texture stays
    /// valid until `RING - 1` further calls.
    pub fn convert(
        &mut self,
        source: &ID3D11Texture2D,
        source_subresource: u32,
    ) -> Result<&ID3D11Texture2D, CodecError> {
        let index = self.next;
        self.next = (self.next + 1) % RING;
        let (texture, view) = &self.outputs[index];
        let view = view.clone();
        self.blit_to_view(source, source_subresource, &view)?;
        Ok(texture)
    }

    /// Converts `source` into `destination`, which must have the output format and size and
    /// be bindable as a render target (swap chain back buffers qualify).
    pub fn blit(
        &self,
        source: &ID3D11Texture2D,
        source_subresource: u32,
        destination: &ID3D11Texture2D,
    ) -> Result<(), CodecError> {
        let view = self.output_view(destination)?;
        self.blit_to_view(source, source_subresource, &view)
    }

    fn blit_to_view(
        &self,
        source: &ID3D11Texture2D,
        source_subresource: u32,
        out_view: &ID3D11VideoProcessorOutputView,
    ) -> Result<(), CodecError> {
        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: source_subresource,
                },
            },
        };
        let mut in_view: Option<ID3D11VideoProcessorInputView> = None;
        // SAFETY: descriptors are valid; the stream's view is released manually afterwards.
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                source,
                &self.enumerator,
                &in_desc,
                Some(&mut in_view),
            )?;
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: ManuallyDrop::new(in_view),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            };
            let result = self.video_context.VideoProcessorBlt(
                &self.processor,
                out_view,
                0,
                std::slice::from_ref(&stream),
            );
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result?;
        }
        Ok(())
    }

    fn create_output(&self, device: &ID3D11Device) -> Result<ID3D11Texture2D, CodecError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.out_size.0,
            Height: self.out_size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: self.out_format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0.cast_unsigned(),
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: desc is valid; the out-pointer lives for the call.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }?;
        texture.ok_or_else(|| CodecError::Encoder("null converter output texture".into()))
    }

    fn output_view(
        &self,
        texture: &ID3D11Texture2D,
    ) -> Result<ID3D11VideoProcessorOutputView, CodecError> {
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut view = None;
        // SAFETY: as above.
        unsafe {
            self.video_device.CreateVideoProcessorOutputView(
                texture,
                &self.enumerator,
                &desc,
                Some(&mut view),
            )?;
        }
        view.ok_or_else(|| CodecError::Encoder("null output view".into()))
    }
}
