---
name: makuris_readme
title: Makuris
description: 开源跨平台远程桌面：用手机远程控制家里的电脑。
summary: 开源跨平台远程桌面：用手机远程控制家里的电脑。
keywords: []
category: Users/user/code/makuris
---

# Makuris

<img src="brand/logo-1024.png" alt="Makuris Logo" width="128" />

开源跨平台远程桌面：用手机远程控制家里的电脑。

- **被控端（Host）**：Rust 实现，目前仅 macOS 可用
- **控制端（Viewer）**：iOS（Swift + WebRTC），Android 后续
- **传输层**：WebRTC（NAT 穿透 / 硬件编解码 / 自适应码率 / SRTP 加密）
- **云端**：信令服务器（Rust axum）+ coturn（STUN/TURN 中继）

## 平台支持现状

| 平台 | 被控端状态 | 说明 |
| --- | --- | --- |
| macOS | 可用 | 采集 CGWindowListCreateImage、输入注入 CGEvent、硬编 VideoToolbox、Bonjour 走系统 API |
| Windows | 未实现 | 计划用 Desktop Duplication API 采集、SendInput 注入 |
| Linux | 未实现 | 暂无排期 |

局域网场景零配置：host 通过 Bonjour 广播 `_makuris._tcp`，并在 8444 端口内置直连信令端点，
手机在同一 Wi-Fi 下打开 app 即自动发现，不需要输入任何地址，也不需要中央信令服务器。

## 仓库结构

```
makuris/
├── proto/               # 共享协议：信令消息 + 输入事件（Rust）
├── host/                # 被控端（Rust，macOS）
│   ├── src/lan.rs       # 局域网直连信令端点 + Bonjour 广播
│   ├── src/bonjour.rs   # 系统 Bonjour FFI（macOS）
│   ├── src/capture/     # 屏幕采集
│   └── src/input/       # 输入注入
├── signaling-server/    # 信令服务器（Rust axum + WebSocket）
├── ios-client/          # iOS 控制端（Swift + WebRTC，需完整 Xcode）
└── deploy/              # docker-compose（信令 + coturn）
```

## 快速开始（开发）

```bash
# 1. 启动被控端（含局域网直连与 Bonjour 广播）
cargo run -p makuris-host

# 2. 如需跨网络，再启动信令服务器
cargo run -p makuris-signaling
# 监听 ws://0.0.0.0:8443/ws

# 3. 跑测试
cargo test -p makuris-proto
```

## 当前状态

信令链路（注册/转发/心跳）、局域网直连与 Bonjour 自动发现已打通并被真机验证，
采集、编码与输入注入已接真实实现（macOS）。

## License

AGPL-3.0-or-later（待定，开源发布前最终确认）