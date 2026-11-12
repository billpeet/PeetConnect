//! Media Foundation H.264 decoder with DXVA output to D3D11 textures.
//!
//! Microsoft's H.264 decoder MFT is synchronous. When bound to a D3D11 device it decodes on
//! the GPU and hands back NV12 textures (usually slices of a texture array) that can be
//! colour-converted or presented without touching system memory.

use std::mem::ManuallyDrop;

use windows::core::Interface;
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate, IMFDXGIBuffer, IMFDXGIDeviceManager,
    IMFSample, IMFTransform, MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{VARIANT, VT_BOOL};

use super::mf::init_mf;
use crate::CodecError;

/// A decoded picture living in a D3D11 texture owned by the decoder's pool.
///
/// The texture may be one slice of an array; always pass `subresource` along. The frame
/// keeps the underlying sample alive, so hold it only as long as needed or the decoder
/// stalls for lack of output buffers.
pub struct DecodedFrame {
    pub texture: ID3D11Texture2D,
    pub subresource: u32,
    pub width: u32,
    pub height: u32,
    pub pts_us: u64,
    _sample: IMFSample,
}

impl std::fmt::Debug for DecodedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedFrame")
            .field("subresource", &self.subresource)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("pts_us", &self.pts_us)
            .finish_non_exhaustive()
    }
}

/// Media Foundation H.264 decoder bound to a D3D11 device.
pub struct MfH264Decoder {
    transform: IMFTransform,
    _manager: IMFDXGIDeviceManager,
    input_id: u32,
    output_id: u32,
    width: u32,
    height: u32,
    provides_samples: bool,
    output_size: u32,
}

// SAFETY: MF transforms are free-threaded COM objects, used from one thread at a time.
unsafe impl Send for MfH264Decoder {}

impl std::fmt::Debug for MfH264Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfH264Decoder")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl MfH264Decoder {
    /// Creates a decoder for streams of the given nominal size. The real size is taken from
    /// the stream and reported on each [`DecodedFrame`].
    pub fn new(width: u32, height: u32, device: &ID3D11Device) -> Result<Self, CodecError> {
        init_mf()?;
        let activate = first_decoder()?;
        // SAFETY: activate came from MFTEnumEx.
        let transform: IMFTransform = unsafe { activate.ActivateObject() }?;

        // SAFETY: plain COM calls on live objects; the manager is retained in `self`.
        let manager = unsafe {
            let attrs = transform.GetAttributes()?;
            if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 1 {
                return Err(CodecError::Unavailable(
                    "H.264 decoder is not D3D11 aware".into(),
                ));
            }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            let mut token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager =
                manager.ok_or_else(|| CodecError::Encoder("null DXGI device manager".into()))?;
            manager.ResetDevice(device, token)?;
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
            manager
        };

        if let Ok(api) = transform.cast::<ICodecAPI>() {
            let mut var = VARIANT::default();
            // SAFETY: writing the discriminant and matching member of a zeroed VARIANT.
            unsafe {
                let inner = &mut *var.Anonymous.Anonymous;
                inner.vt = VT_BOOL;
                inner.Anonymous.boolVal = VARIANT_TRUE;
                if let Err(err) = api.SetValue(&CODECAPI_AVLowLatencyMode, &var) {
                    tracing::debug!(%err, "decoder low-latency mode not applied");
                }
            }
        }

        let (input_id, output_id) = {
            let mut inputs = [0u32];
            let mut outputs = [0u32];
            // SAFETY: E_NOTIMPL means 0-based IDs, which the defaults already are.
            let _ = unsafe { transform.GetStreamIDs(&mut inputs, &mut outputs) };
            (inputs[0], outputs[0])
        };

        // SAFETY: freshly created media types, fully owned.
        unsafe {
            let in_type = MFCreateMediaType()?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            in_type.SetUINT64(
                &MF_MT_FRAME_SIZE,
                (u64::from(width) << 32) | u64::from(height),
            )?;
            in_type.SetUINT32(
                &MF_MT_INTERLACE_MODE,
                MFVideoInterlace_Progressive.0.cast_unsigned(),
            )?;
            transform.SetInputType(input_id, &in_type, 0)?;
        }
        let mut decoder = Self {
            transform,
            _manager: manager,
            input_id,
            output_id,
            width,
            height,
            provides_samples: false,
            output_size: 0,
        };
        decoder.negotiate_output()?;
        // SAFETY: streaming notifications on a configured transform.
        unsafe {
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        tracing::info!(width, height, "H.264 decoder ready (D3D11)");
        Ok(decoder)
    }

    /// Current coded size, updated on stream changes.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn negotiate_output(&mut self) -> Result<(), CodecError> {
        // SAFETY: enumeration stops at the first error; types are live.
        unsafe {
            let mut chosen = None;
            for index in 0u32.. {
                let Ok(candidate) = self.transform.GetOutputAvailableType(self.output_id, index)
                else {
                    break;
                };
                if candidate.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    chosen = Some(candidate);
                    break;
                }
            }
            let out_type = chosen
                .ok_or_else(|| CodecError::Unavailable("decoder offers no NV12 output".into()))?;
            self.transform.SetOutputType(self.output_id, &out_type, 0)?;
            if let Ok(size) = out_type.GetUINT64(&MF_MT_FRAME_SIZE) {
                self.width = (size >> 32) as u32;
                self.height = (size & 0xffff_ffff) as u32;
            }
            let info = self.transform.GetOutputStreamInfo(self.output_id)?;
            self.provides_samples =
                info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0.cast_unsigned() != 0;
            self.output_size = info.cbSize;
        }
        Ok(())
    }

    /// Feeds one access unit (Annex B) and returns any pictures that became available.
    pub fn decode(&mut self, annex_b: &[u8], pts_us: u64) -> Result<Vec<DecodedFrame>, CodecError> {
        let mut frames = Vec::new();
        // SAFETY: buffer is sized to the input and locked/unlocked around the copy.
        let sample = unsafe {
            let len = u32::try_from(annex_b.len())
                .map_err(|_| CodecError::InvalidInput("access unit too large".into()))?;
            let buffer = MFCreateMemoryBuffer(len.max(1))?;
            let mut ptr = std::ptr::null_mut::<u8>();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(annex_b.as_ptr(), ptr, annex_b.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(len)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(i64::try_from(pts_us).unwrap_or(0) * 10)?;
            sample
        };

        // A synchronous decoder refuses input while it holds output; drain first.
        while self.process_output(&mut frames)? {}
        // SAFETY: sample is fully owned.
        match unsafe { self.transform.ProcessInput(self.input_id, &sample, 0) } {
            Ok(()) => {}
            Err(err) if err.code() == MF_E_NOTACCEPTING => {
                while self.process_output(&mut frames)? {}
                // SAFETY: as above.
                unsafe { self.transform.ProcessInput(self.input_id, &sample, 0) }?;
            }
            Err(err) => return Err(err.into()),
        }
        while self.process_output(&mut frames)? {}
        Ok(frames)
    }

    fn process_output(&mut self, frames: &mut Vec<DecodedFrame>) -> Result<bool, CodecError> {
        loop {
            let provided = if self.provides_samples {
                None
            } else {
                // SAFETY: allocating a sample with a buffer of the advertised size.
                Some(unsafe {
                    let sample = MFCreateSample()?;
                    let buffer = MFCreateMemoryBuffer(self.output_size.max(1))?;
                    sample.AddBuffer(&buffer)?;
                    sample
                })
            };
            let mut out = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_id,
                pSample: ManuallyDrop::new(provided),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            // SAFETY: `out` is valid for the call; both fields are taken exactly once below.
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut out), &mut status)
            };
            // SAFETY: see above.
            let sample = unsafe {
                ManuallyDrop::drop(&mut out.pEvents);
                ManuallyDrop::take(&mut out.pSample)
            };
            match result {
                Ok(()) => {
                    if let Some(sample) = sample {
                        frames.push(self.frame_from_sample(sample)?);
                    }
                    return Ok(true);
                }
                Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.negotiate_output()?;
                    tracing::debug!(
                        width = self.width,
                        height = self.height,
                        "decoder stream change"
                    );
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    fn frame_from_sample(&self, sample: IMFSample) -> Result<DecodedFrame, CodecError> {
        // SAFETY: the sample's first buffer is a DXGI buffer when a D3D manager is set.
        unsafe {
            let buffer = sample.GetBufferByIndex(0)?;
            let dxgi: IMFDXGIBuffer = buffer.cast().map_err(|_| {
                CodecError::Unavailable("decoder produced a system-memory sample".into())
            })?;
            let mut raw = std::ptr::null_mut::<core::ffi::c_void>();
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
            let texture = ID3D11Texture2D::from_raw(raw);
            let subresource = dxgi.GetSubresourceIndex()?;
            let pts_us = u64::try_from(sample.GetSampleTime().map_or(0, |t| t / 10)).unwrap_or(0);
            Ok(DecodedFrame {
                texture,
                subresource,
                width: self.width,
                height: self.height,
                pts_us,
                _sample: sample,
            })
        }
    }
}

impl Drop for MfH264Decoder {
    fn drop(&mut self) {
        // SAFETY: best-effort teardown.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

fn first_decoder() -> Result<IMFActivate, CodecError> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx allocates the array with CoTaskMemAlloc; we take the first and free it.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            None,
            &mut activates,
            &mut count,
        )?;
        let mut first = None;
        for i in 0..count as usize {
            let activate = (*activates.add(i)).take();
            if first.is_none() {
                first = activate;
            }
        }
        if !activates.is_null() {
            CoTaskMemFree(Some(activates.cast()));
        }
        first.ok_or_else(|| CodecError::Unavailable("no H.264 decoder MFT".into()))
    }
}
