//! Media Foundation H.264 encoder.
//!
//! Hardware MFTs (NVENC, AMF, Quick Sync) are asynchronous: they tell us when they want input
//! and when output is ready via `IMFMediaEventGenerator`. Microsoft's software MFT is
//! synchronous. This type handles both behind one API.

use std::mem::ManuallyDrop;
use std::sync::OnceLock;

use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::{VARIANT_BOOL, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_High, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode,
    ICodecAPI, IMFActivate, IMFDXGIDeviceManager, IMFMediaEvent, IMFMediaEventGenerator,
    IMFMediaType, IMFSample, IMFTransform, METransformHaveOutput, METransformNeedInput,
    MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFSampleExtension_CleanPoint, MFStartup, MFTEnumEx,
    MFT_FRIENDLY_NAME_Attribute, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NONE,
    MF_EVENT_FLAG_NO_WAIT, MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE,
    MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

use super::convert::GpuConverter;
use crate::nv12::bgra_to_nv12;
use crate::{contains_idr, CodecError, EncodedPacket, EncoderConfig, VideoEncoder};

/// An encoder MFT discovered on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    pub name: String,
    pub hardware: bool,
}

/// Lists H.264 encoder MFTs, hardware first.
pub fn list_encoders() -> Result<Vec<EncoderInfo>, CodecError> {
    init_mf()?;
    let mut out = Vec::new();
    for hardware in [true, false] {
        for activate in enum_encoders(hardware)? {
            out.push(EncoderInfo {
                name: friendly_name(&activate),
                hardware,
            });
        }
    }
    Ok(out)
}

struct D3dPath {
    converter: GpuConverter,
    _manager: IMFDXGIDeviceManager,
}

/// Media Foundation H.264 encoder, hardware or software.
pub struct MfH264Encoder {
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec_api: Option<ICodecAPI>,
    input_id: u32,
    output_id: u32,
    name: String,
    hardware: bool,
    config: EncoderConfig,
    frame_duration: i64,
    need_input_credits: u32,
    provides_samples: bool,
    output_size: u32,
    force_keyframe: bool,
    nv12_scratch: Vec<u8>,
    d3d: Option<D3dPath>,
}

impl std::fmt::Debug for MfH264Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfH264Encoder")
            .field("name", &self.name)
            .field("hardware", &self.hardware)
            .field("async", &self.events.is_some())
            .field("d3d", &self.d3d.is_some())
            .finish_non_exhaustive()
    }
}

impl MfH264Encoder {
    /// Creates an encoder. With `d3d` set, the encoder is bound to that device and accepts
    /// textures via [`encode_texture`](Self::encode_texture) without a CPU round trip.
    pub fn new(
        config: &EncoderConfig,
        hardware: bool,
        d3d: Option<(&ID3D11Device, &ID3D11DeviceContext)>,
    ) -> Result<Self, CodecError> {
        init_mf()?;
        let candidates = enum_encoders(hardware)?;
        let mut last_err = CodecError::Unavailable(format!(
            "no {} H.264 encoder MFT",
            if hardware { "hardware" } else { "software" }
        ));
        // Several hardware MFTs may be present (one per GPU vendor). Only the one matching
        // the D3D device's adapter will accept it, so try each in turn.
        for activate in candidates {
            let name = friendly_name(&activate);
            match Self::build(&activate, &name, config, hardware, d3d) {
                Ok(enc) => return Ok(enc),
                Err(err) => {
                    tracing::debug!(%name, %err, "encoder MFT rejected");
                    last_err = err;
                }
            }
        }
        Err(last_err)
    }

    fn build(
        activate: &IMFActivate,
        name: &str,
        config: &EncoderConfig,
        hardware: bool,
        d3d: Option<(&ID3D11Device, &ID3D11DeviceContext)>,
    ) -> Result<Self, CodecError> {
        let name = name.to_owned();
        // SAFETY: activate came from MFTEnumEx.
        let transform: IMFTransform = unsafe { activate.ActivateObject() }?;

        // SAFETY: plain COM calls on a live transform.
        let (events, d3d_aware) = unsafe {
            let attrs = transform.GetAttributes()?;
            let is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 1;
            if is_async {
                attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            let d3d_aware = attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 1;
            let events = if is_async {
                Some(transform.cast::<IMFMediaEventGenerator>()?)
            } else {
                None
            };
            (events, d3d_aware)
        };

        let d3d_path = match d3d {
            Some((device, context)) if d3d_aware => {
                let mut token = 0u32;
                let mut manager: Option<IMFDXGIDeviceManager> = None;
                // SAFETY: out-pointers valid; manager is then handed to the transform.
                unsafe {
                    MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
                    let manager = manager
                        .ok_or_else(|| CodecError::Encoder("null DXGI device manager".into()))?;
                    manager.ResetDevice(device, token)?;
                    transform
                        .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
                    Some(D3dPath {
                        converter: GpuConverter::bgra_to_nv12(
                            device,
                            context,
                            config.width,
                            config.height,
                        )?,
                        _manager: manager,
                    })
                }
            }
            Some(_) => {
                tracing::debug!(%name, "encoder is not D3D11 aware; falling back to CPU input");
                None
            }
            None => None,
        };

        let (input_id, output_id) = stream_ids(&transform);

        // SAFETY: codec API is optional; failures to set individual properties are logged.
        let codec_api = transform.cast::<ICodecAPI>().ok();
        if let Some(api) = &codec_api {
            set_codec_prop(api, &CODECAPI_AVLowLatencyMode, &variant_bool(true));
            set_codec_prop(
                api,
                &CODECAPI_AVEncCommonRateControlMode,
                &variant_u32(eAVEncCommonRateControlMode_CBR.0.cast_unsigned()),
            );
            set_codec_prop(
                api,
                &CODECAPI_AVEncCommonMeanBitRate,
                &variant_u32(config.bitrate_bps()),
            );
            set_codec_prop(api, &CODECAPI_AVEncMPVDefaultBPictureCount, &variant_u32(0));
            if config.keyframe_interval > 0 {
                set_codec_prop(
                    api,
                    &CODECAPI_AVEncMPVGOPSize,
                    &variant_u32(config.keyframe_interval),
                );
            }
        }

        // SAFETY: media types are freshly created and fully owned here.
        unsafe {
            let out_type = MFCreateMediaType()?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(config.width, config.height))?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(config.fps.max(1), 1))?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_bps())?;
            out_type.SetUINT32(
                &MF_MT_INTERLACE_MODE,
                MFVideoInterlace_Progressive.0.cast_unsigned(),
            )?;
            out_type.SetUINT32(
                &MF_MT_MPEG2_PROFILE,
                eAVEncH264VProfile_High.0.cast_unsigned(),
            )?;
            transform.SetOutputType(output_id, &out_type, 0)?;

            let in_type = find_input_type(&transform, input_id, &MFVideoFormat_NV12)?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(config.width, config.height))?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(config.fps.max(1), 1))?;
            in_type.SetUINT32(
                &MF_MT_INTERLACE_MODE,
                MFVideoInterlace_Progressive.0.cast_unsigned(),
            )?;
            transform.SetInputType(input_id, &in_type, 0)?;
        }

        // SAFETY: stream info is a plain struct.
        let info = unsafe { transform.GetOutputStreamInfo(output_id) }?;
        let provides_samples =
            info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0.cast_unsigned() != 0;

        // SAFETY: streaming notifications on a configured transform.
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        tracing::info!(%name, hardware, is_async = events.is_some(), d3d = d3d_path.is_some(), "encoder ready");
        Ok(Self {
            transform,
            events,
            codec_api,
            input_id,
            output_id,
            name,
            hardware,
            config: *config,
            frame_duration: 10_000_000 / i64::from(config.fps.max(1)),
            need_input_credits: 0,
            provides_samples,
            output_size: info.cbSize,
            force_keyframe: false,
            nv12_scratch: Vec::new(),
            d3d: d3d_path,
        })
    }

    /// Changes the target bitrate mid-stream. Hardware encoders apply it from the next
    /// frame; the software MFT may take a GOP to settle.
    pub fn set_bitrate(&mut self, bitrate_kbps: u32) {
        if let Some(api) = &self.codec_api {
            set_codec_prop(
                api,
                &CODECAPI_AVEncCommonMeanBitRate,
                &variant_u32(bitrate_kbps.saturating_mul(1000)),
            );
        }
    }

    /// True when [`encode_texture`](Self::encode_texture) is available.
    pub fn accepts_textures(&self) -> bool {
        self.d3d.is_some()
    }

    /// Encodes a BGRA8 texture on the bound device. Requires construction with a device.
    pub fn encode_texture(
        &mut self,
        texture: &ID3D11Texture2D,
        pts_us: u64,
    ) -> Result<Vec<EncodedPacket>, CodecError> {
        let path = self
            .d3d
            .as_mut()
            .ok_or_else(|| CodecError::InvalidInput("encoder has no D3D device".into()))?;
        let nv12 = path.converter.convert(texture, 0)?;
        // SAFETY: nv12 is a live texture on the device the encoder is bound to.
        let sample = unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample
        };
        self.submit(&sample, pts_us)
    }

    fn submit(
        &mut self,
        sample: &IMFSample,
        pts_us: u64,
    ) -> Result<Vec<EncodedPacket>, CodecError> {
        let mut packets = Vec::new();
        // SAFETY: sample is fully owned; all calls are on live interfaces.
        unsafe {
            sample.SetSampleTime(i64::try_from(pts_us).unwrap_or(0) * 10)?;
            sample.SetSampleDuration(self.frame_duration)?;

            if let Some(events) = self.events.clone() {
                while self.need_input_credits == 0 {
                    let event = events.GetEvent(MF_EVENT_FLAG_NONE)?;
                    self.handle_event(&event, &mut packets)?;
                }
                self.need_input_credits -= 1;
            }

            if self.force_keyframe {
                if let Some(api) = &self.codec_api {
                    set_codec_prop(api, &CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
                }
                self.force_keyframe = false;
            }
            self.transform.ProcessInput(self.input_id, sample, 0)?;

            if let Some(events) = self.events.clone() {
                // Collect anything already queued, then block once so that a 1-in-1-out
                // encoder returns this frame's output in this call.
                self.drain_events(&events, &mut packets)?;
                if packets.is_empty() {
                    let event = events.GetEvent(MF_EVENT_FLAG_NONE)?;
                    self.handle_event(&event, &mut packets)?;
                    self.drain_events(&events, &mut packets)?;
                }
            } else {
                while self.process_output(&mut packets)? {}
            }
        }
        Ok(packets)
    }

    unsafe fn drain_events(
        &mut self,
        events: &IMFMediaEventGenerator,
        packets: &mut Vec<EncodedPacket>,
    ) -> Result<(), CodecError> {
        loop {
            // SAFETY: caller guarantees the generator is live.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => self.handle_event(&event, packets)?,
                Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(err) => return Err(err.into()),
            }
        }
    }

    fn handle_event(
        &mut self,
        event: &IMFMediaEvent,
        packets: &mut Vec<EncodedPacket>,
    ) -> Result<(), CodecError> {
        // SAFETY: event is live.
        let kind = unsafe { event.GetType() }?;
        if kind == METransformNeedInput.0.cast_unsigned() {
            self.need_input_credits += 1;
        } else if kind == METransformHaveOutput.0.cast_unsigned() {
            self.process_output(packets)?;
        }
        Ok(())
    }

    /// Pulls one output sample. Returns `Ok(false)` when the encoder needs more input.
    fn process_output(&mut self, packets: &mut Vec<EncodedPacket>) -> Result<bool, CodecError> {
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
            // SAFETY: `out` is valid for the call; ownership of its fields is taken below.
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut out), &mut status)
            };
            // SAFETY: we own both ManuallyDrop fields and take them exactly once.
            let sample = unsafe {
                ManuallyDrop::drop(&mut out.pEvents);
                ManuallyDrop::take(&mut out.pSample)
            };
            match result {
                Ok(()) => {
                    if let Some(sample) = sample {
                        packets.push(packet_from_sample(&sample)?);
                    }
                    return Ok(true);
                }
                Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // SAFETY: renegotiate with the type the transform now prefers.
                    unsafe {
                        let new_type = self.transform.GetOutputAvailableType(self.output_id, 0)?;
                        self.transform.SetOutputType(self.output_id, &new_type, 0)?;
                    }
                }
                Err(err) => return Err(err.into()),
            }
        }
    }
}

// SAFETY: Media Foundation transforms and D3D11 objects are free-threaded COM objects
// (the D3D11 context is multithread-protected by `D3dDevice`), and this type is only ever
// driven from one thread at a time.
unsafe impl Send for MfH264Encoder {}

impl VideoEncoder for MfH264Encoder {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_hardware(&self) -> bool {
        self.hardware
    }

    fn encode_bgra(
        &mut self,
        bgra: &[u8],
        stride: usize,
        pts_us: u64,
    ) -> Result<Vec<EncodedPacket>, CodecError> {
        let (w, h) = (self.config.width, self.config.height);
        if stride < w as usize * 4 || bgra.len() < stride * h as usize {
            return Err(CodecError::InvalidInput(
                "frame smaller than configured".into(),
            ));
        }
        let mut scratch = std::mem::take(&mut self.nv12_scratch);
        bgra_to_nv12(bgra, stride, w, h, &mut scratch);
        // SAFETY: buffer is created with exactly scratch.len() capacity and locked/unlocked
        // around the copy.
        let sample = unsafe {
            let len = u32::try_from(scratch.len())
                .map_err(|_| CodecError::InvalidInput("frame too large".into()))?;
            let buffer = MFCreateMemoryBuffer(len)?;
            let mut ptr = std::ptr::null_mut::<u8>();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(scratch.as_ptr(), ptr, scratch.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(len)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample
        };
        self.nv12_scratch = scratch;
        self.submit(&sample, pts_us)
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }
}

impl Drop for MfH264Encoder {
    fn drop(&mut self) {
        // SAFETY: best-effort teardown; errors are irrelevant at this point.
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

pub(crate) fn init_mf() -> Result<(), CodecError> {
    static INIT: OnceLock<Result<(), (i32, String)>> = OnceLock::new();
    INIT.get_or_init(|| {
        // SAFETY: process-wide initialisation; a changed-mode error means COM is already up.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| (e.code().0, e.message()))
        }
    })
    .clone()
    .map_err(|(code, message)| CodecError::Os { code, message })
}

fn enum_encoders(hardware: bool) -> Result<Vec<IMFActivate>, CodecError> {
    let flags: MFT_ENUM_FLAG = if hardware {
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER
    } else {
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx allocates `count` activates with CoTaskMemAlloc; we take each one
    // then free the array.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&output),
            &mut activates,
            &mut count,
        )?;
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            if let Some(activate) = (*activates.add(i)).take() {
                out.push(activate);
            }
        }
        if !activates.is_null() {
            CoTaskMemFree(Some(activates.cast()));
        }
        Ok(out)
    }
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut pwstr = PWSTR::null();
    let mut len = 0u32;
    // SAFETY: on success pwstr is CoTaskMem allocated and freed here.
    unsafe {
        if activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut pwstr, &mut len)
            .is_ok()
            && !pwstr.is_null()
        {
            let name = pwstr.to_string().unwrap_or_default();
            CoTaskMemFree(Some(pwstr.0.cast()));
            return name;
        }
    }
    "unknown encoder".to_owned()
}

fn stream_ids(transform: &IMFTransform) -> (u32, u32) {
    let mut inputs = [0u32];
    let mut outputs = [0u32];
    // SAFETY: E_NOTIMPL means IDs are 0-based indices, which the defaults already are.
    let _ = unsafe { transform.GetStreamIDs(&mut inputs, &mut outputs) };
    (inputs[0], outputs[0])
}

unsafe fn find_input_type(
    transform: &IMFTransform,
    input_id: u32,
    subtype: &GUID,
) -> Result<IMFMediaType, CodecError> {
    for index in 0u32.. {
        // SAFETY: caller guarantees a live transform; enumeration ends with an error.
        let Ok(candidate) = (unsafe { transform.GetInputAvailableType(input_id, index) }) else {
            break;
        };
        // SAFETY: candidate is live.
        if unsafe { candidate.GetGUID(&MF_MT_SUBTYPE) }.ok().as_ref() == Some(subtype) {
            return Ok(candidate);
        }
    }
    Err(CodecError::Unavailable(
        "encoder does not accept NV12 input".into(),
    ))
}

fn packet_from_sample(sample: &IMFSample) -> Result<EncodedPacket, CodecError> {
    // SAFETY: Lock/Unlock are paired and the slice is only read while locked.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr = std::ptr::null_mut::<u8>();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        buffer.Unlock()?;
        let clean_point = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;
        let pts_us = sample.GetSampleTime().map_or(0, |t| t / 10);
        Ok(EncodedPacket {
            keyframe: clean_point || contains_idr(&data),
            data,
            pts_us: u64::try_from(pts_us).unwrap_or(0),
        })
    }
}

fn set_codec_prop(api: &ICodecAPI, key: &GUID, value: &VARIANT) {
    // SAFETY: key and value outlive the call.
    if let Err(err) = unsafe { api.SetValue(key, value) } {
        tracing::debug!(?key, %err, "codec property not applied");
    }
}

fn variant_u32(value: u32) -> VARIANT {
    let mut var = VARIANT::default();
    // SAFETY: writing the discriminant and matching union member of a zeroed VARIANT.
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = value;
    }
    var
}

fn variant_bool(value: bool) -> VARIANT {
    let mut var = VARIANT::default();
    // SAFETY: as above.
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = if value {
            VARIANT_TRUE
        } else {
            VARIANT_BOOL::default()
        };
    }
    var
}

fn pack_u64(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}
