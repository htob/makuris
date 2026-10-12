#!/usr/bin/env python3
"""NAT 类型探测：判断出口 NAT 是否可打洞，并预判 P2P 可行性。

在写公网链路之前先测这个，是因为打洞能不能成完全取决于 NAT 的映射行为：
如果 NAT 对同一个内网 ip:port 到不同目标的映射**保持不变**（锥形），
那么交换 srflx 候选后双方互发 UDP 就能打通；如果映射随目标变化（对称），
候选交换得到的地址只对某个对端有效，互发必然失败，只能退回 TURN 中继。

方法：用**同一个**本地 UDP socket（这一点是关键——换成多个 socket 各自绑定
不同本地端口，映射本来就不同，测不出任何东西）依次向多个不同 IP 的 STUN
服务器发 Binding Request，比较各自观察到的 XOR-MAPPED-ADDRESS：
  · IP 与端口全部一致  → Endpoint-Independent Mapping（锥形 NAT），可打洞
  · IP 一致但端口不同  → 端口/地址依赖映射（对称 NAT），打洞大概率失败
"""

import os
import socket
import struct
import sys
import time

MAGIC = 0x2112A442
BINDING_REQUEST = 0x0001
BINDING_RESPONSE = 0x0101
ATTR_XOR_MAPPED = 0x0020

# 优先国内 STUN，避免跨境链路本身超时造成误判；Google 仅作辅助
SERVERS = [
    ("stun.qq.com", 3478),
    ("stun.miwifi.com", 3478),
    ("stun.chat.bilibili.com", 3478),
    ("stun.l.google.com", 19302),
    ("stun.cloudflare.com", 3478),
]

TIMEOUT = 3.0


def build_request():
    """构造 Binding Request：类型 + 长度 0 + 魔术字 + 12 字节事务 ID。"""
    return struct.pack(">HHI", BINDING_REQUEST, 0, MAGIC) + os.urandom(12)


def parse_xor_mapped(data):
    """从 Binding Response 里取出 XOR-MAPPED-ADDRESS（仅处理 IPv4）。"""
    if len(data) < 20:
        return None
    mtype, mlen = struct.unpack(">HH", data[:4])
    if mtype != BINDING_RESPONSE:
        return None

    pos, end = 20, min(20 + mlen, len(data))
    while pos + 4 <= end:
        atype, alen = struct.unpack(">HH", data[pos : pos + 4])
        val = data[pos + 4 : pos + 4 + alen]
        if atype == ATTR_XOR_MAPPED and len(val) >= 8 and val[1] == 1:
            port = struct.unpack(">H", val[2:4])[0] ^ (MAGIC >> 16)
            ip = bytes(b ^ m for b, m in zip(val[4:8], struct.pack(">I", MAGIC)))
            return socket.inet_ntoa(ip), port
        pos += 4 + alen + ((4 - alen % 4) % 4)
    return None


def probe(sock, host, port):
    """向单个 STUN 服务器发请求，返回 (映射地址, 错误说明)。"""
    try:
        infos = socket.getaddrinfo(host, port, socket.AF_INET, socket.SOCK_DGRAM)
        target = infos[0][4]
    except socket.gaierror as e:
        return None, f"DNS 解析失败({e})"

    try:
        sock.sendto(build_request(), target)
    except OSError as e:
        return None, f"发送失败({e})"

    deadline = time.time() + TIMEOUT
    while time.time() < deadline:
        try:
            data, src = sock.recvfrom(2048)
        except socket.timeout:
            return None, "超时"
        except OSError as e:
            return None, f"接收失败({e})"
        # 只认目标服务器的回复（同一 socket 会收到别处的包）
        if src[0] == target[0] and src[1] == target[1]:
            mapped = parse_xor_mapped(data)
            return (mapped, None) if mapped else (None, "响应无 XOR-MAPPED-ADDRESS")
    return None, "超时"


def main():
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("0.0.0.0", 0))
    sock.settimeout(TIMEOUT)
    local = sock.getsockname()

    print(f"本机 UDP socket: {local[0]}:{local[1]}")
    print(f"目标服务器: {len(SERVERS)} 台（同一 socket 依次探测）\n")

    results = []
    for host, port in SERVERS:
        mapped, err = probe(sock, host, port)
        if mapped:
            print(f"  ✅ {host}:{port:<6} → 映射 {mapped[0]}:{mapped[1]}")
            results.append(mapped)
        else:
            print(f"  ⚠️  {host}:{port:<6} → {err}")
    sock.close()

    print()
    if not results:
        print("❌ 全部 STUN 服务器无响应。")
        print("   可能原因：代理/TUN 接管了 UDP（此前实测过：代理开启时只有代理出口能收到回复），")
        print("   或防火墙拦截。请确认已关闭代理后重试。")
        return 2

    if len(results) == 1:
        print(f"⚠️  只有 1 台服务器响应，样本不足，无法判定 NAT 类型。")
        print(f"   已观测映射：{results[0][0]}:{results[0][1]}")
        return 1

    ips = {ip for ip, _ in results}
    ports = {p for _, p in results}

    if len(ips) == 1 and len(ports) == 1:
        ip, port = results[0]
        print(f"✅ NAT 类型：Endpoint-Independent Mapping（锥形 NAT）")
        print(f"   所有服务器看到的映射完全一致：{ip}:{port}")
        print(f"   → 打洞可行性：高。交换 srflx 候选后双方互发 UDP 即可建立直连。")
        return 0

    if len(ips) == 1 and len(ports) > 1:
        ip = results[0][0]
        print(f"❌ NAT 类型：Address/Port-Dependent Mapping（对称 NAT）")
        print(f"   出口 IP 固定为 {ip}，但映射端口随目标变化：{sorted(ports)}")
        print(f"   → 打洞可行性：低。候选交换得到的地址只对特定对端有效，")
        print(f"     跨网络互发大概率失败，需要 TURN 中继（deploy/coturn 已备好）。")
        return 3

    print(f"⚠️  出口 IP 不一致（{sorted(ips)}），说明走了多出口/代理链路。")
    print(f"   → 请关闭代理后重测。")
    return 4


if __name__ == "__main__":
    sys.exit(main())