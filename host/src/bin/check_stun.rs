//! 公网连通自检：验证 STUN 收集能否拿到本机的 NAT 映射地址，并确认它确实
//! 被写进 offer 的候选列表。
//!
//! 用途有两个：
//!   1. 开发时验证 stun.rs 的收集链路（不依赖 iOS、不启动局域网服务，独立可跑）；
//!   2. 换网络环境（换宽带、换驻地、开热点）后自检——跨网络连不上时，
//!      第一步要分清是「本机在对称 NAT 后，打洞本就不可行」还是「代码没收到候选」。
//!
//! 用法：cargo run --release --bin check_stun

use std::time::Duration;

use rtc::peer_connection::transport::{
    CandidateConfig, CandidateHostConfig, CandidateServerReflexiveConfig, RTCIceCandidate,
};

#[path = "../stun.rs"]
mod stun;
#[path = "../netinfo.rs"]
mod netinfo;
#[path = "../stream/mod.rs"]
mod stream;
#[path = "../turn.rs"]
mod turn;

#[tokio::main]
async fn main() {
    // 与 session.rs 一致：绑 0.0.0.0:0，让 OS 分配端口。
    // 关键在于 STUN 查询必须复用这一个 socket——NAT 按「内网 ip:port」分配映射，
    // 换个 socket 去问，拿到的端口与候选里写的对不上。
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await.expect("bind");
    let local = socket.local_addr().expect("local_addr");
    println!("本会话 UDP socket: {local}");

    let servers = stun::servers();
    println!("STUN 服务器: {servers:?}");

    let began = std::time::Instant::now();
    let mapped = stun::discover(&socket, &servers).await;
    let cost = began.elapsed();
    match mapped {
        Some(addr) => println!("✅ 公网映射: {addr}（耗时 {} ms）", cost.as_millis()),
        None => {
            println!("❌ 未拿到公网映射（耗时 {} ms）", cost.as_millis());
            println!("   局域网直连不受影响；跨网络将只有 host 候选可用。");
            println!("   可用 MAKURIS_STUN 指定可达的 STUN 服务器后重试。");
        }
    }

    // 把候选走进真实的 PeerConnection，看它们是否出现在 SDP 里。
    // 这一步才是对「offer 里有没有 srflx」的最终确认——收集成功但注入失败，
    // 对端同样拿不到这个地址。
    let (mut pc, _sender) = stream::create_peer_connection(
        vec![rtc::peer_connection::transport::RTCIceServer {
            urls: servers.iter().map(|s| format!("stun:{s}")).collect(),
            ..Default::default()
        }],
        "640033",
    )
    .expect("create pc");

    if let Some(ip) = netinfo::local_ip() {
        let c = CandidateHostConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: ip.to_string(),
                port: local.port(),
                component: 1,
                ..Default::default()
            },
            ..Default::default()
        }
        .new_candidate_host()
        .expect("host candidate");
        pc.add_local_candidate(RTCIceCandidate::from(&c).to_json().unwrap())
            .expect("add host");
    }

    if let Some(addr) = mapped {
        let c = CandidateServerReflexiveConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: addr.ip().to_string(),
                port: addr.port(),
                component: 1,
                ..Default::default()
            },
            rel_addr: netinfo::local_ip().map(|ip| ip.to_string()).unwrap_or_default(),
            rel_port: local.port(),
            url: None,
        }
        .new_candidate_server_reflexive()
        .expect("srflx candidate");
        pc.add_local_candidate(RTCIceCandidate::from(&c).to_json().unwrap())
            .expect("add srflx");
    }

    let sdp = stream::create_offer_sdp(&mut pc).expect("offer");
    println!("\n=== offer 里的候选行 ===");
    let mut host_count = 0;
    let mut srflx_count = 0;
    for line in sdp.lines().filter(|l| l.starts_with("a=candidate")) {
        println!("{line}");
        if line.contains("typ host") {
            host_count += 1;
        }
        if line.contains("typ srflx") {
            srflx_count += 1;
        }
    }
    println!("\nhost 候选 {host_count} 条，srflx 候选 {srflx_count} 条");
    if srflx_count > 0 {
        println!("✅ offer 里已含公网候选，跨网络具备打洞条件");
    } else {
        println!("⚠️ offer 里没有公网候选，跨网络必然连不上");
    }

    tokio::time::sleep(Duration::from_millis(50)).await;
}