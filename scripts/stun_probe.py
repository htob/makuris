#!/usr/bin/env python3
"""独立探测本机 STUN 反射地址，用于判断 UDP 出口是否被代理劫持。

为什么需要它：WebRTC 连通性完全依赖 srflx 候选正确。实测过一次故障——Mac 上
Clash Verge 开着时，被控端收集到的 srflx 是代理服务器 IP（203.0.113.30），
而 HTTPS 出口仍是家宽真值（203.0.113.20）。两者不一致就说明 UDP 被劫持，
此时对端往代理 IP 打洞必然进黑洞，表现为 ICE 30 秒 Failed、推送 0 帧。

只读探测，不改动任何网络配置。
"""
import os
import socket
import struct
import sys

MAGIC = 0x2112A442
BINDING_REQUEST = 0x0001
BINDING_SUCCESS = 0x0101
ATTR_MAPPED_ADDRESS = 0x0001
ATTR_XOR_MAPPED_ADDRESS = 0x0020


def parse_attrs(data: bytes):
    i = 20  # 跳过 20 字节消息头
    while i + 4 <= len(data):
        attr_type, attr_len = struct.unpack("!HH", data[i:i + 4])
        i += 4
        value = data[i:i + attr_len]
        i += attr_len + ((4 - attr_len % 4) % 4)  # 属性按 4 字节对齐
        yield attr_type, value


def read_mapped(value: bytes, xor: bool):
    """把 MAPPED-ADDRESS / XOR-MAPPED-ADDRESS 解析成 (ip, port)。"""
    family = value[1]
    port = struct.unpack("!H", value[2:4])[0]
    if xor:
        port ^= MAGIC >> 16
    if family == 0x01:  # IPv4
        raw = value[4:8]
        addr = bytes(b ^ (MAGIC >> shift & 0xFF) for b, shift in
                     zip(raw, (24, 16, 8, 0))) if xor else raw
        return socket.inet_ntoa(addr), port
    return None, port  # IPv6 本项目用不上


def probe(server: str, port: int, timeout: float = 8.0) -> str:
    tx = os.urandom(12)
    msg = struct.pack("!HHI", BINDING_REQUEST, 0, MAGIC) + tx
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(timeout)
    try:
        host_ip = socket.getaddrinfo(server, port, socket.AF_INET)[0][4]
        sock.sendto(msg, host_ip)
        data, _ = sock.recvfrom(2048)
    except Exception as exc:  # 网络不通/超时都如实报告
        return f"{server}:{port}  ❌ {type(exc).__name__}: {exc}"
    finally:
        sock.close()

    if struct.unpack("!H", data[:2])[0] != BINDING_SUCCESS:
        return f"{server}:{port}  ❌ 响应不是 BindingSuccess"

    for attr_type, value in parse_attrs(data):
        if attr_type == ATTR_XOR_MAPPED_ADDRESS:
            ip, p = read_mapped(value, xor=True)
            if ip:
                return f"{server}:{port}  ✅ srflx = {ip}:{p}"
        elif attr_type == ATTR_MAPPED_ADDRESS:
            ip, p = read_mapped(value, xor=False)
            if ip:
                return f"{server}:{port}  ✅ srflx = {ip}:{p}（明文属性）"
    return f"{server}:{port}  ❌ 响应里没有映射地址属性"


SERVERS = [
    ("stun.l.google.com", 19302),
    ("stun1.l.google.com", 19302),
    ("stun.cloudflare.com", 3478),
]


def main() -> int:
    print("本机 STUN 出口探测（UDP 直发，不经系统代理设置）")
    print("-" * 56)
    results = []
    for server, port in SERVERS:
        line = probe(server, port)
        print("  " + line)
        results.append(line)
    ips = {r.split("srflx = ")[1].split(":")[0] for r in results if "✅" in r}
    print("-" * 56)
    if not ips:
        print("结论：三个 STUN 服务器全部无响应 —— UDP 出不了网，代理需彻底关闭。")
        return 1
    if len(ips) > 1:
        print(f"结论：不同 STUN 报出不同出口 {sorted(ips)} —— 存在按目标分流的代理。")
    else:
        print(f"结论：统一出口 {sorted(ips)[0]}。")
    print("对照家宽真值：curl -s https://ifconfig.me/ip")
    return 0


if __name__ == "__main__":
    sys.exit(main())
