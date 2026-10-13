//! Makuris 信令服务器。
//! 职责：设备注册、设备列表查询、viewer 与 host 之间的 SDP/ICE 转发、心跳维持。
//! 不接触媒体内容（零知识），真实音视频走 WebRTC P2P 或 TURN 中继。
//!
//! 接入鉴权（本次加上的）：
//!
//! 原来这台服务器对谁都不设防，等于三个洞摞在一起：
//!   1. 任何人连上端口就能发 ListDevices，把部署者家里所有在线电脑列出来。
//!      设备列表本身就是隐私——设备名常是「书房台式机」这种能定位到房间的信息。
//!   2. 任何人可以用别人的 device_id 注册。服务器按 device_id 路由，
//!      先到先得，于是攻击者能把本该发给那台电脑的信令整个劫走。
//!   3. 任何人都能伪造 from 字段往任意设备投信令，去骚扰或试探别人。
//!
//! 现在要求注册时附 `key_proof`（HMAC-SHA256(接入密钥, "register|" + device_id)，
//! 算法在 `makuris_proto::access`）。用 HMAC 而不是直接比对密钥，是为了让密钥
//! 本身不上链路——否则一台设备被嗅探，整个部署都失守。
//!
//! 「没配密钥」时保留开放模式：局域网里跑一台自己用，逼用户配密钥没有意义，
//! 反而会让人干脆不用。会在启动日志里明确写出「当前为开放模式」。
//!
//! 注意这层与设备配对是两件事：这里管「能不能用信令服务器转发消息」，
//! 被控端的 `pairing` 管「能不能操作那台电脑」。前者是门口，后者是房门。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use makuris_proto::{access, DeviceInfo, Role, SignalMsg};
use tokio::sync::mpsc;

type Tx = mpsc::UnboundedSender<Message>;

#[derive(Clone)]
struct AppState {
    /// device_id -> (该连接的消息发送端, 角色, 被控端上报的可读名字)
    ///
    /// 名字必须存进表里而不是注册时转发一次就走：viewer 是随时来问设备列表的，
    /// 到那时得能答出每台电脑叫什么。
    peers: Arc<DashMap<String, (Tx, Role, Option<String>)>>,
    /// 接入密钥。None 表示开放模式（局域网自用场景）。
    access_key: Option<Arc<String>>,
    /// TURN 时效凭证签发器。None = 未配 MAKURIS_TURN_SECRET，不下发
    /// （host 拿不到凭证时自动退化为仅直连/局域网，见 stream::host_turn_config）。
    turn: Option<Arc<TurnIssuer>>,
}

/// TURN 时效凭证签发器（coturn `use-auth-secret`，REST 风格）。
///
/// username = "<unix 过期时间戳>:<设备id>"，
/// credential = Base64(HMAC-SHA1(共享密钥, username))。
/// coturn 校验时从 username 解析过期时间、用共享密钥重算 HMAC 比对——
/// 过期即自动失效，无需任何吊销动作。
///
/// 为什么由服务器签发而不是写死在客户端源码：长期凭证入源码 = 开源即泄露
/// （docs/OPEN_SOURCE_AUDIT.md P0）。时效凭证即使被截获，几小时后作废，
/// 且 username 绑定设备 id，coturn 日志还能追溯是谁在用中继。
/// 共享密钥只存在于服务器 EnvironmentFile（MAKURIS_TURN_SECRET），永不入库。
struct TurnIssuer {
    urls: Vec<String>,
    realm: String,
    secret: String,
    ttl: u64,
}

impl TurnIssuer {
    /// 从环境变量构建；未配 MAKURIS_TURN_SECRET 返回 None（不启用下发）。
    fn from_env() -> Option<Self> {
        let secret = std::env::var("MAKURIS_TURN_SECRET")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())?;
        // 地址与 realm 必须与云端 coturn 实际配置一致（deploy-signaling.sh 生成 env 时同源写入）
        let urls: Vec<String> = std::env::var("MAKURIS_TURN_URLS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        if urls.is_empty() {
            tracing::warn!("MAKURIS_TURN_SECRET 已配但 MAKURIS_TURN_URLS 为空，跳过 TURN 凭证下发");
            return None;
        }
        let realm = std::env::var("MAKURIS_TURN_REALM").unwrap_or_default();
        let ttl = std::env::var("MAKURIS_TURN_TTL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3600);
        Some(Self { urls, realm, secret, ttl })
    }

    /// 为某设备签发一份当前有效的凭证消息。
    fn issue(&self, device_id: &str) -> Option<SignalMsg> {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        use hmac::{Hmac, Mac};
        use sha1::Sha1;

        let expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs()
            + self.ttl;
        let username = format!("{expiry}:{device_id}");
        let mut mac = Hmac::<Sha1>::new_from_slice(self.secret.as_bytes()).ok()?;
        mac.update(username.as_bytes());
        let credential = STANDARD.encode(mac.finalize().into_bytes());
        Some(SignalMsg::TurnCredentials {
            urls: self.urls.clone(),
            realm: self.realm.clone(),
            username,
            credential,
            ttl: self.ttl,
        })
    }
}

/// 接入密钥的存放位置：优先环境变量，其次数据目录里的文件（首次运行自动生成）。
///
/// 之所以要落盘：密钥若每次重启都变，那所有设备的配置都会失效，用户会被逼回
/// 「关掉鉴权」这条路——安全措施只要让人觉得麻烦，最后一定会被绕过。
fn load_or_create_key() -> Option<String> {
    if let Ok(k) = std::env::var("MAKURIS_ACCESS_KEY") {
        let k = k.trim().to_owned();
        if !k.is_empty() {
            return Some(k);
        }
    }
    // 显式声明开放模式：给自动化测试和「就想在家用一下」的场景留出口
    if std::env::var("MAKURIS_NO_ACCESS_KEY").is_ok() {
        return None;
    }

    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("access-key.txt");
    if let Ok(v) = std::fs::read_to_string(&path) {
        let v = v.trim().to_owned();
        if !v.is_empty() {
            return Some(v);
        }
    }
    let fresh = access::generate_key();
    if let Err(e) = std::fs::write(&path, &fresh) {
        tracing::warn!("接入密钥写入失败（这次仍会用新密钥，重启后需重新配置）: {}", e);
    }
    Some(fresh)
}

fn state_dir() -> PathBuf {
    // 与 host 侧同理：MAKURIS_STATE_DIR 优先，供测试隔离。
    // 服务器会在这里存接入密钥，而默认目录是用户日常那一个——测试写进去的
    // 密钥会顶掉用户已分发给各设备的密钥，所有设备突然都注册不上。
    if let Ok(d) = std::env::var("MAKURIS_STATE_DIR") {
        if !d.trim().is_empty() {
            return PathBuf::from(d.trim());
        }
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join("Library/Application Support/Makuris/signaling")
    }
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("LOCALAPPDATA")
            .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
        PathBuf::from(base).join("Makuris").join("signaling")
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let base = std::env::var("XDG_STATE_HOME").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            format!("{home}/.local/state")
        });
        PathBuf::from(base).join("makuris").join("signaling")
    }
}

async fn health() -> &'static str {
    "ok"
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    // 把通道里的消息转发到 WebSocket
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    let mut device_id: Option<String> = None;

    while let Some(Ok(msg)) = ws_receiver.next().await {
        let Message::Text(text) = msg else { continue };
        let Ok(signal) = serde_json::from_str::<SignalMsg>(&text) else {
            continue;
        };
        match signal {
            SignalMsg::Register { device_id: id, role, key_proof, name } => {
                // 先验接入证明，再谈注册。顺序不能反——注册动作本身会往
                // peers 表里写东西，验过了才写，免得给未授权连接留下痕迹。
                if let Some(key) = &state.access_key {
                    let ok = key_proof
                        .as_deref()
                        .map(|p| access::verify_register(key, &id, p))
                        .unwrap_or(false);
                    if !ok {
                        tracing::warn!("注册被拒（接入密钥不符）: {}", id);
                        let err = serde_json::to_string(&SignalMsg::Error {
                            message: "接入密钥不正确，请在被控端与 App 里填同一串密钥".into(),
                        })
                        .unwrap();
                        let _ = tx.send(Message::Text(err));
                        continue;
                    }
                }

                // 同一 device_id 已在线时覆盖旧的：host 重启后旧连接可能还没
                // 被回收，若不覆盖，新的注册会被挤掉、这台电脑一直显示离线。
                // 覆盖是安全的——前提是已有证明通过，否则上面就 continue 了。
                if let Some(old) = state.peers.insert(id.clone(), (tx.clone(), role, name)) {
                    tracing::warn!("device_id {} 重复注册，已用新连接替换旧连接", id);
                    drop(old);
                }
                tracing::info!("device registered: {} ({:?})", id, role);
                device_id = Some(id.clone());
                let ack = serde_json::to_string(&SignalMsg::Registered { device_id: id.clone() }).unwrap();
                let _ = tx.send(Message::Text(ack));
                // 注册成功后紧接着下发时效 TURN 凭证（配了 MAKURIS_TURN_SECRET 才发）。
                // 凭证不走源码而是运行时下发，源码零秘密；username 内嵌过期时间，
                // 到期由 coturn 自动拒绝，无需吊销。
                //
                // 但「设备重连即重新签发」绝不等于刷新机制：被控端常年在线、从不重连，
                // 只在注册那一刻拿到一份凭证，ttl 一到就永久 401。2026-10-12 实测踩过：
                // Windows 被控端 18:06 领证、20:06 过期、20:07 起连续 237 次 401，iPhone
                // 走蜂窝再也连不上，而 Mac 因重连过 3 次反而「看起来正常」。所以这里必须
                // 再挂一个续发循环，理由与节奏见 turn_refresh_loop。
                if let Some(issuer) = &state.turn {
                    if let Some(msg) = issuer.issue(&id) {
                        let _ = tx.send(Message::Text(serde_json::to_string(&msg).unwrap()));
                    }
                    // 每条连接各挂一个续发任务：tx 是本连接专属的发送端，连接一断
                    // send 就失败、任务自行退出，不会随注册次数累积。
                    tokio::spawn(turn_refresh_loop(state.clone(), id.clone(), tx.clone()));
                }
            }
            SignalMsg::Ping => {
                let pong = serde_json::to_string(&SignalMsg::Pong).unwrap();
                let _ = tx.send(Message::Text(pong));
            }
            SignalMsg::ListDevices => {
                // 未注册就查列表：直接不理。否则「列出在线设备」这个动作
                // 不需要任何凭据就能做，等于把部署者的设备清单公开出去。
                if device_id.is_none() {
                    tracing::warn!("未注册的连接请求设备列表，已忽略");
                    continue;
                }
                // 返回所有在线的 Host 设备列表
                let devices: Vec<DeviceInfo> = state
                    .peers
                    .iter()
                    .filter(|entry| entry.value().1 == Role::Host)
                    .map(|entry| DeviceInfo {
                        device_id: entry.key().clone(),
                        role: Role::Host,
                        platform: None,
                        // 名字随注册存在 peers 表里，这里原样带出。
                        name: entry.value().2.clone(),
                    })
                    .collect();
                let msg = serde_json::to_string(&SignalMsg::DeviceList { devices }).unwrap();
                let _ = tx.send(Message::Text(msg));
            }
            other => {
                // SDP / ICE / Connect / 配对消息：按目标设备转发
                //
                // 同样要求已注册。否则一个未鉴权的连接可以伪造 from 往任意
                // 设备投信令（骚扰、试探，或诱导对端把该连接当作可信对端）。
                if device_id.is_none() {
                    tracing::warn!("未注册的连接尝试转发信令，已忽略");
                    continue;
                }
                if let Some(to) = other.target() {
                    if let Some(peer) = state.peers.get(to) {
                        let _ = peer.0.send(Message::Text(serde_json::to_string(&other).unwrap()));
                    } else {
                        tracing::warn!("target peer offline: {}", to);
                    }
                }
            }
        }
    }

    if let Some(id) = device_id {
        state.peers.remove(&id);
        tracing::info!("device disconnected: {}", id);
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let access_key = load_or_create_key();
    match &access_key {
        Some(k) => tracing::info!(
            "接入鉴权已启用；接入密钥 {}（设备需填同一串；可用 MAKURIS_ACCESS_KEY 指定）",
            k
        ),
        None => tracing::warn!(
            "接入鉴权已关闭（MAKURIS_NO_ACCESS_KEY）：任何人连上本端口都能看到在线设备列表。\
             仅建议在纯局域网自用时这样跑"
        ),
    }

    let turn = TurnIssuer::from_env();
    match &turn {
        Some(_) => tracing::info!("TURN 时效凭证下发已启用（注册成功后随 Registered 下发）"),
        None => tracing::warn!(
            "未配 MAKURIS_TURN_SECRET：不下发 TURN 凭证，公网跨 NAT 场景将只有直连候选（host 自动退化为仅直连）"
        ),
    }

    let state = AppState {
        peers: Arc::new(DashMap::new()),
        access_key: access_key.map(Arc::new),
        turn: turn.map(Arc::new),
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/ws", get(ws_handler))
        .with_state(state);

    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8443);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    // TLS：证书就位则服务 wss，缺失回落明文 ws（仅供本地开发/测试）。
    // 为什么自签而不是 Let's Encrypt：这台部署没有域名，ACME 签发要域名验证。
    // 自签的信任面反而更小——客户端不信任任何 CA，只认 pin 死的证书 SHA256
    // 指纹（见 host/iOS 侧），中间人拿假证书也过不了指纹校验。
    let cert = std::env::var("MAKURIS_TLS_CERT").unwrap_or_else(|_| "/etc/makuris/tls/cert.pem".into());
    let key = std::env::var("MAKURIS_TLS_KEY").unwrap_or_else(|_| "/etc/makuris/tls/key.pem".into());

    if std::path::Path::new(&cert).is_file() && std::path::Path::new(&key).is_file() {
        tracing::info!("Makuris signaling listening on wss://{}:{}/ws", addr.ip(), port);
        let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key).await?;
        axum_server::bind_rustls(addr, rustls_config)
            .serve(app.into_make_service())
            .await?;
    } else {
        tracing::warn!(
            "TLS 证书/私钥缺失（{cert} / {key}），以明文 ws 启动——公网部署强烈不建议，\
             配对流量可被嗅探离线暴破"
        );
        tracing::info!("Makuris signaling listening on ws://{}:{}/ws", addr.ip(), port);
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
    }
    Ok(())
}

/// 周期性给某台设备续发 TURN 时效凭证。
///
/// 为什么必须有这个循环：凭证是 ttl 秒过期的 HMAC 时效串，而 host 只在「注册成功」
/// 那一刻收到一份。被控端常年在线、不重连的话，凭证一到期，TURN Refresh 就永久 401，
/// 中继通道彻底不可用——这正是 2026-10-12 实测到的故障：Windows 被控端 18:06:25 拿到
/// 凭证，20:06 过期，20:07:11 起连续 237 次 `Refresh error response (error 401)`，
/// iPhone 走蜂窝再也连不上；同一时刻 Mac 却「能连」，只是因为它重连过 3 次、每次重连
/// 都重新领了一份新凭证。所以「重启一下就好了」是假象，不修续发机制必然复发。
///
/// 为什么只改信令、host 与 iOS 一行都不用动：host 侧 TurnAllocation 是会话级新建
/// （session.rs 里每次会话现场 `host_turn_config()` 从全局 TURN_CREDS 取当时最新值），
/// 收到新凭证只是覆盖写那份 RwLock，下一个会话自然用新凭证。
///
/// 节奏取 ttl/3（下限 60 秒）：保证任何时刻手上的凭证都还有至少 2/3 寿命，容忍一次
/// 推送丢失；同时避免每分钟都签发的无谓开销。
///
/// 退出条件：send 失败即代表这条 WebSocket 已经关掉（设备下线，或已被同 device_id
/// 的新连接顶替），立刻结束循环。不这么做的话，每次注册都会攒下一个空转任务。
async fn turn_refresh_loop(state: AppState, device_id: String, tx: Tx) {
    let issuer = match state.turn.clone() {
        Some(i) => i,
        // 未配 MAKURIS_TURN_SECRET 时本来就不下发凭证，没有可续的东西。
        None => return,
    };
    let period = std::time::Duration::from_secs((issuer.ttl / 3).max(60));
    let mut tick = tokio::time::interval(period);
    // 用 Delay 而非 Burst：进程繁忙时宁可顺延，也不要积压出一串无意义的连续签发。
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 第一个 tick 是立即触发的，而注册分支刚刚同步下发过一份，跳过它避免重复推送。
    tick.tick().await;
    loop {
        tick.tick().await;
        let msg = match issuer.issue(&device_id) {
            Some(m) => m,
            None => break,
        };
        match serde_json::to_string(&msg) {
            Ok(text) => {
                if tx.send(Message::Text(text)).is_err() {
                    tracing::debug!("TURN 凭证续发已停止（连接已关闭）: {}", device_id);
                    break;
                }
                tracing::debug!("已续发 TURN 时效凭证: {}", device_id);
            }
            // 序列化失败属于编程错误，重试也不会好，退出而不是空转。
            Err(e) => {
                tracing::warn!("TURN 凭证序列化失败，停止续发 {}: {e}", device_id);
                break;
            }
        }
    }
}
