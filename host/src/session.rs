//! 信令驱动的 WebRTC 会话（host 作 offerer）。
//!
//! 流程：iOS(viewer) 发 Connect -> host 建 PeerConnection + H264 视频轨道 + input DataChannel
//! -> 生成 offer 经信令发回 iOS -> iOS 回 answer -> 双方交换 ICE -> P2P 建立 -> host 推流。
//!
//! 复用已验证的 stream::create_peer_connection（协商层）与 demo_stream 的 sans-IO 事件循环。
//! 当前视频源为测试图案，真实 ScreenCaptureKit 采集在后续接入（见知识库路线图）。
use std::time::{Duration, Instant};

use anyhow::Result;
use bytes::BytesMut;
use makuris_proto::{InputEvent, SdpType, SignalMsg};
use rtc::peer_connection::event::RTCPeerConnectionEvent;
use rtc::peer_connection::message::{RTCMessage, TaggedRTCMessage};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::state::RTCPeerConnectionState;
use rtc::peer_connection::transport::{
    CandidateConfig, CandidateHostConfig, CandidateServerReflexiveConfig, RTCIceCandidate,
    RTCIceCandidateInit,
};
use rtc::rtp;
use rtc::rtp::packetizer::Packetizer;
use rtc::sansio::Protocol;
use rtc::shared::error::Error;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::stream;

/// 推送分辨率：屏幕 **2x 物理像素** 3024x1964。
///
/// 为什么必须给到物理像素：Retina 屏上逻辑分辨率 1512x982 只承载一半细节，
/// 采集逻辑像素再推流，等于把文字笔画原地丢掉一半——手机放大只能看到插值糊团。
/// 给到物理像素后，1:1 采样，放大时字形边缘是真实像素。
///
/// 代价与前提：软编（openh264）在此分辨率要 54ms/帧，超 30fps 预算，
/// 因此编码必须走 VideoToolbox 硬编（实测 15ms/帧，见 encode/vt.rs）。
/// 采集与编码在两条线程重叠执行（见 encode/pipeline.rs），克服采集约 27ms 的瓶颈。
#[cfg(target_os = "macos")]
const WIDTH: usize = 3024;
#[cfg(target_os = "macos")]
const HEIGHT: usize = 1964;
const FPS: u64 = 30;

/// 推流尺寸。macOS 用 2x 物理像素常量（理由见上）；
/// 其他平台按**真实屏幕尺寸**决定，因为台式机屏幕未知，
/// 而把 1080p 放大到 4K 不会增加任何信息——放大插值只多耗码率与编码时间。
/// 宽度上限 2560 且强制偶数（H264 要求宽高为偶数，奇数会被编码器拒绝）。
#[cfg(target_os = "macos")]
fn stream_size() -> (usize, usize) {
    (WIDTH, HEIGHT)
}

#[cfg(not(target_os = "macos"))]
fn stream_size() -> (usize, usize) {
    let (w, h) = crate::capture::detect_screen_size().unwrap_or((1920, 1080));
    let w = w.min(2560) & !1;
    let h = h.min(1600) & !1;
    (w.max(640), h.max(480))
}
const FRAME_DURATION: Duration = Duration::from_millis(1000 / FPS);
const RTP_OUTBOUND_MTU: usize = 1200;
const SSRC: u32 = 0x4F44_4553;
const PAYLOAD_TYPE: u8 = 102;
const CLOCK_RATE: u32 = 90000;

/// 会话内部事件：来自信令的 SDP/ICE，或停止信号。
pub enum SessionEvent {
    RemoteSdp { sdp_type: SdpType, sdp: String },
    RemoteIce { candidate: String, sdp_mid: Option<String>, sdp_mline_index: i32 },
    Stop,
}

/// 会话向信令发送的消息（offer/answer/ICE）。
pub enum SessionOut {
    Sdp { sdp_type: SdpType, sdp: String },
    Ice { candidate: String, sdp_mid: Option<String>, sdp_mline_index: i32 },
}

/// 目标码率：2x 物理分辨率下约 26 Mbps。
/// 硬编支持真正的码率控制（不像 openh264 关闭跳帧后只能固定 QP），
/// 码率给足，文字边缘的高频细节才不会被压糊。
#[cfg(target_os = "macos")]
const BITRATE_BPS: i32 = 26_000_000;

/// 探测本机编码器在当前分辨率下实际输出的 profile-level-id。
/// SDP 必须声明这个真实值——声明与编码器实际 SPS 不符时，部分解码端会直接黑屏，
/// 这个坑本项目已踩过多次。探测成本约 30ms，换来零不一致风险。
pub fn probe_profile_level_id(width: usize, height: usize) -> String {
    #[cfg(target_os = "macos")]
    {
        match crate::encode::pipeline::probe_profile_level_id(width, height, FPS as u32, BITRATE_BPS)
        {
            Ok(id) => {
                tracing::info!("[session] 编码器真实 profile-level-id = {}", id);
                return id;
            }
            Err(e) => {
                tracing::warn!(
                    "[session] profile-level-id 探测失败({})，回退 640033（High/Level 5.1）",
                    e
                );
            }
        }
        "640033".to_owned()
    }
    #[cfg(not(target_os = "macos"))]
    {
        // 非 macOS 走 openh264 软编。这里必须实测而不能写死：openh264 按分辨率
        // 自动挑选 level，1080p 与 720p 得到的 level 不同。SDP 声明一个比实际更低的
        // level 会让部分解码端直接拒收视频 m-line——macOS 上已踩过同样的坑。
        match crate::encode::source::probe_profile_level_id(width, height) {
            Ok(id) => {
                tracing::info!("[session] 编码器真实 profile-level-id = {}", id);
                return id;
            }
            Err(e) => {
                tracing::warn!(
                    "[session] profile-level-id 探测失败({})，回退 42c020（Constrained Baseline/Level 3.2）",
                    e
                );
            }
        }
        "42c020".to_owned()
    }
}

/// 运行一次完整的 WebRTC 会话（host 作 offerer）。
/// - `udp_port`: 本地 UDP 端口
/// - `event_rx`: 接收来自信令的 SDP/ICE/Stop
/// - `out_tx`: 向信令发送 offer/ICE
pub async fn run_session(
    _udp_port: u16,
    mut event_rx: mpsc::UnboundedReceiver<SessionEvent>,
    out_tx: mpsc::UnboundedSender<SessionOut>,
) -> Result<()> {
    // 1. 绑定 UDP（端口 0 让 OS 自动分配，避免多 session 端口冲突）
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    let local_addr = socket.local_addr()?;
    tracing::info!("[session] UDP bound on {}", local_addr);

    // 本机在局域网里的可达地址。不能拿 local_addr().ip() 充数：socket 绑的是
    // 0.0.0.0，取出来就是 0.0.0.0——对端拿到这个地址根本无处可发。这个 bug 之前
    // 一直藏着，因为同网段还有 Bonjour 提供的地址兜底，跨网络才会暴露。
    let lan_ip = crate::netinfo::local_ip();

    // 2. STUN 收集：问出本机在 NAT 上的公网映射，作为 srflx 候选。
    //
    // 必须在会话事件循环启动前做完——收集要在这个 socket 上收响应包，
    // 而事件循环一旦跑起来就独占了它的读取。也必须在生成 offer 前完成，
    // 否则候选来不及写进 SDP。
    //
    // 加总超时是为了不拖慢局域网场景：同网段本来不需要穿越 NAT，若 STUN
    // 服务器不通（常见于受限网络），没必要让用户对着转圈等上好几秒。
    let stun_servers = crate::stun::servers();
    let mapped = match tokio::time::timeout(
        Duration::from_millis(1800),
        crate::stun::discover(&socket, &stun_servers),
    )
    .await
    {
        Ok(Some(addr)) => Some(addr),
        Ok(None) => {
            tracing::warn!(
                "[session] STUN 全部未响应（{:?}），本次只有局域网候选：\
                 同一 Wi-Fi 仍可直连，跨网络会连不上。可用 MAKURIS_STUN 换服务器",
                stun_servers
            );
            None
        }
        Err(_) => {
            tracing::warn!("[session] STUN 收集超时，跳过公网候选");
            None
        }
    };

    // 2. 构建 PeerConnection（offerer，DTLS server 角色）+ H264 视频轨道
    // 先定尺寸，再探测编码器真实输出的 profile-level-id：level 随分辨率变化，
    // 所以顺序不能反。SDP 必须声明探测到的真实值，声明与编码器实际不符时
    // 部分解码端会直接拒收视频 m-line（本项目已踩过多次）。
    let (width, height) = stream_size();
    let profile_level_id = probe_profile_level_id(width, height);
    tracing::info!(
        "[session] 推流尺寸 {}x{}@{}fps，profile-level-id={}",
        width,
        height,
        FPS,
        profile_level_id
    );
    let (mut pc, sender_id) = stream::create_peer_connection(
        vec![rtc::peer_connection::transport::RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_string()],
            ..Default::default()
        }],
        &profile_level_id,
    )?;

    // 3. 创建 input DataChannel（iOS 经此回传鼠标/键盘事件）
    // 不可靠模式：鼠标移动丢包无所谓，避免重传导致延迟
    let dc_init = rtc::data_channel::RTCDataChannelInit {
        ordered: false,
        max_retransmits: Some(0),
        ..Default::default()
    };
    let _input_channel = pc.create_data_channel("makuris-input", Some(dc_init))?;
    tracing::info!("[session] input data channel created");

    // 4. 添加本地候选。
    //
    // 【踩坑记录，勿再“修”】host 候选的地址必须与 socket 的绑定地址严格一致
    // （这里绑的是 0.0.0.0，就必须写 0.0.0.0）。原因在 rtc-ice：收到入站包时它用
    // transport.local_addr（就是 socket 的绑定地址）去 find_local_candidate 做
    // **精确相等**匹配，匹配不上直接丢弃整包。曾经把这里改成真实的 192.168.1.3，
    // 理由是「0.0.0.0 对端发不进来」，结果入站包全被丢（日志刷
    // "Discarded message, not a valid local candidate from UDP:0.0.0.0:xxx"），
    // ICE 失败、推送 0 帧，现象是配对成功却永远黑屏。
    //
    // 跨网络的可达性不依赖这个地址：一是下面注入的 srflx 候选给出了 NAT 上的
    // 公网映射，二是本机是 offerer（ICE controlling），会主动向对端候选发
    // Binding 请求，对端据此就能反查出本机的真实源地址（prflx）并回包。
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
    pc.add_local_candidate(RTCIceCandidate::from(&candidate).to_json()?)?;
    tracing::info!(
        "[session] host candidate {}:{}（与 socket 绑定地址一致）",
        local_addr.ip(),
        local_addr.port()
    );

    // srflx：端口必须用本会话 socket 的端口——NAT 是按「内网 ip:port」分配映射的，
    // 换成别的端口去问，拿到的映射与这里写下的端口对不上，对端发来的包会被
    // NAT 丢弃（打洞看着成功却连不上，比直接失败更难查）。
    // raddr/rport 按 RFC 8445 填 base（即本机内网地址与该端口）。
    if let Some(mapped) = mapped {
        let related = lan_ip.map(|ip| ip.to_string()).unwrap_or_default();
        let candidate = CandidateServerReflexiveConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: mapped.ip().to_string(),
                port: mapped.port(),
                component: 1,
                ..Default::default()
            },
            rel_addr: related,
            rel_port: local_addr.port(),
            url: None,
        }
        .new_candidate_server_reflexive()?;
        pc.add_local_candidate(RTCIceCandidate::from(&candidate).to_json()?)?;
        tracing::info!(
            "[session] srflx candidate {}:{}（经 STUN 收集）",
            mapped.ip(),
            mapped.port()
        );
    }

    // 5. 生成 offer 并发给 iOS
    let offer_sdp = stream::create_offer_sdp(&mut pc)?;
    out_tx.send(SessionOut::Sdp { sdp_type: SdpType::Offer, sdp: offer_sdp })?;
    tracing::info!("[session] offer sent, waiting for answer...");

    // 6. 初始化 packetizer（编码器由流水线在独立线程持有，见下）
    let video_codec_rtp = {
        // 从 sender 取协商后的 codec 参数（payloader 需要）
        let mut sender = pc.rtp_sender(sender_id).ok_or_else(|| anyhow::anyhow!("no sender"))?;
        sender
            .get_parameters()
            .rtp_parameters
            .codecs
            .first()
            .map(|c| c.rtp_codec.clone())
            .ok_or_else(|| anyhow::anyhow!("no codec"))?
    };
    let mut packetizer = rtp::packetizer::new_packetizer(
        Instant::now(),
        RTP_OUTBOUND_MTU,
        PAYLOAD_TYPE,
        SSRC,
        video_codec_rtp.payloader()?,
        Box::new(rtp::sequence::new_random_sequencer()),
        CLOCK_RATE,
    );

    // 6.5 路线 A（定案）：host 侧 TURN Allocate + relay 候选注入。
    //
    // iOS WebRTC.xcframework 的 ObjC→C++ 凭证传递损坏（抓包定案：iPhone 恒发
    // 20/28 字节裸 Allocate，收到 401 挑战后从不发带凭证的第二步），iPhone 永远
    // 拿不到 relay；而 rtc-ice 0.21 无内置候选收集器，host 的 with_ice_servers
    // 是空操作。故由 host 自己用 rtc-turn Allocate，把 relay 候选 trickle 给
    // iPhone，iPhone 用普通 UDP 连 relay（实测可达），媒体经 TURN 服务器中转。
    // 详见 turn.rs 头部注释。
    let mut turn_alloc: Option<crate::turn::TurnAllocation> = None;
    let mut relay_local_addr: Option<std::net::SocketAddr> = None;
    // 已建 TURN permission 的对端公网 IP（permission 按 IP 匹配，去重避免每包重发）
    let mut turn_permitted: std::collections::HashSet<std::net::IpAddr> =
        std::collections::HashSet::new();
    if let Some(turn_cfg) = crate::stream::host_turn_config() {
        let turn_url = format!("turn:{}", turn_cfg.turn_addr);
        match crate::turn::TurnAllocation::connect(turn_cfg).await {
            Ok(alloc) => {
                let relay_sock =
                    alloc.relay_addr;
                tracing::info!("[session] TURN Allocate 成功，relay 候选 {}", relay_sock);

                let relay_cfg = rtc::ice::candidate::candidate_relay::CandidateRelayConfig {
                    base_config: rtc::ice::candidate::CandidateConfig {
                        candidate_id: String::new(),
                        foundation: format!("turn{}", relay_sock.port()),
                        // relay 类型偏好为 0：优先级最低，仅作直连全部失败时的兜底
                        priority: (relay_sock.port() as u32 & 0xff) * 256 + 1,
                        address: relay_sock.ip().to_string(),
                        network: "udp".to_string(),
                        port: relay_sock.port(),
                        component: 1,
                    },
                    rel_addr: local_addr.ip().to_string(),
                    rel_port: local_addr.port(),
                    url: Some(turn_url.clone()),
                };
                let candidate = relay_cfg
                    .new_candidate_relay()
                    .map_err(|e| anyhow::anyhow!("构造 relay 候选失败: {}", e))?;
                let candidate_str = candidate.marshal();
                // add_local_candidate 接受 RTCIceCandidateInit（非 async），把 relay 候选
                // 注入本端 ICE agent 参与连通性检查；url 字段供 getStats 归因到 TURN 服务器
                pc.add_local_candidate(RTCIceCandidateInit {
                    candidate: candidate_str.clone(),
                    sdp_mid: Some("0".to_string()),
                    sdp_mline_index: Some(0),
                    username_fragment: None,
                    url: Some(turn_url.clone()),
                })?;
                // offer 已发出，relay 候选走 trickle ICE 信令通道补给 iPhone
                out_tx.send(SessionOut::Ice {
                    candidate: candidate_str.clone(),
                    sdp_mid: Some("0".to_string()),
                    sdp_mline_index: 0,
                })?;
                tracing::info!("[session] relay 候选已 trickle: {}", candidate_str);

                relay_local_addr = Some(relay_sock);
                turn_alloc = Some(alloc);
            }
            Err(e) => {
                tracing::warn!("[session] TURN Allocate 失败（本次会话无中继，直连继续）: {}", e)
            }
        }
    }

    // 7. sans-IO 事件循环
    let mut connected = false;
    // 早于 answer 到达的远端候选。
    //
    // iOS 的 trickle 候选与 answer SDP 是信令上的两条独立消息，顺序没有任何
    // 保证（实测候选常常先到）。而 rtc 在 remote description 未设置时会对
    // add_remote_candidate 直接报 "remote description is not set" —— 候选全丢，
    // 连通性就只能靠 prflx 碰运气（对端主动发 Binding 才反查出地址），表现为
    // 同一对设备时通时不通。这里先攒着，等 set_remote_description 成功后补加。
    let mut pending_remote_candidates: Vec<RTCIceCandidateInit> = Vec::new();
    // 远端描述是否已就位（决定候选是立即加还是入待办队列）
    let mut remote_description_set = false;
    let mut frame_idx: u64 = 0;
    let mut buf = vec![0u8; 2000];
    let mut ticker = tokio::time::interval(FRAME_DURATION);
    let samples_per_frame = (FRAME_DURATION.as_secs_f64() * CLOCK_RATE as f64) as u32;
    let mut injector = crate::input::create_injector();

    // 采集+硬编流水线：采集与编码分两条线程，用 surface 池传递所有权让两者重叠执行。
    // 2x 分辨率下两者串行要 42ms（只剩 24fps，且毫无余量）；重叠后节拍由较慢的
    // 采集（约 27ms）决定，30fps 才稳。首帧编码后会自动核对真实 SPS 与 SDP 声明是否一致。
    // 采集+编码流水线：
    // - macOS：两条线程 + surface 池，VideoToolbox 硬编（见 encode/pipeline.rs 的性能说明）
    // - Windows/Linux：单线程采集+openh264 软编（见 encode/source.rs）
    // 两者对会话层暴露同一套接口（frames / shutdown），因此事件循环完全共用。
    #[cfg(target_os = "macos")]
    let mut pipeline = crate::encode::pipeline::spawn(
        width,
        height,
        FPS,
        BITRATE_BPS,
        Some(profile_level_id.clone()),
    )?;
    #[cfg(not(target_os = "macos"))]
    let mut pipeline = crate::encode::source::spawn(width, height, FPS)?;

    'event_loop: loop {
        // 出站数据包：选中 relay 对时 poll_write 的 local_addr 是 relay 地址
        // （非本机 socket 地址），这类包不能直发对端，必须经 TURN Send indication 转发
        while let Some(msg) = pc.poll_write() {
            if let (Some(relay), Some(alloc)) = (relay_local_addr, turn_alloc.as_ref()) {
                if msg.transport.local_addr == relay {
                    alloc.send_to_peer(msg.transport.peer_addr, msg.message.to_vec());
                    continue;
                }
            }
            let _ = socket.send_to(&msg.message, msg.transport.peer_addr).await;
        }

        // 输入回控：每轮循环开头立即读取 DataChannel，避免被帧率绑架（降低输入延迟）
        while let Some(TaggedRTCMessage { message, .. }) = pc.poll_read() {
            if let RTCMessage::DataChannelMessage(_channel_id, dc_msg) = message {
                let text = String::from_utf8_lossy(&dc_msg.data[..]);
                match serde_json::from_str::<InputEvent>(&text) {
                    Ok(ev) => {
                        // 每个事件都留 debug 痕：排障时把 RUST_LOG 开到 debug，
                        // 就能看到 iOS 到底发了什么、有没有断流。
                        // info 只报首个与每 300 个，避免被每秒几十个鼠标移动淹掉。
                        tracing::debug!("[session] 收到输入事件: {:?}", ev);
                        if let Err(e) = injector.inject(&ev) {
                            tracing::warn!("[session] 输入注入失败: {} ({:?})", e, ev);
                        } else {
                            let n = crate::input::note_injected();
                            if n == 1 || n % 300 == 0 {
                                tracing::info!("[session] 已注入 {} 个输入事件（最近: {:?}）", n, ev);
                            }
                        }
                    }
                    Err(e) => tracing::warn!("[session] 输入事件解析失败: {} ({})", e, text),
                }
            }
        }

        // 事件
        while let Some(event) = pc.poll_event() {
            match event {
                RTCPeerConnectionEvent::OnConnectionStateChangeEvent(state) => {
                    tracing::info!("[session] connection state: {:?}", state);
                    if state == RTCPeerConnectionState::Connected {
                        connected = true;
                        tracing::info!(
                            "[session] ✅ 已连接，开始推流 {}x{}@{}fps",
                            width,
                            height,
                            FPS
                        );
                    } else if state == RTCPeerConnectionState::Failed
                        || state == RTCPeerConnectionState::Closed
                    {
                        tracing::warn!("[session] 连接终止: {:?}", state);
                        break 'event_loop;
                    }
                }
                RTCPeerConnectionEvent::OnIceConnectionStateChangeEvent(state) => {
                    tracing::debug!("[session] ICE state: {:?}", state);
                }
                _ => {}
            }
        }

        // 超时调度
        let eto = pc.poll_timeout().unwrap_or(Instant::now() + Duration::from_secs(86400));
        let delay = eto.checked_duration_since(Instant::now()).unwrap_or(Duration::ZERO);
        if delay.is_zero() {
            pc.handle_timeout(Instant::now())?;
            continue;
        }

        let timer = tokio::time::sleep(delay);
        tokio::pin!(timer);

        tokio::select! {
            biased;
            _ = timer.as_mut() => {
                pc.handle_timeout(Instant::now())?;
            }
            turn_res = crate::turn::recv_or_pending(turn_alloc.as_mut()) => {
                // iPhone 经 TURN relay 送来的数据（Data indication）
                // → 以 relay 候选为 local_addr 喂给 ICE，等同 relay 上收到的包
                if let Some((peer_addr, data)) = turn_res {
                    let relay = relay_local_addr.unwrap_or(local_addr);
                    pc.handle_read(TaggedBytesMut {
                        now: Instant::now(),
                        transport: TransportContext {
                            local_addr: relay,
                            peer_addr,
                            ecn: None,
                            transport_protocol: TransportProtocol::UDP,
                        },
                        message: data,
                    })?;
                }
            }
            res = socket.recv_from(&mut buf) => {
                match res {
                    Ok((n, peer_addr)) => {
                        // 路线 A：见到 iPhone 公网地址 → 为其建 TURN permission
                        // （按 IP 匹配；iPhone 发给 relay 的 STUN 有重传，建权稍晚不致失败）
                        if let Some(alloc) = turn_alloc.as_ref() {
                            if turn_permitted.insert(peer_addr.ip()) {
                                tracing::info!("[session] 为 {} 创建 TURN permission", peer_addr);
                                alloc.create_permission(std::net::SocketAddr::new(
                                    peer_addr.ip(),
                                    0,
                                ));
                            }
                        }
                        pc.handle_read(TaggedBytesMut {
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
                        // Windows 专有陷阱：UDP socket 收到 ICMP「端口不可达」时，
                        // recv 返回 WSAECONNRESET(10054)，tokio 映射为
                        // ErrorKind::ConnectionReset。
                        //
                        // 这在 ICE 连通性检查阶段是常态而非异常：host 会向所有候选
                        // 地址并发 STUN Binding 请求，其中不可达的那些必然回 ICMP。
                        // 当成致命错误就会把整个会话 break 掉——实测 Windows 上
                        // answer 刚到、state 刚进 Connecting 的 8 毫秒后即
                        // 「socket error: 远程主机强迫关闭了一个现有的连接 (os error
                        // 10054)」→ 会话结束、共推送 0 帧，手机侧表现为连接超时。
                        // 同一份代码在 macOS 上不炸，因为 macOS 不把 ICMP 反射到 recv。
                        if e.kind() == std::io::ErrorKind::ConnectionReset {
                            tracing::debug!("[session] 忽略可恢复的 UDP 错误: {}", e);
                            continue 'event_loop;
                        }
                        tracing::error!("[session] socket error: {}", e);
                        break 'event_loop;
                    }
                }
            }
            ev = event_rx.recv() => {
                match ev {
                    Some(SessionEvent::RemoteSdp { sdp_type, sdp }) => {
                        if sdp_type == SdpType::Answer {
                            // 诊断：把收到的 answer 原文与逐条 m-line 的 ICE 凭据记进日志。
                            // 曾出现 iPhone 回的 answer 触发 rtc 的
                            // "multiple conflicting ice-ufrag values"（Chrome 同场景正常），
                            // 这类协商失败只有看到 SDP 原文才能判断是 offer 写法还是要容错解析。
                            tracing::info!("[session] ← 收到 answer SDP（{} 字节）:\n{}", sdp.len(), sdp);
                            for line in sdp.lines() {
                                if line.starts_with("m=")
                                    || line.starts_with("a=ice-ufrag")
                                    || line.starts_with("a=ice-pwd")
                                    || line.starts_with("a=mid")
                                    || line.starts_with("a=inactive")
                                    || line.starts_with("a=sendonly")
                                    || line.starts_with("a=recvonly")
                                    || line.starts_with("a=group")
                                {
                                    tracing::info!("[session]   ans| {}", line);
                                }
                            }
                            // 归属校验：这个 answer 是不是回给我们刚发出的 offer 的。
                            //
                            // 「切换连接线路」时，旧会话的 answer 可能在「旧会话已停、
                            // 新会话 offer 刚发出」的间隙到达。answer 里的 ice-ufrag
                            // 是应答方自己的凭据、与 offer 无关，无法直接比对，因此改用
                            // 「这个 ufrag 我是否已经采纳过」判定：同一 PC 的 ufrag 在其
                            // 生命周期内固定，采纳过的必然是上一轮会话的残留。
                            //
                            // 误采纳的后果是整场会话空转到超时（实测 Mac host.log
                            // 17:48:36：offer sent 后 9 毫秒就 answer received，随后
                            // 持续 pingAllCandidates called with no candidate pairs，
                            // 30 秒后 Failed、推送 0 帧）——因为拿旧 answer 设了 remote
                            // description，iOS 为本次会话发来的候选与之对不上，一个候选对
                            // 都形不成。拒掉残留、等本次会话真正的 answer 到达即可自愈。
                            if !accept_answer(ice_ufrag_of(&sdp).as_deref()) {
                                tracing::warn!(
                                    "[session] 忽略上一轮会话残留的 answer（ice-ufrag 已采纳过），\
                                     继续等待本次会话的应答"
                                );
                                continue 'event_loop;
                            }
                            let desc = RTCSessionDescription::answer(sdp)?;
                            pc.set_remote_description(Instant::now(), desc)?;
                            remote_description_set = true;

                            // 补加先于 answer 到达的那批候选。不补就是白丢——
                            // 那些正是对端真实可用的地址，连通性只能靠 prflx 碰运气。
                            if !pending_remote_candidates.is_empty() {
                                let n = pending_remote_candidates.len();
                                for init in pending_remote_candidates.drain(..) {
                                    // 候选原文必须落日志。answer SDP 只带 iOS 那侧
                                    // 等满 2 秒前收集完的候选，蜂窝下 srflx 往往在
                                    // trickle 阶段才出现；不打这行就会把「SDP 里没
                                    // 等到」误判成「对端拿不到公网地址」。
                                    tracing::info!("[session] 补加候选: {}", init.candidate);
                                    if let Err(e) = pc.add_remote_candidate(init) {
                                        tracing::warn!("[session] 迟到的候选补加失败: {}", e);
                                    }
                                }
                                tracing::info!("[session] 已补加 {} 条早到的远端候选", n);
                            }
                            tracing::info!("[session] answer received, negotiating...");
                        }
                    }
                    Some(SessionEvent::RemoteIce { candidate, sdp_mid, sdp_mline_index }) => {
                        tracing::info!("[session] 收到 trickle 候选: {}", candidate);
                        let init = RTCIceCandidateInit {
                            candidate,
                            sdp_mid,
                            sdp_mline_index: if sdp_mline_index >= 0 {
                                Some(sdp_mline_index as u16)
                            } else {
                                None
                            },
                            username_fragment: None,
                            url: None,
                        };
                        if remote_description_set {
                            if let Err(e) = pc.add_remote_candidate(init) {
                                tracing::warn!("[session] add remote candidate failed: {}", e);
                            }
                        } else {
                            // 还轮不到它：remote description 未设置时 rtc 会直接
                            // 拒绝（"remote description is not set"），先存起来。
                            pending_remote_candidates.push(init);
                        }
                    }
                    Some(SessionEvent::Stop) | None => {
                        tracing::info!("[session] stop signal");
                        break 'event_loop;
                    }
                }
            }
            _ = ticker.tick() => {
                if !connected {
                    continue;
                }
                // 从编码线程取已编码帧（采集+编码在独立线程，不阻塞本循环）
                let data = match pipeline.frames.try_recv() {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                if !data.is_empty() {
                    let packets = packetizer.packetize(Instant::now(), &data, samples_per_frame)?;
                    for mut packet in packets {
                        if let Some(mut sender) = pc.rtp_sender(sender_id) {
                            packet.header.ssrc = sender.track().ssrcs().last().unwrap_or(SSRC);
                            packet.header.payload_type = sender
                                .get_parameters()
                                .rtp_parameters
                                .codecs
                                .first()
                                .map(|c| c.payload_type)
                                .unwrap_or(PAYLOAD_TYPE);
                            if let Err(e) = sender.write_rtp(Instant::now(), packet) {
                                if e != Error::ErrClosedPipe {
                                    tracing::warn!("[session] write_rtp error: {}", e);
                                }
                            }
                        }
                    }
                }
                frame_idx += 1;
                if frame_idx % (FPS * 5) == 0 {
                    tracing::info!("[session] 已推送 {} 帧 (~{}s)", frame_idx, frame_idx / FPS);
                }
            }
        }
    }

    // 先停流水线再关连接：否则采集/编码线程会在连接已断的情况下继续空转，
    // 而且下一会话启动时旧线程仍占着编码器与 surface 池。
    pipeline.shutdown();
    pc.close()?;
    tracing::info!("[session] 会话结束，共推送 {} 帧", frame_idx);
    Ok(())
}

/// 本进程已采纳过的 answer 的 ICE 凭据（`a=ice-ufrag`）。
///
/// 为什么需要它：answer 与 offer 之间没有任何可比对的关联字段——answer 携带的
/// `a=ice-ufrag` 是**应答方自己**的凭据，与本次 offer 的 ufrag 无关，所以无法用
/// 「ufrag 是否匹配我们的 offer」来判定归属。于是「切换连接线路」时会出现串会话：
/// 旧会话的 answer 恰好在「旧会话已停、新会话 offer 刚发出」的间隙到达，新会话
/// 把它当成自己的应答设置了 remote description。
///
/// 实测后果（Mac host.log 17:48:36）：`offer sent` 之后 **9 毫秒**就
/// `answer received` —— 4G 往返不可能这么快，那必然是 3.8 秒前旧 offer 的应答。
/// 新会话据此建立的 ICE 凭据与 iOS 真正发来的 trickle 候选对不上，于是连续刷
/// `pingAllCandidates called with no candidate pairs`，30 秒后 Failed、推送 0 帧。
///
/// 判据成立的前提：一个 PeerConnection 的 ice-ufrag 在其生命周期内固定，而 iOS
/// 每次连接都新建 PC（teardownPeerConnection → 新 ufrag），因此「这个 ufrag 我
/// 已经采纳过」足以判定它是迟到的旧应答。代价是同一 PC 上的 re-offer 重协商会被
/// 误丢——本项目 iOS 不做 re-offer，若将来支持，需改成比对 offer 的会话版本号。
static ANSWER_UFRAG_SEEN: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::OnceLock::new();

/// 取 SDP 里第一条 `a=ice-ufrag` 的值，没有则 None。
fn ice_ufrag_of(sdp: &str) -> Option<String> {
    sdp.lines()
        .find_map(|l| l.strip_prefix("a=ice-ufrag:"))
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// 判断这个 answer 该不该被当前会话采纳；false 表示是迟到的旧应答，必须丢弃。
fn accept_answer(ufrag: Option<&str>) -> bool {
    let Some(ufrag) = ufrag else {
        // 解析不出 ufrag 的 SDP 极少见。宁可放行，也不要因为判据缺失把会话卡死。
        return true;
    };
    let seen = ANSWER_UFRAG_SEEN
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let mut guard = seen.lock().unwrap();
    if guard.contains(ufrag) {
        return false;
    }
    // 上限保护：进程长跑、反复切换线路会让集合无界增长。攒满就整表清空——被清掉
    // 的都是很久以前的凭据，同一串 ufrag 再出现一次的概率远低于让内存一直涨。
    if guard.len() >= 32 {
        guard.clear();
    }
    guard.insert(ufrag.to_owned());
    true
}

/// 全局唯一的活跃会话槽位。
///
/// 为什么必须是全局的：会话有两个入口——中央信令（main.rs）与局域网直连
/// （lan.rs）。这两处各持一个发送端变量、彼此看不见，于是「先经公网连上、
/// 再切到局域网」时会出现两个会话同时活着：两套采集线程、两个 VideoToolbox
/// 编码器同时抢同一块屏幕与 CPU。屏幕只有一块，任何时刻只允许一个会话持有它。
static ACTIVE_SESSION: std::sync::OnceLock<std::sync::Mutex<Option<ActiveSession>>> =
    std::sync::OnceLock::new();

/// 一个已登记的会话：既能被叫停（往 `event_tx` 发 Stop），
/// 也能被等待（`done_rx` 在会话任务真正收尾后收到信号）。
struct ActiveSession {
    /// 会话序号。会话自然结束（对端断开）时要清掉自己的槽位，否则下次连接
    /// 会对着一个早已结束的会话喊「先停掉它」——日志误导，而且白等一轮。
    /// 有了序号就能判断槽位里还是不是自己：期间已被新会话顶替时就不许清。
    id: u64,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
    done_rx: tokio::sync::oneshot::Receiver<()>,
}

/// 会话序号发生器。唯一用途是给槽位里的会话一个可比对的身份。
static SESSION_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 停掉当前活跃会话，并等它把采集/编码资源放干净。
///
/// 「等」这一步不能省：只发 Stop 就立刻开新会话的话，旧流水线的 shutdown 是
/// 异步的，两套采集/编码会在重叠的那段时间里同时跑——用户看到的仍然是
/// 「切过去就卡」。3 秒是兜底：正常情况下 pipeline.shutdown() 毫秒级完成，
/// 等不到说明旧会话已经卡死，那就不能再让它拖住新连接。
async fn stop_active_session() {
    let prev = {
        let slot = ACTIVE_SESSION.get_or_init(|| std::sync::Mutex::new(None));
        slot.lock().unwrap().take()
    };
    let Some(prev) = prev else { return };

    tracing::info!("[session] 已有会话在跑，先停掉它再建新会话（切换连接线路）");
    let _ = prev.event_tx.send(SessionEvent::Stop);
    let _ = tokio::time::timeout(Duration::from_secs(3), prev.done_rx).await;
}

/// 建立一次 WebRTC 会话：接通信令转发与会话任务，返回该会话的信令入口。
///
/// 放在这里而不是留在 main.rs：中央信令与局域网直连两条路径都要建会话，
/// 而「建会话」这件事本身就是一个安全动作——它意味着开始把屏幕推给对端。
/// 若两条路径各写一份，将来任何一处加了校验、另一处忘了，就是一个后门。
/// 合并成一处，准入判定（由调用方在 `pairing::admit` 里完成）只需正确一次。
///
/// `udp_port` 目前未使用（run_session 内部 bind 0 让 OS 分配端口），
/// 保留参数是为了与既有调用点签名一致。
pub async fn start_session(
    tx: &mpsc::UnboundedSender<tokio_tungstenite::tungstenite::Message>,
    host_id: &str,
    viewer_id: &str,
    udp_port: u16,
) -> Option<mpsc::UnboundedSender<SessionEvent>> {
    // 单会话互斥：无论这次 Connect 来自中央信令还是局域网直连，都先停掉
    // 正在跑的那个会话。这是「能切换线路」的前提——两条路径各持一个句柄
    // 时，旧线路的会话不会自己消失，它会继续占着屏幕与编码器。
    stop_active_session().await;

    tracing::info!("[session] 为 {} 建立会话", viewer_id);

    let (event_tx, event_rx) = mpsc::unbounded_channel::<SessionEvent>();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SessionOut>();

    // 会话输出（offer / ICE）经信令发回这个 viewer
    let fwd_tx = tx.clone();
    let host_id = host_id.to_owned();
    let viewer_id = viewer_id.to_owned();
    tokio::spawn(async move {
        while let Some(out) = out_rx.recv().await {
            let sig = match out {
                SessionOut::Sdp { sdp_type, sdp } => SignalMsg::Sdp {
                    from: host_id.clone(),
                    to: viewer_id.clone(),
                    sdp_type,
                    sdp,
                },
                SessionOut::Ice { candidate, sdp_mid, sdp_mline_index } => SignalMsg::Ice {
                    from: host_id.clone(),
                    to: viewer_id.clone(),
                    candidate,
                    sdp_mid,
                    sdp_mline_index,
                },
            };
            if let Ok(json) = serde_json::to_string(&sig) {
                use tokio_tungstenite::tungstenite::Message;
                let _ = fwd_tx.send(Message::Text(json.into()));
            }
        }
    });

    // 启动会话任务（host 作 offerer：采集 -> 编码 -> 推流）
    let my_id = SESSION_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        if let Err(e) = run_session(udp_port, event_rx, out_tx).await {
            tracing::error!("session error: {}", e);
        }
        // 会话自然结束时把槽位让出来（仅当里面还是自己）。不清的话，下一次
        // 连接会对着一条已经结束的会话白喊一声「先停掉它」，那些日志会
        // 把「切换线路时到底停了几次」这个关键信号淹掉。
        if let Some(slot) = ACTIVE_SESSION.get() {
            let mut guard = slot.lock().unwrap();
            if guard.as_ref().map(|s| s.id) == Some(my_id) {
                *guard = None;
            }
        }
        // 会话任务真正收尾（采集与编码线程都已关闭）之后才通知等待方。
        // 不能在发 Stop 时就认为它结束了：那时 pipeline.shutdown() 还没跑完。
        let _ = done_tx.send(());
    });

    // 登记为当前活跃会话，供下一次连接（无论走信令还是局域网）叫停
    {
        let slot = ACTIVE_SESSION.get_or_init(|| std::sync::Mutex::new(None));
        *slot.lock().unwrap() = Some(ActiveSession {
            id: my_id,
            event_tx: event_tx.clone(),
            done_rx,
        });
    }

    Some(event_tx)
}