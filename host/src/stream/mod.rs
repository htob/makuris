//! WebRTC 推流模块（基于 rtc 0.21 sans-IO API）。
//!
//! 被控端作为 offerer：构建 PeerConnection -> 注册 H264 视频轨道 -> 生成 offer SDP，
//! 经信令发给控制端(iOS/浏览器)；控制端回 answer，双方交换 ICE 后建立 P2P。
//! 编码后的 H264 帧经 rtp_sender.write_rtp 推入，由 rtc 负责 RTP 打包与 SRTP 加密。
//!
//! 注意：rtc 0.21 是 sans-IO 架构，PeerConnection 不自带网络/定时器，
//! 需由调用方 pump 事件循环（poll_write/poll_event/poll_timeout/handle_read/handle_timeout）。
//! 本模块当前提供协商层（构建+offer），事件循环与编码器集成在后续接入。
use std::time::{Duration, Instant};

use anyhow::Result;
use rtc::interceptor::Registry;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MediaEngine};
use rtc::peer_connection::configuration::setting_engine::SettingEngineBuilder;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::transport::{RTCDtlsRole, RTCIceServer};
use rtc::peer_connection::RTCPeerConnectionBuilder;
use rtc::rtp_transceiver::rtp_sender::{
    RtpCodecKind, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters,
};
use rtc::rtp_transceiver::RTCRtpSenderId;

/// H264 视频编码参数（packetization-mode=1，浏览器/iOS 通用）。
///
/// `profile_level_id` 必须是编码器实际输出 SPS 里的那三个字节，否则部分解码端会直接黑屏
/// ——这个坑本项目已经踩过多次（SDP 声明 42e01f 而编码器输出 42c016）。
/// 所以这里不再硬编码，改由调用方传入，取值来自真实编码一帧后解析出的 SPS。
fn h264_codec(profile_level_id: &str) -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: format!(
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile_level_id}"
            ),
            rtcp_feedback: vec![],
        },
        payload_type: 102,
        ..Default::default()
    }
}

/// 编码器不可用或探测失败时的兜底 SDP 声明（Baseline Level 4.0）。
/// 仅用于保证协商能走完，不代表编码器真实参数。
pub const FALLBACK_PROFILE_LEVEL_ID: &str = "42c028";

/// 被控端自己的 TURN 中继配置（路线 A，2026-10-10 定案）。
///
/// 为什么必须由 host 侧当 TURN 客户端：iPhone 侧的 WebRTC.xcframework 在 ObjC→C++
/// 传递链路上丢失 TURN 长期凭证——两种初始化器均实测过：运行时埋点显示 wrapper 层
/// 凭证非空，但服务器抓包里 iPhone 发出的 Allocate 永远停在 20/28 字节裸请求，
/// 收到 401 后只重传裸请求，100+ 字节的带凭证第二步零出现。框架层缺陷，业务代码不可修。
///
/// 而两端网络条件均已实测成立：iPhone 蜂窝 UDP 到云服务器双向通畅（401 挑战-响应
/// 往返完整）；host 侧经 UDP 3478 可成功 Allocate 拿到 relay。所以改由被控端自己
/// Allocate relay 候选、trickle 给 iPhone——iPhone 只需向一个普通 UDP 地址发包，
/// 绕开它的 TURN 客户端。
///
/// 凭证来源（2026-10-12 开源改造）：**不再硬编码**。信令服务器在设备注册成功后
/// 下发 coturn 时效性凭证（REST 风格 HMAC，几小时自动过期），host 存进进程内
/// 全局供建会话使用。源码零秘密，泄露的凭证也会很快作废。
pub struct TurnCreds {
    pub urls: Vec<String>,
    pub realm: String,
    pub username: String,
    pub credential: String,
}

static TURN_CREDS: std::sync::RwLock<Option<TurnCreds>> = std::sync::RwLock::new(None);

/// 信令下发 TurnCredentials 消息后由 main.rs 调用，存下当前有效凭证。
pub fn set_turn_creds(creds: TurnCreds) {
    if let Ok(mut g) = TURN_CREDS.write() {
        *g = Some(creds);
    }
}

/// 字段与构造方式对照 rtc-0.21.0 源码核实：
///   rtc::peer_connection::transport::RTCIceServer { urls: Vec<String>, username, credential }
///   RTCConfigurationBuilder::with_ice_servers(Vec<RTCIceServer>)（lib.rs 文档示例同款）
/// 凭证未下发（如纯局域网场景）时返回空列表，仅用 host/srflx 候选。
pub fn turn_ice_servers() -> Vec<RTCIceServer> {
    let guard = match TURN_CREDS.read() {
        Ok(g) => g,
        Err(poison) => poison.into_inner(),
    };
    match guard.as_ref() {
        Some(c) => vec![RTCIceServer {
            urls: c.urls.clone(),
            username: c.username.clone(),
            credential: c.credential.clone(),
        }],
        None => vec![],
    }
}

/// 构建一个带 H264 视频轨道的 PeerConnection。
/// 返回 (peer_connection, video_sender_id)。ice_servers 为空时仅用 host candidate（局域网）。
///
/// `profile_level_id` 为 6 位十六进制字符串（如 "640033"），须与编码器真实 SPS 一致。
pub fn create_peer_connection(
    ice_servers: Vec<RTCIceServer>,
    profile_level_id: &str,
) -> Result<(rtc::peer_connection::RTCPeerConnection, RTCRtpSenderId)> {
    let mut media_engine = MediaEngine::default();
    let video_codec = h264_codec(profile_level_id);
    media_engine.register_codec(video_codec.clone(), RtpCodecKind::Video)?;

    let registry = Registry::new();
    let registry = register_default_interceptors(registry, &mut media_engine)?;

    // 被控端作为 DTLS server（控制端为 client）
    let setting_engine = SettingEngineBuilder::new()
        .with_answering_dtls_role(RTCDtlsRole::Server)
        // ICE 重试窗口放宽。默认 200ms × 7 次 ≈ 1.4 秒就把一个候选对判死，
        // 而 iOS 的蜂窝 srflx 是 trickle 晚到的（answer SDP 里只有 2 秒内收集完
        // 的 host 候选），1.4 秒窗口等于必然错过。放宽到 400ms × 40 次 ≈ 16 秒，
        // 给 NAT 映射建立留出时间；总时长仍受 30 秒连接超时约束。
        .with_ice_connection_attempts(Some(Duration::from_millis(400)), Some(40))
        .build();

    // 路线 A（2026-10-10 定案）：iOS 的 WebRTC.xcframework 在 ObjC→C++ 传递链路上丢失
    // TURN 长期凭证——两种初始化器均实测：iPhone 的 Allocate 永远停在 20/28 字节裸请求，
    // 收到 401 后只重传裸请求，100+ 字节的带凭证第二步零出现（对照组：同一凭证在本机
    // turnutils_uclient 走 UDP 3478 可成功 Allocate）。故改由被控端自己充当 TURN 客户端：
    // 向云端 coturn Allocate 出 relay 候选，经 SDP trickle 给 iPhone；iPhone 只需连接
    // 普通 UDP relay 地址（蜂窝实测双向可达，401 挑战-响应往返完整即为证据）。
    // 凭证来自信令下发的时效 TurnCreds（turn_ice_servers 读进程内容器；
    // 未下发时为空列表，仅 host/srflx 直连候选）。
    let config = RTCConfigurationBuilder::new()
        .with_ice_servers(turn_ice_servers())
        .build();

    let mut pc = RTCPeerConnectionBuilder::new()
        .with_configuration(config)
        .with_setting_engine(setting_engine)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build(Instant::now())?;

    // 固定 SSRC（生产环境应随机化，避免多会话冲突）
    let ssrc: u32 = 0x4F44_4553; // "ODES"
    let track = MediaStreamTrack::new(
        "makuris-stream".to_owned(),
        "makuris-video-track".to_owned(),
        "makuris-video".to_owned(),
        RtpCodecKind::Video,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            codec: video_codec.rtp_codec.clone(),
            ..Default::default()
        }],
    );
    let sender_id = pc.add_track(track)?;
    Ok((pc, sender_id))
}

/// 生成 SDP offer 并设为本地描述，返回 offer 的 SDP 字符串。
pub fn create_offer_sdp(pc: &mut rtc::peer_connection::RTCPeerConnection) -> Result<String> {
    let offer = pc.create_offer(None)?;
    pc.set_local_description(Instant::now(), offer)?;
    let local = pc
        .local_description()
        .ok_or_else(|| anyhow::anyhow!("local description missing after create_offer"))?;
    Ok(local.sdp)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证 WebRTC 协商层在本机可用：能构建 PC、生成合法 offer、且声明 H264。
    /// 不依赖网络（空 ICE 列表），证明 rtc 0.21 栈编译且工作正常。
    #[test]
    fn offer_is_valid_and_advertises_h264() {
        let (mut pc, _sender) =
            create_peer_connection(vec![], "640033").expect("create pc");
        let sdp = create_offer_sdp(&mut pc).expect("create offer");
        assert!(sdp.contains("v=0"), "SDP 应以版本行开头");
        assert!(
            sdp.to_uppercase().contains("H264"),
            "SDP 应声明 H264 编码, got: {}",
            &sdp[..sdp.len().min(300)]
        );
        assert!(sdp.contains("m=video"), "SDP 应含 video 媒体段");
        // profile-level-id 必须原样出现在 fmtp 行，不能凭空改写
        assert!(
            sdp.contains("profile-level-id=640033"),
            "SDP 应带上调用方给的 profile-level-id, got: {}",
            &sdp[..sdp.len().min(500)]
        );
    }
}


/// host 侧路线 A 用的 TURN 配置（凭证来自信令下发的时效 TurnCreds，
/// 背景见 turn.rs 头注释）。返回 None 表示禁用 TURN 中继（凭证未下发，仅直连）。
pub fn host_turn_config() -> Option<crate::turn::TurnConfig> {
    let guard = match TURN_CREDS.read() {
        Ok(g) => g,
        Err(poison) => poison.into_inner(),
    };
    let creds = guard.as_ref()?;
    let url = creds.urls.iter().find(|u| u.starts_with("turn:"))?;
    // "turn:host:port?transport=udp" → "host:port"
    let turn_addr = url.trim_start_matches("turn:").split('?').next()?.to_string();
    Some(crate::turn::TurnConfig {
        turn_addr,
        username: creds.username.clone(),
        password: creds.credential.clone(),
        realm: creds.realm.clone(),
    })
}