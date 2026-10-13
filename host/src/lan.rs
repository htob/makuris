//! 局域网直连：host 内置信令端点 + Bonjour 广播。
//!
//! 为什么要有这个模块：
//! 中央信令服务器是为跨网络（公网、NAT 穿透）场景准备的，但局域网内
//! 手机和电脑本来就在同一网段、互相可见，却要求用户去查一串
//! `ws://192.168.x.x:8443/ws` 并手敲进去——这是把实现细节当成了配置项。
//! 局域网应该是零配置的：打开 app 就看到电脑。
//!
//! 所以 host 自己做两件事：
//!   1. 用 Bonjour（`_makuris._tcp`）广播自己，手机浏览即得，不需要输入任何地址；
//!   2. 内置一个轻量信令端点，手机连上来直接开始会话，不必经过外部服务器。
//!
//! 消息格式沿用 `SignalMsg`，与中央信令完全一致，因此 iOS 侧可以复用
//! 同一个 `SignalingClient`，只是把连接的 URL 指向这台电脑而不是服务器。

// 只在非 macOS 分支用到（service_info 的 ip 字段、start_bonjour 的 V4 判定）；
// macOS 走系统 Bonjour，不需要它，不加门控会留下 unused import warning。
#[cfg(not(target_os = "macos"))]
use std::net::IpAddr;

use crate::netinfo::local_ip;

use anyhow::Result;
use futures::{SinkExt, StreamExt};
// 非 macOS 才用纯 Rust 的 mDNS；macOS 走系统 Bonjour（见 crate::bonjour），
// 因为系统 mDNSResponder 独占 5353，独立 daemon 收不到多播。
#[cfg(not(target_os = "macos"))]
use mdns_sd::{ServiceDaemon, ServiceInfo};
use makuris_proto::{DeviceInfo, Role, SignalMsg};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use crate::session;

/// Bonjour 服务类型（mdns-sd 用的形式）。
/// iOS 侧的 NSBonjourServices 必须声明同一个字符串，
/// 否则 iOS 14+ 会拒绝浏览（且不报错，只是永远发现不了设备）。
#[allow(dead_code)]
pub const SERVICE_TYPE: &str = "_makuris._tcp.local.";

/// 系统 Bonjour API 用的服务类型：结尾带点但不含 `.local.`，
/// 域名由 domain 参数单独给（传 NULL 即默认 local.）。
#[allow(dead_code)]
pub const REGTYPE: &str = "_makuris._tcp.";

/// LAN 直连信令端口。刻意与中央信令服务器（默认 8443）错开，
/// 两者可以同时运行：一个管跨网络，一个管同网段零配置。
pub const LAN_PORT: u16 = 8444;

/// 实际使用的 LAN 端口：默认 `LAN_PORT`，可用 MAKURIS_LAN_PORT 覆盖。
///
/// 为什么需要这个开关：端口写死 8444 时，本机只要已经跑着一个实例，第二个
/// 实例就会在 bind 阶段直接失败——报的是 `Address already in use`，而且只
/// 出现在日志里。外面看到的症状是「局域网连不上」，很容易被误判成网络问题。
/// 能改端口，回归测试就能与日常实例共存，不必先停掉用户正在用的那一个。
pub fn resolve_port() -> u16 {
    std::env::var("MAKURIS_LAN_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(LAN_PORT)
}

/// 启动 LAN 服务（长驻）。
///
/// 返回类型是 `Result<()>` 且内部 `loop` 不退出——调用方应当 `tokio::spawn` 它，
/// 这样 Bonjour 守护进程的生命周期就和任务绑定（daemon 被 drop 会注销广播）。
pub async fn serve(device_id: String, port: u16) -> Result<()> {
    let advertised = start_bonjour(&device_id, port);

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(
        "[lan] 局域网直连已就绪：本机 {}，端口 {}，Bonjour {}",
        local_ip().map(|ip| ip.to_string()).unwrap_or_else(|| "未知".into()),
        port,
        if advertised { "已广播" } else { "未广播（可在 app 内手填地址兜底）" }
    );

    loop {
        let (stream, peer) = listener.accept().await?;
        let id = device_id.clone();
        tokio::spawn(async move {
            match handle_viewer(stream, id).await {
                Ok(()) => tracing::info!("[lan] 来自 {} 的会话结束", peer),
                Err(e) => tracing::warn!("[lan] 来自 {} 的连接出错: {}", peer, e),
            }
        });
    }
}

/// 处理一个 viewer（手机）的直连会话。
///
/// 与中央信令路径的区别只有一点：这里没有第三方转发，`Connect` 直接被自己接受，
/// SDP/ICE 原地往返。因此不需要 `to` 字段参与路由。
async fn handle_viewer(stream: TcpStream, device_id: String) -> Result<()> {
    let ws = accept_async(stream).await?;
    let (mut sink, mut source) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    // 统一写出口：业务逻辑往 channel 发，这个任务负责写进 WebSocket
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    let mut session_events: Option<mpsc::UnboundedSender<session::SessionEvent>> = None;

    while let Some(Ok(msg)) = source.next().await {
        let Message::Text(text) = msg else { continue };
        let Ok(signal) = serde_json::from_str::<SignalMsg>(&text) else {
            continue;
        };

        match signal {
            SignalMsg::Register { .. } => {
                // 直连模式下，手机已经是"连上这台电脑"的状态了，注册只是流程对齐
                send_json(&tx, &SignalMsg::Registered { device_id: device_id.clone() });
            }

            SignalMsg::ListDevices => {
                // 直连端点只知道自己，但保持这个能力可以让 iOS 复用同一套客户端
                send_json(
                    &tx,
                    &SignalMsg::DeviceList {
                        devices: vec![DeviceInfo {
                            device_id: device_id.clone(),
                            role: Role::Host,
                            platform: Some(platform_name().to_string()),
                            // 局域网直连这条路也带上主机名，App 在哪个通道下
                            // 看到的都是同一套展示逻辑，不必两套。
                            name: Some(hostname()),
                        }],
                    },
                );
            }

            SignalMsg::Connect { from, token, .. } => {
                // 局域网路径同样要过准入。别因为「同一个 Wi-Fi」就放行：访客网络、
                // 合租宽带、被人拿下的一台设备都在同一网段里；而且这是公网之外
                // 唯一能建会话的入口，漏了它，前面的加固全白做。
                match crate::pairing::admit(&from, token.as_deref()) {
                    crate::pairing::Admission::Granted => {
                        session_events =
                            session::start_session(&tx, &device_id, &from, 0).await;
                    }
                    crate::pairing::Admission::Challenge(nonce) => {
                        tracing::info!("[lan] {} 尚未配对，发出挑战", from);
                        send_json(
                            &tx,
                            &SignalMsg::AuthRequired {
                                from: device_id.clone(),
                                to: from.clone(),
                                nonce,
                            },
                        );
                    }
                    crate::pairing::Admission::Denied(reason) => {
                        tracing::warn!("[lan] 拒绝 {}：{}", from, reason);
                        send_json(
                            &tx,
                            &SignalMsg::AuthDenied {
                                from: device_id.clone(),
                                to: from.clone(),
                                reason,
                            },
                        );
                    }
                }
            }

            SignalMsg::AuthResponse { from, proof, .. } => {
                match crate::pairing::respond(&from, &proof) {
                    Ok(token) => {
                        send_json(
                            &tx,
                            &SignalMsg::AuthGranted {
                                from: device_id.clone(),
                                to: from.clone(),
                                token,
                            },
                        );
                        // 配对成功即开始会话：用户刚输完码，不该再要求他回列表点一次
                        session_events =
                            session::start_session(&tx, &device_id, &from, 0).await;
                    }
                    Err(reason) => {
                        send_json(
                            &tx,
                            &SignalMsg::AuthDenied {
                                from: device_id.clone(),
                                to: from.clone(),
                                reason,
                            },
                        );
                    }
                }
            }

            SignalMsg::Sdp { sdp_type, sdp, .. } => {
                if let Some(etx) = &session_events {
                    let _ = etx.send(session::SessionEvent::RemoteSdp { sdp_type, sdp });
                }
            }

            SignalMsg::Ice { candidate, sdp_mid, sdp_mline_index, .. } => {
                if let Some(etx) = &session_events {
                    let _ = etx.send(session::SessionEvent::RemoteIce {
                        candidate,
                        sdp_mid,
                        sdp_mline_index,
                    });
                }
            }

            SignalMsg::Ping => send_json(&tx, &SignalMsg::Pong),

            other => tracing::debug!("[lan] 忽略消息: {:?}", other),
        }
    }

    Ok(())
}

fn send_json(tx: &mpsc::UnboundedSender<Message>, msg: &SignalMsg) {
    if let Ok(json) = serde_json::to_string(msg) {
        let _ = tx.send(Message::Text(json.into()));
    }
}

/// 广播 Bonjour 服务，让手机在同一 Wi-Fi 下自动发现本机。
/// 返回是否成功把广播交给系统/守护进程。
///
/// 失败只记警告——局域网直连本身不依赖广播，用户仍可在 app 里手填地址兜底。
#[cfg(target_os = "macos")]
fn start_bonjour(device_id: &str, port: u16) -> bool {
    // macOS 必须走系统 Bonjour。本机实测：mdns-sd 自注册后连自己都浏览不到
    // （resolved = 0），而系统 dns-sd 注册立刻可见——因为 mDNSResponder 独占
    // 5353。详见 src/bonjour.rs 的说明。
    let host_name = hostname();
    // TXT 记录里带上 IP 与端口：手机侧浏览到即可直接拼出连接地址，
    // 不必再做一次地址解析——多网卡（含代理虚拟网卡）场景下，
    // 解析结果可能选到错误网卡，而这里给的是 host 自己认准的局域网地址。
    let ip_text = local_ip().map(|ip| ip.to_string()).unwrap_or_default();
    let port_text = port.to_string();
    let txt = [
        ("id", device_id),
        ("platform", platform_name()),
        ("name", host_name.as_str()),
        ("ip", ip_text.as_str()),
        ("port", port_text.as_str()),
    ];
    match crate::bonjour::register(&host_name, REGTYPE, port, &txt) {
        Ok(()) => {
            tracing::info!("[lan] Bonjour 广播已提交系统（{}，端口 {}）", REGTYPE, port);
            true
        }
        Err(e) => {
            tracing::warn!("[lan] Bonjour 广播失败: {}", e);
            false
        }
    }
}

/// mDNS 服务描述：实例名、地址、端口与 TXT 记录。
///
/// TXT 记录必须带上 ip/port，与 macOS 分支保持一致。
/// iOS 侧优先用 TXT 里的 ip 直连（见 BonjourDiscovery.swift）：那是不依赖
/// 域名解析的地址。少了它，手机只能拿 "<实例名>.local" 去解析，而 Windows
/// 主机上的 .local 解析并不可靠（既非 Bonjour 原生平台，也没有 mDNSResponder），
/// 典型症状就是「列表里看得到这台电脑，点了没反应」。
#[cfg(not(target_os = "macos"))]
fn service_info(device_id: &str, port: u16) -> Option<ServiceInfo> {
    let ip = local_ip();
    let ip_text = ip.map(|a| a.to_string()).unwrap_or_default();
    let ips: Vec<IpAddr> = ip.into_iter().collect();
    let port_text = port.to_string();
    let props = vec![
        ("id".to_string(), device_id.to_string()),
        ("platform".to_string(), platform_name().to_string()),
        ("name".to_string(), hostname()),
        ("ip".to_string(), ip_text),
        ("port".to_string(), port_text),
    ];

    match ServiceInfo::new(
        SERVICE_TYPE,
        &hostname(),
        &format!("{}.local.", device_id),
        ips.as_slice(),
        port,
        props.as_slice(),
    ) {
        Ok(i) => Some(i),
        Err(e) => {
            tracing::warn!("[lan] mDNS 服务描述构建失败: {}", e);
            None
        }
    }
}

/// 起一个 mDNS 守护进程并把广播注册进去。首次启动与自检失败后的重建都走这里。
#[cfg(not(target_os = "macos"))]
fn spawn_mdns_daemon(device_id: &str, port: u16) -> Option<ServiceDaemon> {
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("[lan] mDNS 守护进程启动失败: {}", e);
            return None;
        }
    };
    let info = service_info(device_id, port)?;
    match daemon.register(info) {
        Ok(()) => Some(daemon),
        Err(e) => {
            tracing::warn!("[lan] mDNS 注册失败: {}", e);
            None
        }
    }
}

/// mDNS 组播地址与端口（RFC 6762）。
#[cfg(not(target_os = "macos"))]
const MDNS_MULTICAST_V4: std::net::Ipv4Addr = std::net::Ipv4Addr::new(224, 0, 0, 251);
#[cfg(not(target_os = "macos"))]
const MDNS_PORT: u16 = 5353;
/// 自检窗口：要盖住 mdns-sd 的探测+公告阶段（注册后 1 秒内发完）。
#[cfg(not(target_os = "macos"))]
const MDNS_SELFCHECK_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);
/// 自检没通过时最多重建几次广播（退避 2s / 6s / 18s，每次重建后重新自检）。
#[cfg(not(target_os = "macos"))]
const MDNS_REBUILD_LIMIT: u32 = 3;

/// 开一个只用来旁听的 socket：与 mdns-sd 共享 5353 端口并加入 mDNS 组播组。
///
/// 必须设 SO_REUSEADDR：Windows 上 5353 恒被系统 Dnscache 与 chrome/Edge 一起
/// 共享，不带这个选项连绑都绑不上（实测报 Only one usage of each socket
/// address）。只在局域网那块网卡上入组——多网卡机器（本机有 6 张 169.254
/// 幽灵网卡）入错网卡同样收不到包。
#[cfg(not(target_os = "macos"))]
fn open_mdns_listener(local: std::net::Ipv4Addr) -> Option<std::net::UdpSocket> {
    let sock = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .ok()?;
    sock.set_reuse_address(true).ok()?;
    let any = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, MDNS_PORT));
    sock.bind(&any.into()).ok()?;
    if let Err(e) = sock.join_multicast_v4(&MDNS_MULTICAST_V4, &local) {
        tracing::warn!("[lan] mDNS 自检：在 {} 上加入组播组失败: {}", local, e);
        return None;
    }
    // 读超时：窗口内轮询，不能一直阻塞
    sock.set_read_timeout(Some(std::time::Duration::from_millis(250)))
        .ok()?;
    Some(sock.into())
}

/// 窗口内是否看到「本机自己发出」的 mDNS 包。
///
/// 这是本机唯一可靠的「广播真的生效了吗」判据：mdns-sd 若只建成了 IPv6 socket
/// （2026-10-13 台式机实测的故障形态），IPv4 组播上就一个包都不会有，而手机是
/// 走 IPv4 浏览的——于是「进程正常、8444 正常、日志正常，就是看不到这台电脑」。
/// 只认「源地址 = 本机局域网 IP 且源端口 = 5353」的包，避免把邻居的 mDNS 流量
/// 当成自己的广播。
#[cfg(not(target_os = "macos"))]
fn saw_own_mdns(
    sock: &std::net::UdpSocket,
    local: std::net::Ipv4Addr,
    window: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + window;
    let mut buf = [0u8; 2048];
    while std::time::Instant::now() < deadline {
        match sock.recv_from(&mut buf) {
            Ok((_, from)) => {
                if let std::net::SocketAddr::V4(v4) = from {
                    if *v4.ip() == local && v4.port() == MDNS_PORT {
                        return true;
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                tracing::debug!("[lan] mDNS 自检读取失败: {}", e);
                return false;
            }
        }
    }
    false
}

/// 广播自检 + 失败重建（跑在后台线程里，不拖慢「局域网直连已就绪」）。
///
/// 为什么要有这段：mdns-sd 建 IPv4 socket 失败时只在内部打 debug 级日志，默认
/// 过滤器把日志吞掉——进程活着、8444 在听、日志一片正常，唯一症状是「同一
/// Wi-Fi 下手机看不到这台电脑」，而当时的恢复手段只有重启碰运气（2026-10-13
/// 实测）。这里在注册后旁听 mDNS 组播：窗口内没看到本机发出的包，就判定广播
/// 没生效，重建 daemon 重试；连续失败则明确报错，把「该重启被控端了」变成
/// 日志里一条看得懂的记录，而不是靠猜。
#[cfg(not(target_os = "macos"))]
fn selfcheck_mdns(
    mut daemon: ServiceDaemon,
    listener: std::net::UdpSocket,
    local: std::net::Ipv4Addr,
    device_id: String,
    port: u16,
) {
    let mut attempt = 0u32;
    loop {
        if saw_own_mdns(&listener, local, MDNS_SELFCHECK_WINDOW) {
            if attempt == 0 {
                tracing::info!("[lan] mDNS 自检通过：{} 上已发出广播", local);
            } else {
                tracing::warn!("[lan] mDNS 广播已重建成功（第 {} 次重试，地址 {}）", attempt, local);
            }
            // 线程退出前把 daemon 保活：drop 掉它，广播就撤销了
            std::mem::forget(daemon);
            return;
        }
        if attempt >= MDNS_REBUILD_LIMIT {
            tracing::error!(
                "[lan] mDNS 广播自检连续 {} 次未通过：{} 秒内没看到本机在 IPv4 组播上发出的包，\
                 同一 Wi-Fi 下很可能看不到这台电脑（在 app 里手填 {} 仍可直接连）。\
                 排查：在 host.log 里搜 mdns_sd 的 \"failed to set up IPv4 socket\" 或 \
                 \"failed to join multicast\" 看具体原因；重启被控端可临时恢复。",
                attempt,
                MDNS_SELFCHECK_WINDOW.as_secs(),
                local
            );
            // 仍然保活最后一次建起来的 daemon：它至少还有 IPv6 那一半
            std::mem::forget(daemon);
            return;
        }
        let backoff = std::time::Duration::from_secs(2 * 3u64.pow(attempt));
        attempt += 1;
        tracing::warn!(
            "[lan] mDNS 自检未通过：{:?} 内没看到本机发出的 mDNS 包，{:?} 后重建广播（第 {} 次）",
            MDNS_SELFCHECK_WINDOW,
            backoff,
            attempt
        );
        std::thread::sleep(backoff);
        // 重建前必须先撤掉旧实例：不撤的话同一个实例名会在组播上并存，mdns-sd
        // 探测到名字冲突会把新实例改名，手机列表里就冒出「user (2)」这种重复项
        // （2026-10-13 复核发现：原实现直接 forget 旧 daemon，没有撤销这一步）。
        let _ = daemon.shutdown();
        match spawn_mdns_daemon(&device_id, port) {
            Some(d) => daemon = d,
            None => {
                tracing::warn!("[lan] mDNS 广播重建失败，放弃自愈；重启被控端可临时恢复");
                return;
            }
        }
    }
}

/// 非 macOS 平台用纯 Rust 的 mDNS 广播
/// （Linux/Windows 上一般没有系统守护进程独占 5353 的问题）。
#[cfg(not(target_os = "macos"))]
fn start_bonjour(device_id: &str, port: u16) -> bool {
    // 先开旁听、后注册：mdns-sd 的探测与公告都在 register() 里发起，
    // 顺序反了就监听不到自己的包。
    let local_v4 = match local_ip() {
        Some(IpAddr::V4(v4)) => Some(v4),
        _ => None,
    };
    let listener = local_v4.and_then(open_mdns_listener);

    let daemon = match spawn_mdns_daemon(device_id, port) {
        Some(d) => d,
        None => return false,
    };
    tracing::info!("[lan] mDNS 广播已启动: {} (端口 {})", SERVICE_TYPE, port);

    match (listener, local_v4) {
        (Some(listener), Some(local)) => {
            let device_id = device_id.to_string();
            // daemon 的所有权交给自检线程：它负责保活（正常路径 forget），并在重建
            // 时先 shutdown 掉旧实例——否则同名实例会在组播上并存，mdns-sd 检测到
            // 名字冲突会给新实例改名，手机列表里冒出「user (2)」这种重复项。
            std::thread::spawn(move || selfcheck_mdns(daemon, listener, local, device_id, port));
        }
        _ => {
            // 没开旁听 socket 时由主线程保活：drop 掉 daemon 就等于撤销广播
            std::mem::forget(daemon);
            tracing::warn!("[lan] mDNS 自检跳过：没取到局域网 IPv4 地址，无法判断广播是否真的发出");
        }
    }

    true
}

// 本机局域网地址探测已抽到 crate::netinfo（三处共用：此处广播、WebRTC 候选、
// 公网自检 bin），实现与「为什么不能用 connect 外网取 local_addr」的说明见该模块。

/// 本机名字（mDNS 服务名、TXT 的 name、以及上报信令的设备名共用）。
/// pub：main.rs 注册信令时也要用它，两处必须一致，否则同一个通道下
/// 局域网发现的名字和信令列表的名字会对不上。
pub fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().trim_end_matches(".local").to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Makuris".to_string())
}

fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}