//! Makuris 被控端（Host）。
//! 职责：连接信令服务器注册，等待控制端(iOS)的连接请求；
//! 收到请求后启动 WebRTC 会话：屏幕采集 -> H264 编码 -> 推流，接收输入事件并注入系统。
// bonjour.rs 是 macOS 专有实现：它 FFI 调用系统 libdns_sd 的 DNSServiceRegister，
// 这些符号在 Windows 上根本不存在，无条件编译会直接链接失败。
// 非 macOS 的广播走 lan.rs 里基于 mdns-sd 的分支。
#[cfg(target_os = "macos")]
mod bonjour;
mod capture;
mod encode;
mod input;
mod lan;
mod menubar;
mod netinfo;
mod pairing;
mod session;
mod stream;
mod stun;
mod tls_pin;
mod turn;

/// 默认日志目录，按各平台惯例：
/// - macOS：~/Library/Logs/Makuris
/// - Windows：%LOCALAPPDATA%\Makuris\logs
/// - 其他（Linux）：$XDG_STATE_HOME/makuris/logs
///
/// 不能三处都拼 "Library/Logs"：Windows 没有 HOME 变量（会退到不存在的 /tmp），
/// 也没有这种目录结构，日志会静默写不进去——排障时最需要日志的时候恰好没有。
fn default_log_dir() -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        std::path::PathBuf::from(home).join("Library/Logs/Makuris")
    }
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("LOCALAPPDATA")
            .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
        std::path::PathBuf::from(base).join("Makuris").join("logs")
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let base = std::env::var("XDG_STATE_HOME").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            format!("{home}/.local/state")
        });
        std::path::PathBuf::from(base).join("makuris").join("logs")
    }
}

/// 没设 MAKURIS_DEVICE_ID 时按平台给默认设备名。
///
/// 不能统一退回 "host-mac-01"：那样 Windows 机器会自称 mac，多机场景下
/// 还会与真正的 Mac 撞同一个 ID，设备列表里两台机器无法区分。
fn default_device_id() -> String {
    if cfg!(target_os = "macos") {
        "host-mac-01".to_string()
    } else if cfg!(target_os = "windows") {
        "host-win-01".to_string()
    } else {
        "host-linux-01".to_string()
    }
}

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use makuris_proto::{access, Role, SignalMsg};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> Result<()> {
    // 日志：同时写 stdout 和文件
    // （.app 双击运行时 stdout 不可见，文件日志用于排障；RUST_LOG 可覆盖级别）
    // 目录按平台惯例：macOS 是 ~/Library/Logs/Makuris，Windows 是
    // %LOCALAPPDATA%\Makuris\logs。不能都拼 Library/Logs——Windows 上既没有
    // HOME 变量（会退回 /tmp，那是个不存在的路径），也没有这种目录结构。
    use tracing_subscriber::prelude::*;
    let log_dir = std::env::var("MAKURIS_LOG_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| default_log_dir());
    let _ = std::fs::create_dir_all(&log_dir);
    let file_appender = tracing_appender::rolling::never(&log_dir, "host.log");
    let (non_blocking, _log_guard) = tracing_appender::non_blocking(file_appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
        .with(tracing_subscriber::fmt::layer().with_writer(non_blocking))
        .init();

    // WebRTC 启动自检：验证 H264 协商层在本机可用
    match stream::create_peer_connection(vec![], "640033") {
        Ok((mut pc, _)) => match stream::create_offer_sdp(&mut pc) {
            Ok(sdp) => tracing::info!("WebRTC self-test passed: H264 offer ({} bytes)", sdp.len()),
            Err(e) => tracing::warn!("WebRTC self-test: offer failed: {}", e),
        },
        Err(e) => tracing::warn!("WebRTC self-test: PC creation failed: {}", e),
    }

    // 辅助功能权限自检。
    //
    // 不查这一项，「鼠标完全不动」的反馈会变成无底洞：CGEvent 缺该权限时
    // 照样返回成功、不报错，事件被系统静默丢弃。而缺权限在改名或重签名后
    // 是常态（TCC 授权绑定 bundle id 与签名），且系统只对屏幕录制弹提示，
    // 辅助功能永远是静默拒绝——用户和开发者都得不到任何线索。
    #[cfg(target_os = "macos")]
    {
        if input::accessibility_granted() {
            tracing::info!("辅助功能权限已授予，输入注入可用");
        } else {
            tracing::warn!(
                "缺少「辅助功能」权限——鼠标/键盘事件会被系统静默丢弃（画面正常但操作无反应）。\
                 请到 系统设置 > 隐私与安全性 > 辅助功能 勾选 Makuris 后重启被控端"
            );
        }
    }

    // 设备 ID：优先取环境变量，没有时按平台给一个默认名。
    // 不能统一退回 "host-mac-01"：Windows 机器会自称 mac（实测日志里
    // Windows host 报的是 host-mac-01，而运行脚本明明设了 host-win-01），
    // 多机场景下更会与真正的 Mac 撞同一个 ID。
    let device_id = std::env::var("MAKURIS_DEVICE_ID").unwrap_or_else(|_| default_device_id());
    let signaling_url = std::env::var("MAKURIS_SIGNALING")
        .unwrap_or_else(|_| "wss://203.0.113.10:8443/ws".to_string());
    let udp_port: u16 = std::env::var("MAKURIS_UDP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(9001);

    tracing::info!("connecting to signaling {} as {}", signaling_url, device_id);

    // 打印配对码与查看路径。被控端常是双击启动的无界面 .app，用户没有别的
    // 途径知道该在手机上输什么，所以必须主动告知一次。
    pairing::announce();

    // 菜单栏助手：把配对码常驻到系统菜单栏（仅 .app bundle 内生效）。
    // 日志与文件是给开发者看的，菜单栏才是给坐在电脑前的用户看的。
    {
        let p = pairing::global().lock().unwrap();
        menubar::spawn(p.code(), &p.code_path());
    }

    // 配对码轮换的后台驱动：每 30 秒检查一次，随机码满 5 分钟自动换新
    // （逻辑与安全考量见 pairing::maintain）。env 固定码模式下空转即返回。
    tokio::spawn(async {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tick.tick().await;
            pairing::maintain();
        }
    });

    // 局域网直连 + Bonjour 广播：手机与电脑在同一网段时，打开 app 就能看到本机，
    // 不需要用户去查 IP、端口、路径——这些是实现细节，不该让人背。
    // 与中央信令服务器并行不冲突：一个管同网段零配置，一个管跨网络/公网。
    {
        let lan_device_id = device_id.clone();
        tokio::spawn(async move {
            if let Err(e) = lan::serve(lan_device_id, lan::resolve_port()).await {
                tracing::error!("[lan] 局域网直连服务退出: {}", e);
            }
        });
    }

    // 信令连接：失败不是致命错误，不能让它把进程带走。
    //
    // 局域网直连服务（lan::serve）已经在上面独立 spawn，手机与电脑同网段时
    // 完全不走中央信令服务器。若这里用 `?` 直接返回，整个进程会退出，
    // 连已经就绪的局域网直连服务一起拖死——表现就是「电脑上程序一闪而过，
    // 手机再也发现不了这台电脑」，而真正的原因跟局域网毫无关系。
    // 外层重连循环：信令断线只该让被控端「暂时不在公网在线列表里」，
    // 不该把整个进程带走。
    //
    // 这里踩过一个真实的坑：连接建立阶段的失败有重试（下面的内层 loop），
    // 但连上之后再断开就直接从 main 返回、进程退出——连带把已经就绪的
    // 局域网直连服务与 Bonjour 广播一起杀死。表现是「手机起初还能局域网
    // 直连，过一会儿整台电脑都找不到了」，而根因（信令重启、隧道抖动）
    // 跟局域网毫无关系，够排查半天。公网线路本来就会抖，所以断线必须重连。
    // wss 走指纹固定 TLS（自签证书，见 tls_pin.rs 的安全论证）；
    // 本地测试用 ws:// 地址仍走明文通道。
    loop {
    let (ws, _resp) = loop {
        match tls_pin::connect(&signaling_url).await {
            Ok(ok) => break ok,
            Err(e) => {
                tracing::warn!(
                    "信令服务器 {} 连接失败: {}；局域网直连不受影响，5 秒后重试",
                    signaling_url,
                    e
                );
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    };
    let (mut ws_sink, mut ws_stream) = ws.split();

    // 统一消息出口：业务逻辑经 channel 发送，转发任务写入 WebSocket
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // 注册为被控端。若信令服务器要求接入密钥，还要附上证明，否则会被拒。
    //
    // 密钥从环境变量读而不是写死在代码或随包分发：写死等于人人相同，
    // 等于没有；随包分发则一泄露全泄露。
    let reg = SignalMsg::Register {
        device_id: device_id.clone(),
        role: Role::Host,
        key_proof: access_key().map(|k| access::register_proof(&k, &device_id)),
    };
    tx.send(Message::Text(serde_json::to_string(&reg)?.into()))?;

    // 心跳保活
    {
        let hb = tx.clone();
        tokio::spawn(async move {
            let mut itv = tokio::time::interval(std::time::Duration::from_secs(25));
            loop {
                itv.tick().await;
                let ping = serde_json::to_string(&SignalMsg::Ping).unwrap();
                if hb.send(Message::Text(ping.into())).is_err() {
                    break;
                }
            }
        });
    }

    // 当前活跃会话的信令入口（收到 Connect 后建立）
    let mut session_event_tx: Option<mpsc::UnboundedSender<session::SessionEvent>> = None;

    // 信令接收循环
    while let Some(Ok(msg)) = ws_stream.next().await {
        let Message::Text(text) = msg else { continue };
        match serde_json::from_str::<SignalMsg>(&text) {
            Ok(SignalMsg::Registered { device_id }) => {
                tracing::info!("registered as {}", device_id);
            }
            Ok(SignalMsg::TurnCredentials { urls, realm, username, credential, ttl }) => {
                stream::set_turn_creds(stream::TurnCreds { urls, realm, username, credential });
                tracing::info!("[turn] 收到信令下发的时效 TURN 凭证（{} 秒后过期）", ttl);
            }
            Ok(SignalMsg::Pong) => {}
            Ok(SignalMsg::Connect { from, token, .. }) => {
                // 收到 Connect 不等于可以开始：先过准入。
                // 之前这里是无条件建会话，局域网里等于「谁连上谁就能控制」，
                // 暴露到公网后更等于把电脑交出去。
                match pairing::admit(&from, token.as_deref()) {
                    pairing::Admission::Granted => {
                        session_event_tx =
                            start_session(&tx, &device_id, &from, udp_port).await;
                    }
                    pairing::Admission::Challenge(nonce) => {
                        tracing::info!("[pairing] {} 尚未配对，发出挑战", from);
                        send_signal(
                            &tx,
                            &SignalMsg::AuthRequired {
                                from: device_id.clone(),
                                to: from.clone(),
                                nonce,
                            },
                        );
                    }
                    pairing::Admission::Denied(reason) => {
                        tracing::warn!("[pairing] 拒绝 {}：{}", from, reason);
                        send_signal(
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
            Ok(SignalMsg::AuthResponse { from, proof, .. }) => {
                match pairing::respond(&from, &proof) {
                    Ok(token) => {
                        tracing::info!("[pairing] {} 配对通过", from);
                        send_signal(
                            &tx,
                            &SignalMsg::AuthGranted {
                                from: device_id.clone(),
                                to: from.clone(),
                                token,
                            },
                        );
                        // 配对成功即开始会话：用户刚输完码，不该再要求他回列表点一次。
                        session_event_tx =
                            start_session(&tx, &device_id, &from, udp_port).await;
                    }
                    Err(reason) => {
                        send_signal(
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
            Ok(SignalMsg::Sdp { sdp_type, sdp, .. }) => {
                if let Some(etx) = &session_event_tx {
                    let _ = etx.send(session::SessionEvent::RemoteSdp { sdp_type, sdp });
                }
            }
            Ok(SignalMsg::Ice { candidate, sdp_mid, sdp_mline_index, .. }) => {
                if let Some(etx) = &session_event_tx {
                    let _ = etx.send(session::SessionEvent::RemoteIce {
                        candidate,
                        sdp_mid,
                        sdp_mline_index,
                    });
                }
            }
            Ok(other) => tracing::debug!("signal: {:?}", other),
            Err(e) => tracing::warn!("bad signaling message: {} ({})", e, text),
        }
    }

    tracing::warn!("信令连接断开，5 秒后重连（局域网直连不受影响）");
        // 断开时把会话句柄放掉：它一被释放，正在跑的会话就收到 None 而收尾。
        // 不放的话，重连后新会话会与这个「已经没人关注的会话」并存，
        // 又回到两套采集/编码抢同一块屏幕的状态。
        if let Some(tx) = session_event_tx.take() {
            drop(tx);
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// 信令服务器要求的接入密钥（没配就返回 None，走开放模式）。
///
/// 只在环境变量里取，不做任何默认值：一旦代码里藏一个「默认密钥」，它就会
/// 随二进制一起分发出去，等于所有部署共用同一把钥匙，比不设防还给用户错觉。
/// 密文本就该由部署者自己指定——这与被控端的配对码不同，配对码是给用户
/// 抄写的，需要人可读；接入密钥是机器对机器的，可以是一长串十六进制。
fn access_key() -> Option<String> {
    if let Ok(k) = std::env::var("MAKURIS_ACCESS_KEY") {
        let k = k.trim().to_owned();
        if !k.is_empty() {
            return Some(k);
        }
    }
    // 回落读文件：macOS 上的被控端是以 .app 形式由 launchd 拉起的，不继承终端
    // 里的环境变量。只认环境变量的话，从 Finder 双击启动就永远配不上密钥，
    // 而信令服务器一旦启用鉴权会直接拒绝它的注册——现象是「手机看不到这台
    // 电脑」，跟密钥没有任何字面关联，够排查半天。写成文件后部署者只需配一次。
    std::fs::read_to_string(pairing::state_dir().join(ACCESS_KEY_FILE))
        .ok()
        .map(|k| k.trim().to_owned())
        .filter(|k| !k.is_empty())
}

/// 接入密钥的落盘文件名（与 signaling-server 各自独立存放，值需一致）。
const ACCESS_KEY_FILE: &str = "access-key.txt";

/// 向信令通道发一条消息，失败只记日志。
fn send_signal(tx: &mpsc::UnboundedSender<Message>, msg: &SignalMsg) {
    if let Ok(json) = serde_json::to_string(msg) {
        let _ = tx.send(Message::Text(json.into()));
    }
}

/// 会话建立已下沉到 `session::start_session`，中央信令与局域网直连两条路径
/// 共用同一份实现。建会话本身是个安全动作（开始把屏幕推给对端），两份实现
/// 迟早会各自漂移，任何一处漏掉校验都等于开了后门。
///
/// 它是 async 的：建会话前要先停掉正在跑的旧会话并等它把采集/编码资源放干净，
/// 而那段等待必须发生在「发 offer 之前」——否则新旧两套流水线会同时抢屏幕。
async fn start_session(
    tx: &mpsc::UnboundedSender<Message>,
    host_id: &str,
    viewer_id: &str,
    udp_port: u16,
) -> Option<mpsc::UnboundedSender<session::SessionEvent>> {
    session::start_session(tx, host_id, viewer_id, udp_port).await
}
