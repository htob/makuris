//! 本机网络地址探测。
//!
//! 单独立一个模块，是被三处共用逼出来的：局域网广播（lan.rs）、WebRTC 候选
//! （session.rs）、公网自检（bin/check_stun.rs）。此前它长在 lan.rs 里，而
//! lan.rs 依赖 crate::session 与 crate::bonjour，独立 bin 用 #[path] 引它会
//! 连带拉进半个 crate、直接编译不过。放在这里，谁都取得干净。
//!
//! 难点在 macOS：本机装着代理，内核多出一张 utun 虚拟网卡（198.18.0.1）。
//! 用「connect 一个外部地址再看 local_addr」这种取巧办法，内核会挑中代理网卡，
//! 拿到一个外界根本连不上的地址——而且不报错，只在实连时表现为莫名超时。
//! 所以这里一律枚举网卡，只认私有且非链路本地的 IPv4，并跳过 utun/lo。

use std::net::{IpAddr, Ipv4Addr};

/// 取本机真实的局域网 IPv4（macOS）。
#[cfg(target_os = "macos")]
pub(crate) fn local_ip() -> Option<IpAddr> {
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return None;
        }
        let mut found = None;
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name)
                .to_string_lossy()
                .into_owned();
            // 跳过回环与隧道（代理/VPN）网卡
            if !name.starts_with("utun") && !name.starts_with("lo") && !ifa.ifa_addr.is_null() {
                let addr = &*ifa.ifa_addr;
                if addr.sa_family as i32 == libc::AF_INET {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    let v4 = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                    if v4.is_private() && !v4.is_link_local() {
                        found = Some(IpAddr::V4(v4));
                        break;
                    }
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
        found
    }
}

/// 取本机真实的局域网 IPv4（非 macOS）。
///
/// 这里本可以用「连一个外部地址让内核选出口网卡」的办法——它在 macOS 上不可靠
/// （会选中代理的 utun 网卡），但 Windows/Linux 的默认路由通常就是真实网卡。
/// 不过实测台式机上也装了虚拟网卡，照样拿到 198.18.0.1，故与 macOS 一样枚举网卡。
/// 仍校验取到的是私有地址：若返回公网或回环地址，说明选错了网卡，
/// 此时宁可返回 None 交给 mDNS 自行挑选接口，也不要广播一个连不上的地址。
#[cfg(not(target_os = "macos"))]
pub(crate) fn local_ip() -> Option<IpAddr> {
    let ifaces = if_addrs::get_if_addrs().ok()?;
    for iface in ifaces {
        if let if_addrs::IfAddr::V4(v4) = iface.addr {
            if v4.ip.is_private() && !v4.ip.is_link_local() {
                return Some(IpAddr::V4(v4.ip));
            }
        }
    }
    None
}