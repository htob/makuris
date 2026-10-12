//! H264 编码模块。
//! 将采集到的帧编码为 H264 NAL 码流（Annex-B，含起始码），供 WebRTC 推流层打包为 RTP。
//!
//! 两条编码路径：
//! - `vt`（macOS 专用）：VideoToolbox 硬件编码。3024x1964 仅 ~15ms/帧，
//!   是支撑 2x 物理分辨率（文字清晰）的关键；
//! - 本文件的 `H264Encoder`：openh264 软编，作为非 macOS 平台的回退实现。
//!
//! 本文件同时提供平台无关的 NAL 工具函数（AVCC↔Annex-B、SPS 解析）——两条路径共用。
use anyhow::Result;
use openh264::encoder::Encoder;
use openh264::formats::{RgbSliceU8, YUVBuffer};

#[cfg(target_os = "macos")]
pub mod vt;

/// 采集+硬编流水线：采集与编码两条线程重叠执行，克服 2x 分辨率下的串行瓶颈。
pub mod pipeline;

/// 非 macOS 的采集+编码流水线（openh264 软编）。与 pipeline.rs 接口一致，
/// 由 session.rs 按平台选择。
/// 第二个条件让本机能用 `--features windows_typecheck` 对它做类型检查。
#[cfg(any(not(target_os = "macos"), feature = "windows_typecheck"))]
pub mod source;

/// Annex-B 起始码（4 字节形式）。
pub const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// 去掉 NAL 前的起始码（3 或 4 字节）；无起始码时原样返回。
pub fn strip_start_code(nal: &[u8]) -> &[u8] {
    if nal.starts_with(&START_CODE) {
        &nal[4..]
    } else if nal.starts_with(&[0x00, 0x00, 0x01]) {
        &nal[3..]
    } else {
        nal
    }
}

/// 码流首 NAL 是否为 IDR（nal_unit_type = 5）。
pub fn is_idr(annexb: &[u8]) -> bool {
    if annexb.len() < 5 || !annexb.starts_with(&START_CODE) {
        return false;
    }
    (annexb[4] & 0x1F) == 5
}

/// AVCC → Annex-B：把每个 NAL 的 big-endian 长度前缀换成起始码。
///
/// VideoToolbox 输出 AVCC，而下游 RTP 打包链路（rtc::rtp::packetizer）与解码端
/// 期望 Annex-B，这层转换不能省。
pub fn avcc_to_annexb(avcc: &[u8], nal_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(avcc.len() + 16);
    let mut i = 0usize;
    while i + nal_len <= avcc.len() {
        let mut len = 0usize;
        for k in 0..nal_len {
            len = (len << 8) | usize::from(avcc[i + k]);
        }
        i += nal_len;
        if len == 0 || i + len > avcc.len() {
            break;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&avcc[i..i + len]);
        i += len;
    }
    out
}

/// 在 Annex-B 码流里找 SPS（nal_unit_type = 7），返回
/// (profile_idc, constraint_flags, level_idc) 三个字节。
///
/// SPS 结构：NAL header | profile_idc | constraint_flags | level_idc | ...
/// 三个字节都要原样取用：约束标志位随 profile 而变（Baseline 常为 0xc0、High 常为 0x00），
/// 它不是常量。曾经把中间字节写死成 0xc0，导致 SDP 声明与真实 SPS 不符、解码端黑屏。
pub fn parse_sps_profile_level(annexb: &[u8]) -> Option<(u8, u8, u8)> {
    let mut i = 0usize;
    while i + 8 <= annexb.len() {
        if annexb[i..].starts_with(&START_CODE) {
            let nal_type = annexb[i + 4] & 0x1F;
            if nal_type == 7 {
                return Some((annexb[i + 5], annexb[i + 6], annexb[i + 7]));
            }
            i += 4;
        } else {
            i += 1;
        }
    }
    None
}

/// 把 SPS 的三个字节格式化成 SDP 的 profile-level-id，如 "42c033"、"640033"。
pub fn profile_level_id_hex(profile_idc: u8, constraint_flags: u8, level_idc: u8) -> String {
    format!("{profile_idc:02x}{constraint_flags:02x}{level_idc:02x}")
}

pub struct H264Encoder {
    inner: Encoder,
}

impl H264Encoder {
    pub fn new() -> Result<Self> {
        Ok(Self { inner: Encoder::new()? })
    }

    /// 编码一帧 RGB（每像素 3 字节，行优先，宽高为偶数）。
    /// 返回 H264 码流（首帧通常含 SPS/PPS/IDR 多个 NAL）。
    pub fn encode_rgb(&mut self, rgb: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
        let source = RgbSliceU8::new(rgb, (width, height));
        let mut yuv = YUVBuffer::new(width, height);
        yuv.read_rgb(source);
        let bitstream = self.inner.encode(&yuv)?;
        Ok(bitstream.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证编码器在本机可用：编码一张合成 RGB 图，输出应为合法 H264 码流（含起始码）。
    #[test]
    fn encodes_frame_to_valid_h264() {
        let (w, h) = (64usize, 64usize);
        let mut rgb = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 3;
                rgb[i] = x as u8;
                rgb[i + 1] = y as u8;
                rgb[i + 2] = 128;
            }
        }
        let mut enc = H264Encoder::new().expect("create encoder");
        let out = enc.encode_rgb(&rgb, w, h).expect("encode");
        assert!(out.len() > 4, "H264 输出过短: {} bytes", out.len());
        let has_start_code =
            out.windows(3).any(|s| s == [0, 0, 1]) || out.windows(4).any(|s| s == [0, 0, 0, 1]);
        assert!(
            has_start_code,
            "H264 输出缺少起始码, 前 8 字节: {:?}",
            &out[..out.len().min(8)]
        );
    }

    #[test]
    fn converts_avcc_to_annexb() {
        // 两个 NAL：[len=3] 65 aa bb | [len=2] 41 cc
        let avcc = [0, 0, 0, 3, 0x65, 0xaa, 0xbb, 0, 0, 0, 2, 0x41, 0xcc];
        let annexb = avcc_to_annexb(&avcc, 4);
        assert_eq!(
            annexb,
            vec![0, 0, 0, 1, 0x65, 0xaa, 0xbb, 0, 0, 0, 1, 0x41, 0xcc]
        );
    }

    #[test]
    fn detects_idr_and_parses_sps() {
        // SPS：00 00 00 01 67 64 00 33 ...（High profile / Level 5.1）
        let mut stream = vec![0, 0, 0, 1, 0x67, 0x64, 0x00, 0x33, 0x8c];
        assert_eq!(parse_sps_profile_level(&stream), Some((0x64, 0x00, 0x33)));
        assert_eq!(profile_level_id_hex(0x64, 0x00, 0x33), "640033");
        assert!(!is_idr(&stream));
        stream = vec![0, 0, 0, 1, 0x65, 0x88];
        assert!(is_idr(&stream));
    }

    #[test]
    fn strips_existing_start_code() {
        assert_eq!(strip_start_code(&[0, 0, 0, 1, 0x67]), &[0x67]);
        assert_eq!(strip_start_code(&[0, 0, 1, 0x67]), &[0x67]);
        assert_eq!(strip_start_code(&[0x67]), &[0x67]);
    }
}