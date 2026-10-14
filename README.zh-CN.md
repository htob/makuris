# Makuris

**简体中文** | [English](README.md)

<img src="brand/logo-1024.png" alt="Makuris" width="120" />

**用手机远程控制家里的电脑。** 开源、自托管、跨平台。

Makuris 是一套远程桌面方案：被控端（Host）跑在你要控制的电脑上，控制端（Viewer）是 iOS App。
画面与输入经 WebRTC 在两端之间直连或中继传输，信令服务器只负责牵线，不参与数据传输。

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)
![Host: macOS / Windows](https://img.shields.io/badge/host-macOS%20%7C%20Windows-lightgrey)
![Viewer: iOS](https://img.shields.io/badge/viewer-iOS-lightgrey)

---

## 特性

- **局域网零配置**：被控端通过 Bonjour 广播 `_makuris._tcp`，手机在同一 Wi-Fi 下打开 App 即自动列出电脑，
  不需要输入任何地址，也不依赖中央服务器。
- **跨网络可用**：不在同一网络时，经信令服务器协商、由 coturn（STUN/TURN）助攻建立 WebRTC 连接，4G 下也能出画。
- **媒体端到端加密**：WebRTC 的 DTLS-SRTP，信令服务器既看不到画面也看不到输入。
- **硬件编码**：macOS 走 VideoToolbox 硬编，并按 2× 物理分辨率采集，文字清晰不糊。
- **完整输入回控**：鼠标移动 / 左右键 / 滚轮 / 拖拽，以及键盘文本输入；
  Windows 侧额外处理多显示器虚拟桌面坐标与组合键修饰状态。
- **多通道切换**：把「局域网」自动扫描与每条已保存地址各作为一条通道，通道下挂各自的电脑；
  网络切换后自动重连，不必重启 App。

## 平台支持现状

| 组件 | 平台 | 状态 | 说明 |
| --- | --- | --- | --- |
| 被控端 Host | macOS | ✅ 可用 | CGWindowList 采集、CGEvent 注入、VideoToolbox 硬编、系统 Bonjour |
| 被控端 Host | Windows | ✅ 可用 | GDI BitBlt 采集（声明 DPI 感知）、SendInput 注入、openh264 编码、mdns-sd 广播 |
| 被控端 Host | Linux | ⬜ 未实现 | 规划中 |
| 控制端 Viewer | iOS | ✅ 可用 | Swift + WebRTC，Bonjour 自动发现 + 手动保存地址 |
| 控制端 Viewer | Android | ⬜ 未实现 | 规划中 |
| 信令服务器 | 跨平台 | ✅ 可用 | Rust axum + WebSocket，可 Docker 部署 |
| TURN 中继 | Linux | ✅ 可用 | coturn，`use-auth-secret` 时效凭证 |

## 工作原理

```
                    ┌──────────────────────────────────┐
                    │           信令服务器              │
                    │   Rust axum, wss://<host>:8443   │
                    │  设备注册 / 信令转发 / TURN 凭证签发 │
                    └───────────┬──────────────────────┘
                                │ 信令：SDP / ICE / 配对
                 ┌──────────────┴──────────────┐
                 │                             │
      ┌──────────▼───────────┐     ┌───────────▼──────────┐
      │     被控端 Host       │◄───►│   控制端 Viewer       │
      │    macOS / Windows   │     │      iOS App         │
      │  采集 → 编码 → 推流    │WebRTC│  收流 → 渲染 + 输入回传 │
      └──────────────────────┘     └──────────────────────┘
                 │
                 │ 同一局域网时：Bonjour 广播 + 8444 内置直连信令
                 └──────────── 无需中心服务器 ────────────┘
```

两条路径：

1. **局域网**：Bonjour 发现 → 8444 端口内置信令直连 → 本地 WebRTC 直连（流量不过任何服务器）。
2. **跨网络**：连 wss 信令服务器 → ICE 打洞，打不通则回落到 coturn 中继。

## 仓库结构

```
makuris/
├── proto/                 # 共享协议：信令消息、输入事件、配对（Rust）
├── host/                  # 被控端（Rust）
│   ├── src/capture/       # 屏幕采集（macOS surface / Windows GDI）
│   ├── src/encode/        # 视频编码（VideoToolbox 硬编 / openh264）
│   ├── src/input/         # 输入注入（CGEvent / SendInput）
│   ├── src/lan.rs         # 局域网直连信令 + Bonjour 广播
│   ├── src/bonjour.rs     # 系统 Bonjour FFI（仅 macOS）
│   ├── src/pairing.rs     # 配对与信任令牌
│   ├── src/tls_pin.rs     # 信令 wss 证书指纹固定
│   └── src/turn.rs        # TURN 中继客户端
├── signaling-server/      # 信令服务器（Rust axum + WebSocket + TLS）
├── ios-client/            # iOS 控制端（Swift + WebRTC）
├── deploy/                # Docker Compose（信令 + coturn）与 systemd 单元
├── packaging/             # macOS .app 打包（含菜单栏助手）
├── scripts/               # 部署与诊断脚本（cloud / windows / e2e）
├── brand/                 # Logo 母版
└── docs/                  # 文档（含开源前安全审计报告）
```

## 快速开始

### 前置条件

- Rust stable 工具链（edition 2021）
- macOS 被控端需要 Xcode Command Line Tools
- iOS 控制端需要完整 Xcode 16+ 与真机，详见 [ios-client/SETUP.md](ios-client/SETUP.md)

### 1. 启动被控端

```bash
cargo run -p makuris-host
```

首次运行需在系统设置里授权：

- **macOS**：屏幕录制 + 辅助功能（系统设置 → 隐私与安全性）
- **Windows**：如需控制以管理员权限运行的窗口，被控端本身也要以管理员身份运行

### 2. 局域网连接（零配置）

手机与电脑连同一 Wi-Fi，打开 App，首页「局域网」通道会自动列出这台电脑，点击即进入画面。

### 3. 跨网络连接

启动信令服务器：

```bash
cargo run -p makuris-signaling
# 默认监听 ws://0.0.0.0:8443/ws
```

让被控端连上信令（两端必须用同一把接入密钥）：

```bash
MAKURIS_SIGNALING=wss://your-server:8443/ws \
MAKURIS_ACCESS_KEY=<与信令端一致> \
cargo run -p makuris-host
```

或用 Docker Compose 一次拉起信令与 coturn：

```bash
cd deploy && docker compose up -d
# 上线前务必修改 coturn 的 TURN 密钥并配置 TLS
```

### 4. 构建 iOS 控制端

```bash
open ios-client/Makuris.xcodeproj
```

Bonjour 权限声明等关键配置见 [ios-client/SETUP.md](ios-client/SETUP.md)。

### 5. 跑测试

```bash
cargo test -p makuris-proto
```

## 配置项

### 被控端（host）

| 环境变量 | 默认值 | 说明 |
| --- | --- | --- |
| `MAKURIS_DEVICE_ID` | `host-mac-01` / `host-win-01` / `host-linux-01` | 设备标识，信令路由键 |
| `MAKURIS_SIGNALING` | 空 | 信令地址（如 `wss://host:8443/ws`）；不填则只走局域网 |
| `MAKURIS_ACCESS_KEY` | 空 | 接入密钥，须与信令服务器一致 |
| `MAKURIS_PAIRING_CODE` | 随机生成 | 固定配对码；默认随机生成并写入状态目录 |
| `MAKURIS_LAN_PORT` | `8444` | 局域网直连信令端口 |
| `MAKURIS_UDP_PORT` | 自动 | WebRTC 直连 UDP 端口 |
| `MAKURIS_STUN` | 内置公共 STUN | 覆盖 STUN 服务器地址 |
| `MAKURIS_STATE_DIR` | 平台惯例 | 状态目录（配对码、信任令牌） |
| `MAKURIS_LOG_DIR` | macOS `~/Library/Logs/Makuris`；Windows `%LOCALAPPDATA%\Makuris\logs` | 日志目录 |

### 信令服务器

| 环境变量 | 默认值 | 说明 |
| --- | --- | --- |
| `PORT` | `8443` | 监听端口 |
| `MAKURIS_ACCESS_KEY` | 自动生成并落盘 | 接入密钥；设备需填同一串 |
| `MAKURIS_NO_ACCESS_KEY` | 空 | 设了就关闭接入鉴权 ⚠️ 仅建议纯局域网自用 |
| `MAKURIS_TLS_CERT` / `MAKURIS_TLS_KEY` | `/etc/makuris/tls/cert.pem` / `key.pem` | 证书与私钥；缺失则回落明文 `ws`（公网不建议） |
| `MAKURIS_TURN_SECRET` | 空 | coturn 共享密钥；不配则不下发 TURN 凭证 |
| `MAKURIS_TURN_URLS` | 空 | TURN 地址列表（逗号分隔） |
| `MAKURIS_TURN_REALM` | 空 | TURN realm |
| `MAKURIS_TURN_TTL` | 见代码默认 | 时效凭证有效期（秒） |
| `MAKURIS_STATE_DIR` | 平台惯例 | 状态目录 |

## 安全模型

1. **接入密钥**：设备注册信令须附密钥的 HMAC 证明；未通过者既拿不到设备列表，也不能转发信令。
2. **配对与信任令牌**：首次连接需在被控端输入配对码（HMAC-SHA256 挑战应答，算法与位数在 `proto` 中统一定义）；
   配对成功后签发信任令牌，之后免输。
3. **TLS 指纹固定**：信令用自签证书，客户端不信任任何公共 CA，只认证书的 SHA256 指纹（`host/src/tls_pin.rs`）。
4. **TURN 时效凭证**：coturn 走 `use-auth-secret`，凭证由信令在设备注册时下发并在到期前续签；
   **源码零秘密**，即使凭证泄露也会在数小时内自动失效。
5. **媒体端到端加密**：WebRTC DTLS-SRTP，信令服务器不接触画面与输入。

> 开源前的凭证硬编码与个人信息排查记录见 [docs/OPEN_SOURCE_AUDIT.md](docs/OPEN_SOURCE_AUDIT.md)。

## 构建与打包

### macOS .app（推荐）

```bash
./packaging/build_app.sh release
# 产物：dist/Makuris.app（内含菜单栏助手）
```

直接跑 `cargo run` 的裸二进制每次重编译都会丢掉「屏幕录制 / 辅助功能」授权，打成 .app 后可稳定授权。

### Windows 交叉编译（在 macOS 上产出 .exe）

```bash
brew install mingw-w64
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu -p makuris-host
# 产物：target/x86_64-pc-windows-gnu/release/makuris-host.exe
```

### 信令服务器

```bash
cargo build --release -p makuris-signaling
```

云端部署脚本：`scripts/deploy-signaling.sh`（云端原生编译 + systemd）。

## 测试与诊断

```bash
cargo test -p makuris-proto                                 # 协议单测
cargo check -p makuris-host --features windows_typecheck    # 在非 Windows 上类型检查 Windows 代码
```

`scripts/` 下另有端到端与诊断脚本：

- `lan_admission_e2e.mjs` — 局域网准入回归
- `switch_path_e2e.mjs` — 切换连接线路回归
- `turn_renew_e2e.mjs` — TURN 凭证续签
- `security_e2e.mjs` — 安全回归
- `scripts/windows/diag-*.ps1` — Windows 侧诊断（mDNS 广播、端口占用、防火墙、进程状态）

## 已知限制

- **Linux 被控端未实现**。
- **Windows 采集走 GDI BitBlt**：DirectX / 独占全屏应用可能采到黑屏，后续按需升级 DXGI。
- **Windows 输入注入受 UIPI 限制**：普通权限进程无法向以管理员权限运行的窗口注入输入。
- **被控端全局单会话**：同一台被控端同时只服务一个控制端。
- **控制端目前只有 iOS**，Android 未实现。
- **公网跨 NAT 需自建 coturn**；未配 TURN 时只剩直连候选。

## 路线图

- [x] macOS 被控端
- [x] Windows 被控端
- [x] iOS 控制端 + 局域网零配置
- [x] 公网信令 + TURN 中继
- [ ] Linux 被控端
- [ ] Android 控制端
- [ ] Windows DXGI 采集（独占全屏）

## 贡献

欢迎提 issue 与 PR。提交前请确保以下检查通过：

```bash
cargo test -p makuris-proto
cargo check -p makuris-host
cargo check --target x86_64-pc-windows-gnu -p makuris-host   # 改动 host 时
cargo check -p makuris-signaling
```

提交信息请使用英文，并按 `feat:` / `fix:` / `docs:` / `chore:` 前缀分类。

## License

[Apache License 2.0](LICENSE)。

## 致谢

本项目建立在以下开源工作之上：

- [tokio](https://tokio.rs/)、[axum](https://github.com/tokio-rs/axum)、[serde](https://serde.rs/)、[tracing](https://github.com/tokio-rs/tracing)
- [webrtc-rs](https://github.com/webrtc-rs/webrtc)（`rtc` / `rtc-turn`）
- [OpenH264](https://github.com/cisco/openh264)（Cisco，软件编码）
- [rustls](https://github.com/rustls/rustls)（ISC/Apache-2.0）、[tokio-tungstenite](https://github.com/snapview/tokio-tungstenite)
- [mdns-sd](https://github.com/keepsimple1/mdns-sd)
- [stasel/WebRTC](https://github.com/stasel/WebRTC)（iOS 预编译框架，BSD-3-Clause）
- [coturn](https://github.com/coturn/coturn)（BSD-3-Clause）

各依赖的版权与许可证以其自身仓库为准。
