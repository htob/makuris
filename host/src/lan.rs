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

/// 非 macOS 平台用纯 Rust 的 mDNS 广播
/// （Linux/Windows 上一般没有系统守护进程独占 5353 的问题）。
#[cfg(not(target_os = "macos"))]
fn start_bonjour(device_id: &str, port: u16) -> bool {
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("[lan] mDNS 守护进程启动失败: {}", e);
            return false;
        }
    };

    let ip = local_ip();
    let ip_text = ip.map(|a| a.to_string()).unwrap_or_default();
    let ips: Vec<IpAddr> = ip.into_iter().collect();
    let port_text = port.to_string();
    // TXT 记录必须带上 ip/port，与 macOS 分支保持一致。
    // iOS 侧优先用 TXT 里的 ip 直连（见 BonjourDiscovery.swift）：那是不依赖
    // 域名解析的地址。少了它，手机只能拿 "<实例名>.local" 去解析，而 Windows
    // 主机上的 .local 解析并不可靠（既非 Bonjour 原生平台，也没有 mDNSResponder），
    // 典型症状就是「列表里看得到这台电脑，点了没反应」。
    let props = vec![
        ("id".to_string(), device_id.to_string()),
        ("platform".to_string(), platform_name().to_string()),
        ("name".to_string(), hostname()),
        ("ip".to_string(), ip_text),
        ("port".to_string(), port_text),
    ];

    let info = match ServiceInfo::new(
        SERVICE_TYPE,
        &hostname(),
        &format!("{}.local.", device_id),
        ips.as_slice(),
        port,
        props.as_slice(),
    ) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("[lan] mDNS 服务描述构建失败: {}", e);
            return false;
        }
    };

    match daemon.register(info) {
        Ok(()) => {
            // daemon 必须保活：drop 掉它，广播就撤销了
            std::mem::forget(daemon);
            tracing::info!("[lan] mDNS 广播已启动: {} (端口 {})", SERVICE_TYPE, port);
            true
        }
        Err(e) => {
            tracing::warn!("[lan] mDNS 注册失败: {}", e);
            false
        }
    }
}

// 本机局域网地址探测已抽到 crate::netinfo（三处共用：此处广播、WebRTC 候选、
// 公网自检 bin），实现与「为什么不能用 connect 外网取 local_addr」的说明见该模块。

fn hostname() -> String {
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