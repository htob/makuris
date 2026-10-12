//! Makuris 端到端串流验证 demo。
//! 测试图案 → H264 编码 → RTP 打包 → WebRTC 推流 → 浏览器观看。
//!
//! 用法：
//!   cargo run -p makuris-host --bin demo_stream
//!   打开 host/viewer.html，点 "Start"，复制浏览器 SDP 粘贴到终端，
//!   再复制终端输出的 answer 粘贴回浏览器，即可看到实时画面。
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use bytes::{Bytes, BytesMut};
use openh264::encoder::Encoder;
use openh264::formats::{RgbSliceU8, YUVBuffer};
use rtc::interceptor::Registry;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MediaEngine};
use rtc::peer_connection::configuration::setting_engine::SettingEngineBuilder;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::event::RTCPeerConnectionEvent;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::state::RTCPeerConnectionState;
use rtc::peer_connection::transport::{
    CandidateConfig, CandidateHostConfig, RTCDtlsRole, RTCIceCandidate, RTCIceServer,
};
use rtc::peer_connection::RTCPeerConnectionBuilder;
use rtc::rtp;
use rtc::rtp::packetizer::Packetizer;
use rtc::rtp_transceiver::rtp_sender::{
    RtpCodecKind, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters,
    RTCRtpEncodingParameters,
};
use rtc::sansio::Protocol;
use rtc::shared::error::Error;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use tokio::net::UdpSocket;

const WIDTH: usize = 640;
const HEIGHT: usize = 480;
const FPS: u64 = 15;
const FRAME_DURATION: Duration = Duration::from_millis(1000 / FPS);
const RTP_OUTBOUND_MTU: usize = 1200;
const SSRC: u32 = 20261006;
const PAYLOAD_TYPE: u8 = 102;
const CLOCK_RATE: u32 = 90000;
const HOST_PORT: u16 = 9001;

fn read_line(prompt: &str) -> Result<String> {
    print!("{}", prompt);
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// 生成移动彩条测试图案（RGB，每像素 3 字节）
fn generate_test_pattern(frame_idx: u64) -> Vec<u8> {
    let mut rgb = vec![0u8; WIDTH * HEIGHT * 3];
    let offset = (frame_idx * 5) as usize;
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let i = (y * WIDTH + x) * 3;
            let bar = ((x + offset) / 80) % 6;
            let (r, g, b) = match bar {
                0 => (255u8, 255u8, 255u8),
                1 => (255, 255, 0),
                2 => (0, 255, 255),
                3 => (0, 255, 0),
                4 => (255, 0, 255),
                _ => (255, 0, 0),
            };
            rgb[i] = r;
            rgb[i + 1] = g;
            rgb[i + 2] = b;
        }
    }
    rgb
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("[demo] Makuris 端到端串流验证");
    println!("[demo] {}x{} @ {}fps, H264, UDP port {}", WIDTH, HEIGHT, FPS, HOST_PORT);

    // 1. 绑定 UDP socket
    let socket = UdpSocket::bind(format!("0.0.0.0:{}", HOST_PORT)).await?;
    let local_addr = socket.local_addr()?;
    println!("[demo] UDP bound on {}", local_addr);

    // 2. MediaEngine：仅注册 H264 视频
    let mut media_engine = MediaEngine::default();
    let video_codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: CLOCK_RATE,
            channels: 0,
            sdp_fmtp_line:
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: PAYLOAD_TYPE,
        ..Default::default()
    };
    media_engine.register_codec(video_codec.clone(), RtpCodecKind::Video)?;

    // 3. 构建 PeerConnection（Rust 作 answerer，DTLS server 角色）
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;
    let setting_engine = SettingEngineBuilder::new()
        .with_answering_dtls_role(RTCDtlsRole::Server)
        .build();
    let config = RTCConfigurationBuilder::new()
        .with_ice_servers(vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_string()],
            ..Default::default()
        }])
        .build();
    let mut peer_connection = RTCPeerConnectionBuilder::new()
        .with_configuration(config)
        .with_setting_engine(setting_engine)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build(Instant::now())?;

    // 4. 添加视频轨道
    let output_track = MediaStreamTrack::new(
        "makuris-stream".to_owned(),
        "makuris-video".to_owned(),
        "makuris-screen".to_owned(),
        RtpCodecKind::Video,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(SSRC),
                ..Default::default()
            },
            codec: video_codec.rtp_codec.clone(),
            ..Default::default()
        }],
    );
    let sender_id = peer_connection.add_track(output_track)?;
    println!("[demo] video track added");

    // 5. 读取浏览器 offer（base64 编码的 JSON）
    let offer_b64 = read_line("\n[demo] 粘贴浏览器 offer (base64) 后回车: ")?;
    let offer_json = String::from_utf8(B64.decode(&offer_b64)?)?;
    let offer: RTCSessionDescription = serde_json::from_str(&offer_json)?;
    println!("[demo] offer received");

    // 6. 协商：set remote → add local candidate → create answer
    peer_connection.set_remote_description(Instant::now(), offer)?;
    let candidate = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: local_addr.ip().to_string(),
            port: local_addr.port(),
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    peer_connection.add_local_candidate(RTCIceCandidate::from(&candidate).to_json()?)?;
    let answer = peer_connection.create_answer(None)?;
    peer_connection.set_local_description(Instant::now(), answer)?;

    // 7. 输出 answer（base64）供粘贴回浏览器
    let local_desc = peer_connection
        .local_description()
        .ok_or_else(|| anyhow::anyhow!("no local description"))?;
    let answer_json = serde_json::to_string(&local_desc)?;
    let answer_b64 = B64.encode(answer_json.as_bytes());
    println!("\n[demo] ===== 复制以下 answer 粘贴到浏览器 =====");
    println!("{}", answer_b64);
    println!("[demo] ===== answer 结束 =====\n");

    // 8. 初始化 H264 编码器 + RTP packetizer
    let mut encoder = Encoder::new()?;
    let mut packetizer = rtp::packetizer::new_packetizer(
        Instant::now(),
        RTP_OUTBOUND_MTU,
        PAYLOAD_TYPE,
        SSRC,
        video_codec.rtp_codec.payloader()?,
        Box::new(rtp::sequence::new_random_sequencer()),
        CLOCK_RATE,
    );

    // 9. 事件循环：pump PeerConnection + 推流
    let mut connected = false;
    let mut frame_idx: u64 = 0;
    let mut buf = vec![0u8; 2000];
    let mut ticker = tokio::time::interval(FRAME_DURATION);
    let samples_per_frame = (FRAME_DURATION.as_secs_f64() * CLOCK_RATE as f64) as u32;

    println!("[demo] 等待 WebRTC 连接建立...");

    'event_loop: loop {
        // pump 出站数据包
        while let Some(msg) = peer_connection.poll_write() {
            let _ = socket.send_to(&msg.message, msg.transport.peer_addr).await;
        }
        // pump 事件
        while let Some(event) = peer_connection.poll_event() {
            match event {
                RTCPeerConnectionEvent::OnConnectionStateChangeEvent(state) => {
                    println!("[demo] connection state: {:?}", state);
                    if state == RTCPeerConnectionState::Connected {
                        connected = true;
                        println!(
                            "[demo] ✅ 已连接！推流测试图案 {}x{}@{}fps",
                            WIDTH, HEIGHT, FPS
                        );
                    } else if state == RTCPeerConnectionState::Failed {
                        eprintln!("[demo] ❌ 连接失败");
                        break 'event_loop;
                    }
                }
                RTCPeerConnectionEvent::OnIceConnectionStateChangeEvent(state) => {
                    println!("[demo] ICE state: {:?}", state);
                }
                _ => {}
            }
        }

        // 超时调度
        let eto = peer_connection
            .poll_timeout()
            .unwrap_or(Instant::now() + Duration::from_secs(86400));
        let delay = eto
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        if delay.is_zero() {
            peer_connection.handle_timeout(Instant::now())?;
            continue;
        }

        let timer = tokio::time::sleep(delay);
        tokio::pin!(timer);

        tokio::select! {
            biased;
            _ = timer.as_mut() => {
                peer_connection.handle_timeout(Instant::now())?;
            }
            res = socket.recv_from(&mut buf) => {
                match res {
                    Ok((n, peer_addr)) => {
                        peer_connection.handle_read(TaggedBytesMut {
                            now: Instant::now(),
                            transport: TransportContext {
                                local_addr,
                                peer_addr,
                                ecn: None,
                                transport_protocol: TransportProtocol::UDP,
                            },
                            message: BytesMut::from(&buf[..n]),
                        })?;
                    }
                    Err(e) => {
                        eprintln!("[demo] socket error: {}", e);
                        break 'event_loop;
                    }
                }
            }
            _ = ticker.tick() => {
                if !connected {
                    continue;
                }
                // 生成测试图案 → H264 编码 → RTP 打包 → 推流
                let rgb = generate_test_pattern(frame_idx);
                let source = RgbSliceU8::new(&rgb, (WIDTH, HEIGHT));
                let mut yuv = YUVBuffer::new(WIDTH, HEIGHT);
                yuv.read_rgb(source);
                let bitstream = encoder.encode(&yuv)?;
                let data = Bytes::from(bitstream.to_vec());
                if !data.is_empty() {
                    let packets = packetizer.packetize(Instant::now(), &data, samples_per_frame)?;
                    for mut packet in packets {
                        if let Some(mut sender) = peer_connection.rtp_sender(sender_id) {
                            packet.header.ssrc =
                                sender.track().ssrcs().last().unwrap_or(SSRC);
                            packet.header.payload_type = sender
                                .get_parameters()
                                .rtp_parameters
                                .codecs
                                .first()
                                .map(|c| c.payload_type)
                                .unwrap_or(PAYLOAD_TYPE);
                            if let Err(e) = sender.write_rtp(Instant::now(), packet) {
                                if e != Error::ErrClosedPipe {
                                    eprintln!("[demo] write_rtp error: {}", e);
                                }
                            }
                        }
                    }
                }
                frame_idx += 1;
                if frame_idx % (FPS * 5) == 0 {
                    println!("[demo] 已推送 {} 帧 (~{}s)", frame_idx, frame_idx / FPS);
                }
            }
        }
    }

    peer_connection.close()?;
    println!("[demo] 结束，共推送 {} 帧", frame_idx);
    Ok(())
}
