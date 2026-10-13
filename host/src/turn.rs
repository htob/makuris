//! host 侧 TURN 客户端（路线 A：手写 sans-IO TURN）
//!
//! 背景（已定案）：
//! - rtc-ice 0.21 无内置候选收集器，AgentConfig.urls 只存不用，host 候选全靠
//!   自研 stun.rs 收集 + add_local_candidate 注入。
//! - iOS 侧 WebRTC.xcframework 的 ObjC→C++ 凭证传递在两种初始化器下均丢失，
//!   iPhone 永远发不出带凭证的 Allocate（抓包定案：入包恒为 20/28 字节裸请求）。
//! - 故 4G 中继只能由 host 侧（Rust，自己可控）完成 TURN 认证：host Allocate
//!   拿到 relay 地址，作为本端 relay 候选 trickle 给 iPhone；iPhone 连这个
//!   relay（普通 UDP，实测可达），媒体经 TURN 服务器中转。
//!
//! 本模块用 rtc-turn 0.21 的 sans-IO API 实现（用法对照官方 examples/turn_client_udp.rs）：
//! - connect(): Allocate 握手（401 长期凭证重试由 rtc-turn 内部处理）
//! - create_permission(): 为 iPhone 公网地址建 permission
//! - send_to_peer() / recv_from_peer(): 媒体双向转发（Send/Data indication）
//! - 后台泵送 task: 持续驱动 socket ↔ client，处理重传/超时/Refresh 保活
//!
//! sans-IO 泵送模型（方法来自 sansio::Endpoint trait，须 `use sansio::Endpoint` 引入）：
//! - poll_write() -> Option<TransportMessage<BytesMut>>：取出待发包，发到 .transport.peer_addr
//! - handle_read(TransportMessage<BytesMut>)：把收到的包喂给 client
//! - poll_timeout() / handle_timeout(Instant)：驱动事务重传与超时
//! - poll_event() -> Option<Event>：取出 AllocateResponse / DataIndication 等事件

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use bytes::BytesMut;
use rtc_shared::{TransportContext, TransportMessage, TransportProtocol};
use rtc_turn::client::{Client, ClientConfig, Event, RelayedAddr};
use sansio::Protocol;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// TURN 服务器配置（与 iOS 侧同一套长期凭证）
pub struct TurnConfig {
    /// TURN 服务器地址，如 "203.0.113.10:3478"
    pub turn_addr: String,
    pub username: String,
    pub password: String,
    pub realm: String,
}

/// 因本地 permission 未就绪而暂存的中继包（等权限建好后重发）。
struct PendingSend {
    peer: SocketAddr,
    data: Vec<u8>,
    queued_at: Instant,
}

/// 待发队列上限：防止对端持续不可达时无限堆积内存。
const MAX_PENDING_SENDS: usize = 512;
/// 中继包最长等待时间：媒体包超时即失去意义，超时直接丢弃。
const PENDING_SEND_TTL: Duration = Duration::from_secs(8);

/// 后台泵送 task 的命令
enum Cmd {
    Send { peer: SocketAddr, data: Vec<u8> },
    Permission { peer: SocketAddr },
    Refresh,
}

/// 已建立的 TURN 分配。持有后台泵送 task 的通信端点。
pub struct TurnAllocation {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    data_rx: mpsc::UnboundedReceiver<(SocketAddr, BytesMut)>,
    /// TURN 服务器分配的中继地址（作为 host 的 relay 候选 trickle 给 iPhone）
    pub relay_addr: RelayedAddr,
}

/// 构造 rtc-turn 所需的 crypto provider（ring 后端）。
fn crypto_provider() -> Result<Arc<dyn rtc_turn::crypto::RTCCryptoProvider>> {
    rtc_turn::crypto::default_provider().context("创建 crypto provider 失败")
}

impl TurnAllocation {
    /// 向 TURN 服务器 Allocate，泵送 sans-IO 事务直到拿到 relay 地址，
    /// 然后启动后台泵送 task 维持分配与转发。
    pub async fn connect(cfg: TurnConfig) -> Result<Self> {
        let server_addr: SocketAddr = cfg
            .turn_addr
            .parse()
            .with_context(|| format!("TURN 地址解析失败: {}", cfg.turn_addr))?;

        // 绑定本地 UDP socket（与 TURN 服务器通信用；端口任意，NAT 会映射）
        let socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .context("绑定本地 socket 失败")?;
        let local_addr = socket.local_addr()?;
        info!("[turn] 本地 socket {} → TURN 服务器 {}", local_addr, server_addr);

        let client_cfg = ClientConfig {
            stun_serv_addr: String::new(), // 不用 STUN，只走 TURN
            turn_serv_addr: cfg.turn_addr.clone(),
            local_addr,
            transport_protocol: TransportProtocol::UDP,
            username: cfg.username,
            password: cfg.password,
            realm: cfg.realm,
            software: String::new(),
            rto_in_ms: 200,
            allocation_refresh_interval_cap: None,
        };

        let mut client =
            Client::new(client_cfg, crypto_provider()?).context("创建 TURN client 失败")?;

        // 发起 Allocate（rtc-turn 内部处理 401 挑战→带凭证重试）
        let now = Instant::now();
        client.allocate(now).context("发起 Allocate 失败")?;

        // 同步泵送直到 AllocateResponse
        let relay_addr = Self::pump_allocate(&mut client, &socket, local_addr).await?;
        info!("[turn] Allocate 成功，relay 地址 {}", relay_addr);

        // 启动后台泵送 task
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (data_tx, data_rx) = mpsc::unbounded_channel();
        let socket = Arc::new(socket);
        tokio::spawn(Self::pump_loop(
            client, socket, relay_addr, local_addr, cmd_rx, data_tx,
        ));

        Ok(Self { cmd_tx, data_rx, relay_addr })
    }

    /// connect 阶段的同步泵送：取发包→sendto，recvfrom→喂收包，直到 Allocate 完成。
    async fn pump_allocate(
        client: &mut Client,
        socket: &UdpSocket,
        local_addr: SocketAddr,
    ) -> Result<RelayedAddr> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut buf = [0u8; 2048];
        loop {
            if Instant::now() > deadline {
                return Err(anyhow!("TURN Allocate 超时（10s）"));
            }
            let now = Instant::now();

            // 1. 取出待发包，发到 TURN 服务器
            while let Some(pkt) = client.poll_write() {
                socket.send_to(&pkt.message, pkt.transport.peer_addr).await?;
            }

            // 2. 处理事务超时/重传
            client.handle_timeout(now)?;

            // 3. 收包（短超时，避免卡死泵送循环）
            match tokio::time::timeout(Duration::from_millis(50), socket.recv_from(&mut buf)).await
            {
                Ok(Ok((n, from))) => {
                    client.handle_read(TransportMessage {
                        now: Instant::now(),
                        transport: TransportContext {
                            local_addr,
                            peer_addr: from,
                            transport_protocol: TransportProtocol::UDP,
                            ecn: None,
                        },
                        message: BytesMut::from(&buf[..n]),
                    })?;
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {} // 超时，继续泵送
            }

            // 4. 取事件
            while let Some(ev) = client.poll_event() {
                match ev {
                    Event::AllocateResponse(_, addr) => return Ok(addr),
                    Event::AllocateError(_, e) => {
                        return Err(anyhow!("TURN Allocate 失败: {:?}", e));
                    }
                    _ => {}
                }
            }
        }
    }

    /// 后台泵送循环：持续驱动 socket ↔ client，处理命令、收包、事件、Refresh 保活。
    async fn pump_loop(
        mut client: Client,
        socket: Arc<UdpSocket>,
        relay_addr: RelayedAddr,
        local_addr: SocketAddr,
        mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
        data_tx: mpsc::UnboundedSender<(SocketAddr, BytesMut)>,
    ) {
        let mut buf = [0u8; 2048];
        let mut last_refresh = Instant::now();
        // 分配默认 lifetime 10 分钟，5 分钟 Refresh 一次留足余量
        let refresh_interval = Duration::from_secs(300);
        // 因本地缺 permission 而发不出去的中继包：先补建权限、入队，下一轮重发。
        //
        // rtc-turn 的 send_to() 是「本地先查 perm_map，没有 Permitted 条目就直接返回
        // ErrNoPermission」，包连网卡都出不去。而 perm 条目可能被 rtc-turn 就地删除
        // （收到任何 CreatePermission 错误响应时它都会 perm_map.remove），此后该 peer
        // 永久发不出包 —— 实测 4G 中继下 Windows→iPhone 方向 0 包、会话 Failed 即此因。
        let mut pending_sends: Vec<PendingSend> = Vec::new();

        loop {
            let now = Instant::now();

            // 处理主线程命令（send / permission / refresh）
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    Cmd::Send { peer, data } => {
                        let mut sent = false;
                        if let Ok(mut r) = client.relay(relay_addr) {
                            match r.send_to(now, &data, peer) {
                                Ok(()) => sent = true,
                                Err(e) => warn!(
                                    "[turn] send_to 失败: {:?} peer={}，补建 permission 后排队重发",
                                    e, peer
                                ),
                            }
                        }
                        if !sent {
                            // 无条件补一次 CreatePermission：perm 丢了就重建；若仍处
                            // Pending 则是幂等空转（rtc-turn 内部按地址去重，不会重发请求）。
                            if let Ok(mut r) = client.relay(relay_addr) {
                                if let Err(e) = r.create_permission(now, peer) {
                                    warn!("[turn] 补建 permission 失败: {:?} peer={}", e, peer);
                                }
                            }
                            if pending_sends.len() < MAX_PENDING_SENDS {
                                pending_sends.push(PendingSend { peer, data, queued_at: now });
                            } else {
                                warn!("[turn] 待发队列已满，丢弃发往 {} 的中继包", peer);
                            }
                        }
                    }
                    Cmd::Permission { peer } => {
                        // TURN 的 CreatePermission 要求 XOR-PEER-ADDRESS 与 relay 地址同族，
                        // 异族请求服务端必定回 443 Peer Address Family Mismatch；而 rtc-turn
                        // 收到错误响应会把该 peer 从 perm_map 里删掉，反把建权状态搅乱（实测
                        // 11 次 443 紧邻 10 次 send_to 失败）。iPhone 在蜂窝下会同时广播 IPv4
                        // srflx 与 IPv6 候选，故必须在本地就按地址族挡掉，只建可能生效的一族。
                        if peer.is_ipv4() != relay_addr.is_ipv4() {
                            info!(
                                "[turn] 跳过异地址族建权: peer={} relay={}",
                                peer, relay_addr
                            );
                        } else if let Ok(mut r) = client.relay(relay_addr) {
                            if let Err(e) = r.create_permission(now, peer) {
                                warn!("[turn] create_permission 失败: {:?} peer={}", e, peer);
                            }
                        }
                    }
                    Cmd::Refresh => {
                        if let Err(e) = client.refresh_allocations(now) {
                            warn!("[turn] refresh 失败: {:?}", e);
                        }
                    }
                }
            }

            // 权限就绪后补发排队的中继包；超时仍未就绪则丢弃（防队列无限增长）
            if !pending_sends.is_empty() {
                let mut keep: Vec<PendingSend> = Vec::with_capacity(pending_sends.len());
                for ps in pending_sends.drain(..) {
                    if now.duration_since(ps.queued_at) > PENDING_SEND_TTL {
                        warn!("[turn] 待发包超时丢弃: peer={}", ps.peer);
                        continue;
                    }
                    match client.relay(relay_addr) {
                        Ok(mut r) => match r.send_to(now, &ps.data, ps.peer) {
                            Ok(()) => info!("[turn] 待发包补发成功: peer={}", ps.peer),
                            Err(_) => keep.push(ps),
                        },
                        Err(_) => keep.push(ps),
                    }
                }
                pending_sends = keep;
            }

            // 定期 Refresh 保活
            if last_refresh.elapsed() >= refresh_interval {
                if let Err(e) = client.refresh_allocations(now) {
                    warn!("[turn] 定期 Refresh 失败: {:?}", e);
                }
                last_refresh = now;
            }

            // 取发包 → 发到 TURN 服务器
            while let Some(pkt) = client.poll_write() {
                if let Err(e) = socket.send_to(&pkt.message, pkt.transport.peer_addr).await {
                    warn!("[turn] 发包失败: {}", e);
                }
            }

            // 超时/重传处理
            if let Err(e) = client.handle_timeout(now) {
                warn!("[turn] handle_timeout 失败: {:?}", e);
            }

            // 收包 → 喂给 client
            match tokio::time::timeout(Duration::from_millis(50), socket.recv_from(&mut buf)).await
            {
                Ok(Ok((n, from))) => {
                    let msg = TransportMessage {
                        now: Instant::now(),
                        transport: TransportContext {
                            local_addr,
                            peer_addr: from,
                            transport_protocol: TransportProtocol::UDP,
                            ecn: None,
                        },
                        message: BytesMut::from(&buf[..n]),
                    };
                    if let Err(e) = client.handle_read(msg) {
                        warn!("[turn] handle_read 失败: {:?}", e);
                    }
                }
                Ok(Err(e)) => {
                    error!("[turn] socket 收包错误: {}，退出泵送", e);
                    break;
                }
                Err(_) => {} // 超时，继续
            }

            // 事件 → 数据通道（peer 经 relay 发来的媒体/信令）
            while let Some(ev) = client.poll_event() {
                match ev {
                    Event::DataIndicationOrChannelData(_, peer, data) => {
                        if data_tx.send((peer, data)).is_err() {
                            info!("[turn] 数据接收端已关闭，退出泵送");
                            return;
                        }
                    }
                    Event::AllocateError(_, e) => {
                        error!("[turn] 分配错误（可能已失效）: {:?}", e);
                    }
                    Event::CreatePermissionError(_, e) => {
                        warn!("[turn] CreatePermission 失败: {:?}", e);
                    }
                    _ => {}
                }
            }
        }
    }

    /// 为 peer（iPhone 公网地址）创建 permission，允许其经 relay 收发。
    pub fn create_permission(&self, peer: SocketAddr) {
        let _ = self.cmd_tx.send(Cmd::Permission { peer });
    }

    /// 把数据经 TURN relay 转发给 peer（iPhone）。
    pub fn send_to_peer(&self, peer: SocketAddr, data: Vec<u8>) {
        let _ = self.cmd_tx.send(Cmd::Send { peer, data });
    }

    /// 接收 peer（iPhone）经 relay 发来的数据。
    pub async fn recv_from_peer(&mut self) -> Option<(SocketAddr, BytesMut)> {
        self.data_rx.recv().await
    }

    /// 手动触发 Refresh（通常由后台 task 自动处理）。
    pub fn refresh(&self) {
        let _ = self.cmd_tx.send(Cmd::Refresh);
    }
}


/// 供 tokio::select! 使用：有分配时接收 relay 数据，无分配时永久 pending（不触发）。
pub async fn recv_or_pending(alloc: Option<&mut TurnAllocation>) -> Option<(SocketAddr, BytesMut)> {
    match alloc {
        Some(a) => a.recv_from_peer().await,
        None => std::future::pending().await,
    }
}