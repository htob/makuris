//! macOS VideoToolbox 硬件 H.264 编码器。
//!
//! 为什么要它：同一台 M4 Pro 上，openh264 软编 3024x1964（屏幕 2x 物理像素）
//! 需要 54ms/帧，远超 30fps 的 33.3ms 预算；VideoToolbox 硬编只要 15ms。
//! 只有硬编才可能把推流分辨率提到物理像素——那是文字清晰的根子。
//!
//! 与 openh264 有两处格式差异必须自己抹平，否则解码端黑屏：
//! 1. VideoToolbox 输出 **AVCC** 格式（每个 NAL 前是 big-endian 长度前缀），
//!    而下游 RTP 打包链路（rtc::rtp::packetizer）吃的是 **Annex-B**（0x00000001 起始码）；
//! 2. 参数集（SPS/PPS）不随码流输出，而是藏在 CMSampleBuffer 的 format description 里，
//!    必须在每个关键帧前手动插入，否则对端无法初始化解码器。
//!
//! NAL 相关的转换与解析工具放在父模块（encode/mod.rs），与软编路径共用。

use anyhow::{anyhow, Result};
use apple_cf::cm::CMTime;
use apple_cf::iosurface::IOSurface;
use videotoolbox::prelude::*;

use super::{avcc_to_annexb, is_idr, parse_sps_profile_level, strip_start_code, START_CODE};

/// H.264 的 NAL 长度字段默认宽度（字节）。
const DEFAULT_NAL_LEN: usize = 4;
/// 视频时钟频率（RTP 与 VideoToolbox 都用 90kHz）。
const CLOCK_RATE: i32 = 90_000;

/// 目标 H.264 profile level：**Constrained Baseline**。
///
/// 这是两端支持列表的**唯一交集**，选它是实测逼出来的，不是保守：
///
/// | profile-level-id | 含义 | Chrome | iPhone |
/// |---|---|---|---|
/// | `640033` | 无约束 High | ✅ | ❌ 白屏 |
/// | `640c33` | Constrained High | ❌ 拒绝视频 | 未测 |
/// | `42e033` | Constrained Baseline | ✅ | ✅ |
///
/// iPhone 侧的证据：对 WebRTC.framework 二进制做 strings，它只声明 `42e01f`
/// （Constrained Baseline）与 `640c1f`（Constrained High），**不含无约束 High**；
/// 且此前 openh264 输出的 `42c020`（同属 Constrained Baseline 族）在 iPhone 上
/// 长期正常出画。Chrome 侧的实测：`640033` 能协商成功，换成 `640c33` 后它把
/// 视频 m-line 直接拒掉（answer 里 `m=video 0`、无 payload），说明 Chrome 也不
/// 接受 Constrained High。
///
/// 视频 m-line 被拒的后果很隐蔽：被拒的 m-line 仍带自己的 ice-ufrag，与数据通道
/// 的不同，于是触发 rtc 的 "multiple conflicting ice-ufrag values" 而终止会话，
/// 手机端表现为**白屏**（不是黑屏——黑屏是轨道建立但解不出帧，白屏是轨道都没建立）。
///
/// 代价是 Baseline 没有 CABAC，同码率下压缩率约低 10%。但清晰度的主要来源是
/// 2x 物理像素采样与 26Mbps 码率，熵编码这点差距远小于分辨率带来的收益。
const PROFILE_LEVEL: ProfileLevel = ProfileLevel::H264ConstrainedBaselineAutoLevel;

/// VideoToolbox 硬件 H.264 编码器：输入 BGRA `IOSurface`，输出 Annex-B 码流。
pub struct VtEncoder {
    session: CompressionSession,
    fps: u32,
    next_pts: i64,
    /// 参数集（Annex-B 形式，每个 NAL 前带起始码），首个样本后缓存复用。
    params: Option<Vec<u8>>,
    /// AVCC 长度字段宽度，由 format description 给出。
    nal_len: usize,
    /// 首帧 SPS 解析出的 (profile_idc, constraint_flags, level_idc)，用于核对 SDP 声明。
    sps_profile_level: Option<(u8, u8, u8)>,
}

impl VtEncoder {
    /// 创建硬编会话。`bitrate_bps` 为目标码率——硬编支持真正的码率控制，
    /// 不像 openh264 在关闭跳帧后只能退回固定 QP。
    pub fn new(width: usize, height: usize, fps: u32, bitrate_bps: i32) -> Result<Self> {
        let session = CompressionSession::builder(width as i32, height as i32, Codec::H264)
            // 实时模式：允许编码器为降低延迟而牺牲一点压缩率
            .with_real_time(true)
            .with_average_bit_rate(bitrate_bps)
            .with_expected_frame_rate(f64::from(fps))
            // 1 秒一个关键帧。原为 2 秒（fps * 2），改成 1 秒是为了缩短「丢包后
            // 画面卡住」的时长：解码端丢包后必须等下一个 IDR 才能恢复，间隔 2 秒
            // 意味着最坏要僵住 2 秒，体感上就是「点了没反应」。代价是码率上升
            // 约 10~15%，对交互流畅度的收益值这个价。
            .with_max_keyframe_interval(fps as i32)
            .with_low_latency_rate_control(true)
            // 必须显式指定 Constrained Baseline：默认的无约束 High 不被 iPhone 接受（见 PROFILE_LEVEL）
            .with_profile_level(PROFILE_LEVEL)
            // 明确要求硬件编码：不可用时报错，而不是静默退回软编
            .with_hardware_acceleration(HardwareAcceleration::Required)
            .build()
            .map_err(|e| anyhow!("VideoToolbox 编码会话创建失败: {e}"))?;

        Ok(Self {
            session,
            fps,
            next_pts: 0,
            params: None,
            nal_len: DEFAULT_NAL_LEN,
            sps_profile_level: None,
        })
    }

    /// 编码一个 BGRA `IOSurface`，返回 Annex-B 码流（关键帧前置 SPS/PPS）。
    /// 返回空 `Vec` 表示编码器本帧未产出码流。
    pub fn encode(&mut self, surface: &IOSurface) -> Result<Vec<u8>> {
        self.encode_inner(surface, false)
    }

    /// 强制关键帧编码。用于长时间无产出或需要新连接立刻出画的场景。
    pub fn encode_force_keyframe(&mut self, surface: &IOSurface) -> Result<Vec<u8>> {
        self.encode_inner(surface, true)
    }

    fn encode_inner(&mut self, surface: &IOSurface, force_key: bool) -> Result<Vec<u8>> {
        let pts = CMTime::new(self.next_pts, CLOCK_RATE);
        self.next_pts += i64::from(CLOCK_RATE) / i64::from(self.fps);

        let props = FrameProperties::new().with_force_key_frame(force_key);
        let frame = self
            .session
            .encode_with_properties(surface, pts, props)
            .map_err(|e| anyhow!("VideoToolbox 编码失败: {e}"))?;

        if frame.data.is_empty() {
            return Ok(Vec::new());
        }

        // 首个样本：提取并缓存参数集（SPS/PPS 只在这里能拿到）
        if self.params.is_none() {
            self.cache_parameter_sets(&frame);
        }

        let mut out = avcc_to_annexb(&frame.data, self.nal_len);

        if self.sps_profile_level.is_none() {
            self.sps_profile_level = parse_sps_profile_level(&out);
        }

        // 关键帧必须携带参数集：否则对端解码器无法初始化，或丢包重连后无法恢复
        if is_idr(&out) {
            if let Some(params) = &self.params {
                if !out.starts_with(params.as_slice()) {
                    let mut with_params = Vec::with_capacity(params.len() + out.len());
                    with_params.extend_from_slice(params);
                    with_params.extend_from_slice(&out);
                    out = with_params;
                }
            }
        }

        Ok(out)
    }

    /// 首帧 SPS 的 (profile_idc, constraint_flags, level_idc)。
    /// SDP 里的 profile-level-id 必须与之一致，否则部分解码端会直接黑屏
    /// ——这个坑本项目踩过多次。
    pub fn sps_profile_level(&self) -> Option<(u8, u8, u8)> {
        self.sps_profile_level
    }

    /// 把 SPS/PPS 以 Annex-B 形式拼成参数集块。
    fn cache_parameter_sets(&mut self, frame: &EncodedFrame) {
        let Some(sample_buffer) = frame.cm_sample_buffer() else {
            return;
        };
        let Some(desc) = sample_buffer.format_description() else {
            return;
        };
        let Ok(sets) = desc.video_parameter_sets() else {
            return;
        };

        if sets.nal_unit_header_length > 0 {
            self.nal_len = sets.nal_unit_header_length as usize;
        }

        let mut buf = Vec::new();
        for set in &sets.parameter_sets {
            buf.extend_from_slice(&START_CODE);
            buf.extend_from_slice(strip_start_code(set));
        }
        if !buf.is_empty() {
            tracing::info!(
                "[encode] VideoToolbox 参数集就绪：{} 个 NAL（{} 字节），NAL 长度字段 {} 字节",
                sets.parameter_sets.len(),
                buf.len(),
                self.nal_len
            );
            self.params = Some(buf);
        }
    }
}