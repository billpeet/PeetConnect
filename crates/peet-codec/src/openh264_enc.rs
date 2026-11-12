//! Software H.264 via Cisco openh264 (BSD-2). Pure CPU; works on every platform.

use openh264::encoder::{
    BitRate, Encoder, EncoderConfig as OhConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, SpsPpsStrategy, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use openh264::{OpenH264API, Timestamp};

use crate::{CodecError, EncodedPacket, EncoderConfig, VideoEncoder};

pub struct OpenH264Encoder {
    encoder: Encoder,
    yuv: YUVBuffer,
    packed: Vec<u8>,
    width: u32,
    height: u32,
    force_keyframe: bool,
}

impl std::fmt::Debug for OpenH264Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenH264Encoder")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl OpenH264Encoder {
    pub fn new(config: &EncoderConfig) -> Result<Self, CodecError> {
        if config.width == 0 || config.height == 0 {
            return Err(CodecError::InvalidInput("zero-sized frame".into()));
        }
        let mut oh = OhConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(config.bitrate_bps()))
            .max_frame_rate(FrameRate::from_hz(config.fps.max(1) as f32))
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            // Screen-content mode does not support these; disabling them avoids warnings.
            .adaptive_quantization(false)
            .background_detection(false)
            // Bitrate control requires frame skipping; a skipped frame yields no packet.
            .skip_frames(true)
            // 0 lets openh264 pick a thread count for the core count.
            .num_threads(0);
        if config.keyframe_interval > 0 {
            oh = oh.intra_frame_period(IntraFramePeriod::from_num_frames(config.keyframe_interval));
        }
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), oh)?;
        Ok(Self {
            encoder,
            yuv: YUVBuffer::new(config.width as usize, config.height as usize),
            packed: Vec::new(),
            width: config.width,
            height: config.height,
            force_keyframe: false,
        })
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn name(&self) -> &'static str {
        "openh264 (software)"
    }

    fn is_hardware(&self) -> bool {
        false
    }

    fn encode_bgra(
        &mut self,
        bgra: &[u8],
        stride: usize,
        pts_us: u64,
    ) -> Result<Vec<EncodedPacket>, CodecError> {
        let w = self.width as usize;
        let h = self.height as usize;
        if stride < w * 4 || bgra.len() < stride * h {
            return Err(CodecError::InvalidInput(
                "frame smaller than configured".into(),
            ));
        }
        let packed: &[u8] = if stride == w * 4 {
            &bgra[..w * 4 * h]
        } else {
            self.packed.clear();
            for row in bgra.chunks_exact(stride).take(h) {
                self.packed.extend_from_slice(&row[..w * 4]);
            }
            &self.packed
        };
        self.yuv.read_bgra8(BgraSliceU8::new(packed, (w, h)));

        if self.force_keyframe {
            self.encoder.force_intra_frame();
            self.force_keyframe = false;
        }
        let bitstream = self
            .encoder
            .encode_at(&self.yuv, Timestamp::from_millis(pts_us / 1000))?;
        let frame_type = bitstream.frame_type();
        if matches!(frame_type, FrameType::Skip | FrameType::Invalid) {
            return Ok(Vec::new());
        }
        Ok(vec![EncodedPacket {
            data: bitstream.to_vec(),
            keyframe: matches!(frame_type, FrameType::IDR | FrameType::I),
            pts_us,
        }])
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }
}
