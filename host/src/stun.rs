//! STUN 收集：问出本机在 NAT 上的公网映射地址，作为 srflx 候选。
//!
//! 为什么要自己实现：本项目用的 rtc 0.21 / rtc-ice 0.21 **没有内置候选收集器**。
//! rtc-ice 的 `gather_candidates` 在源码里至今是整段被注释掉的 TODO；Agent 对外
//! 只提供 `add_local_candidate`/`add_remote_candidate`。`AgentConfig.urls` 虽然
//! 收下了 ice_servers 里的地址，但检索全库可见它只是被存下来、没有任何代码拿它
//! 去发 Binding 请求。结果是 host 的 offer 里永远只有一条 host 候选——同网段能用
//! （同网段直连不需要 NAT 穿越），一旦跨网络就必然失败。
//!
//! 实现上有一处关键约束：**必须在会话实际使用的那个 UDP socket 上发请求**。
//! NAT 给每个内网 ip:port 单独分配映射，换一个 socket 去问，拿到的端口与候选里
//! 写的端口对不上，对端往那个端口发包会被 NAT 直接丢掉——表现是打洞「成功」
//! 却始终连不上，比单纯失败更难查。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::net::UdpSocket;

/// STUN 消息头固定 20 字节：类型(2) + 长度(2) + 魔术字(4) + 事务 ID(12)
const HEADER: usize = 20;
/// Binding 请求与成功响应（RFC 8489 §5）
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_RESPONSE: u16 = 0x0101;
/// 固定魔术字：既用于识别响应属于 STUN，也是 XOR-MAPPED-ADDRESS 的异或密钥
const MAGIC: u32 = 0x2112_A442;
/// MAPPED-ADDRESS 与 XOR-MAPPED-ADDRESS。后者是标准做法，前者是给不支持的实现兜底
const ATTR_MAPPED: u16 = 0x0001;
const ATTR_XOR_MAPPED: u16 = 0x0020;
/// 单个服务器的等待上限。会话建立要等这一步，不宜设长
const QUERY_TIMEOUT: Duration = Duration::from_millis(800);

/// 默认 STUN 服务器。可用环境变量 MAKURIS_STUN 覆盖（逗号分隔），
/// 便于换成内网自建的 STUN——公共服务器在受限网络里可能不通。
///
/// 顺序按「国内可达性」排，不按名气：Google 的 STUN 在国内常被阻断，
/// 排第一个会让每次会话都白等一个超时（实测出现过「STUN 收集超时，
/// 跳过公网候选」直接导致本次会话没有 srflx 候选、跨网络必失败）。
/// Cloudflare 与小米的服务器在实测中都能通，放前面。
const DEFAULT_SERVERS: &[&str] = &[
    "stun.cloudflare.com:3478",
    "stun.miwifi.com:3478",
    "stun.l.google.com:19302",
];

pub fn servers() -> Vec<String> {
    match std::env::var("MAKURIS_STUN") {
        Ok(v) if !v.trim().is_empty() => v
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => DEFAULT_SERVERS.iter().map(|s| (*s).to_owned()).collect(),
    }
}

/// 依次询问各 STUN 服务器，返回第一个拿到的公网映射地址。
///
/// 全部失败时返回 None——这不致命：返回 None 时会话照旧只有 host 候选，
/// 局域网直连仍能用，只是跨网络不通。调用方据此决定是否告警。
pub async fn discover(socket: &UdpSocket, servers: &[String]) -> Option<SocketAddr> {
    for server in servers {
        match query(socket, server).await {
            Some(addr) => {
                tracing::info!("[stun] {} 报告公网映射 {}", server, addr);
                return Some(addr);
            }
            None => tracing::debug!("[stun] {} 无响应或响应不合法", server),
        }
    }
    None
}

/// 向单个服务器发一次 Binding 请求并解析响应。
async fn query(socket: &UdpSocket, server: &str) -> Option<SocketAddr> {
    let tid = transaction_id();
    let mut req = [0u8; HEADER];
    req[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // 长度字段保持 0：本请求不带任何属性
    req[2..4].copy_from_slice(&0u16.to_be_bytes());
    req[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    req[8..20].copy_from_slice(&tid);

    socket.send_to(&req, server).await.ok()?;

    let start = tokio::time::Instant::now();
    loop {
        let left = QUERY_TIMEOUT.checked_sub(start.elapsed())?;
        let mut buf = [0u8; 576];
        let (n, _) = tokio::time::timeout(left, socket.recv_from(&mut buf))
            .await
            .ok()?
            .ok()?;
        // 收到的未必是我们要的：会话还没开始，但网卡上会有 mDNS、系统广播等
        // 其它 UDP 流量。事务 ID 对不上就继续等，免得把别人的地址当自己的。
        if let Some(addr) = parse_response(&buf[..n], &tid) {
            return Some(addr);
        }
    }
}

/// 解析 Binding 成功响应里的映射地址。校验严格，不合格一律返回 None。
fn parse_response(buf: &[u8], tid: &[u8; 12]) -> Option<SocketAddr> {
    if buf.len() < HEADER {
        return None;
    }
    let typ = u16::from_be_bytes([buf[0], buf[1]]);
    if typ != BINDING_RESPONSE {
        return None;
    }
    if u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) != MAGIC {
        return None;
    }
    if &buf[8..20] != tid {
        return None;
    }

    // 属性区长度以头部声明为准，且不能越过实际收到的字节数
    let declared = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let end = HEADER.checked_add(declared)?.min(buf.len());

    let mut pos = HEADER;
    while pos + 4 <= end {
        let attr = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let len = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]) as usize;
        let val = buf.get(pos + 4..pos + 4 + len)?;

        let parsed = match attr {
            // XOR 版：端口异或魔术字高 16 位，IPv4 整体异或魔术字
            ATTR_XOR_MAPPED if val.len() >= 8 && val[1] == 0x01 => {
                let port = u16::from_be_bytes([val[2], val[3]]) ^ 0x2112;
                let raw = u32::from_be_bytes([val[4], val[5], val[6], val[7]]) ^ MAGIC;
                Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(raw)), port))
            }
            // 明文版：只在对方不支持 XOR 时才会出现
            ATTR_MAPPED if val.len() >= 8 && val[1] == 0x01 => {
                let port = u16::from_be_bytes([val[2], val[3]]);
                let raw = u32::from_be_bytes([val[4], val[5], val[6], val[7]]);
                Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(raw)), port))
            }
            // IPv6 映射（family=0x02）暂不支持：家庭宽带场景先只覆盖 IPv4，
            // 有了 IPv6 直连反而是另一条更简单的路，届时单独做。
            _ => None,
        };
        if parsed.is_some() {
            return parsed;
        }

        // 属性按 4 字节对齐，下一项从这里往后推
        pos += 4 + len + ((4 - len % 4) % 4);
    }
    None
}

/// 生成 12 字节事务 ID。
///
/// 这里不做认证，ID 只需在同机并发查询间不重复；因此用纳秒时间戳 + 进程内
/// 自增计数混合即可，不必为它引入随机数依赖（host 目前并没有 rand）。
fn transaction_id() -> [u8; 12] {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) as u64;

    let mut out = [0u8; 12];
    out[0..8].copy_from_slice(&(nanos ^ (seq << 40)).to_be_bytes());
    out[8..12].copy_from_slice(&((nanos as u32) ^ (seq as u32)).to_be_bytes());
    out
}