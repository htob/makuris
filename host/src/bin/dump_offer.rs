//! 一次性诊断工具：打印 host 生成的 offer SDP 全文。
//!
//! 用途：iPhone 回 answer 时 host 报 `multiple conflicting ice-ufrag values`，
//! 而 Chrome 正常。该错误由 rtc 的 extract_ice_details 抛出——要求 answer 里
//! 所有**非 inactive** 的 m-line 共享同一 ice-ufrag。要判断是 offer 里的 ufrag
//! 写法有问题，还是要修正对 answer 的解析，必须先看清 offer 原文。
//!
//! 只为排查用，不参与生产链路。

use std::time::Instant;

// host 是 bin-only crate，用 #[path] 直接引入生产源码，确保看到的就是线上那份 offer。
#[path = "../stream/mod.rs"]
mod stream;
#[path = "../turn.rs"]
mod turn;

fn main() {
    let profile_level_id = "640033";
    let (mut pc, _sender_id) = stream::create_peer_connection(
        vec![rtc::peer_connection::transport::RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_string()],
            ..Default::default()
        }],
        profile_level_id,
    )
    .expect("create pc");

    // 与 session.rs 完全一致的 DataChannel 配置
    let dc_init = rtc::data_channel::RTCDataChannelInit {
        ordered: false,
        max_retransmits: Some(0),
        ..Default::default()
    };
    pc.create_data_channel("makuris-input", Some(dc_init))
        .expect("create dc");

    // 一个假的 host candidate，模拟局域网直连（真实端口由 OS 分配，与结构无关）
    use rtc::peer_connection::transport::{
        CandidateConfig, CandidateHostConfig, RTCIceCandidate,
    };
    let candidate = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.1.100".to_owned(),
            port: 54321,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()
    .expect("build candidate");
    pc.add_local_candidate(RTCIceCandidate::from(&candidate).to_json().unwrap())
        .expect("add candidate");

    let _ = Instant::now();
    let sdp = stream::create_offer_sdp(&mut pc).expect("create offer");

    println!("=== OFFER SDP 全文 ===");
    println!("{sdp}");
    println!("=== END ===");

    // 摘要：列出每条 m-line 的 ice-ufrag，方便直接判断是否冲突
    println!("\n=== m-line 与 ice-ufrag 对照 ===");
    let mut cur_media = String::new();
    for line in sdp.lines() {
        if line.starts_with("m=") {
            cur_media = line.to_owned();
            println!("{cur_media}");
        } else if line.starts_with("a=ice-ufrag") || line.starts_with("a=ice-pwd") {
            println!("    {line}");
        } else if line.starts_with("a=inactive")
            || line.starts_with("a=sendonly")
            || line.starts_with("a=recvonly")
            || line.starts_with("a=group")
        {
            println!("    {line}");
        }
    }
}