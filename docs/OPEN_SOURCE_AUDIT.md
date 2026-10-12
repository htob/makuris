---
name: 2026_10_12_open_source_audit
title: 开源前安全审计报告（2026-10-12）
description: 目的：git push 到公开仓库之前的全仓风险排查。排查范围：host / signaling server / ios client / scripts / packaging / deploy / proto / brand / .cargo。
summary: 目的：git push 到公开仓库之前的全仓风险排查。排查范围：host / signaling server / ios client / scripts / packaging / deploy / proto / brand / .cargo。
keywords: []
category: Users/user/code/makuris/docs
---

# 开源前安全审计报告（2026-10-12）

> 目的：git push 到公开仓库之前的全仓风险排查。排查范围：host / signaling-server / ios-client / scripts / packaging / deploy / proto / brand / .cargo。
> 结论：**P0 凭证硬编码已清除（2026-10-12 凭证下发改造）**；P1 个人信息建议后续参数化；P2 已确认安全。
> 本文档自身遵守红线：**不记录任何真实凭证值**（此前版本曾误写接入密钥值，已重写脱敏）。需要取值时看云端 `/etc/makuris/turn.env`、`/etc/makuris/signaling.env` 或本机 `access-key.txt`，这些文件均不入库。

## P0 · 长期凭证硬编码（已修复）

原问题：TURN 静态用户的用户名 + 32 位密码明文写死 4 处：

| 位置 | 形态 |
|---|---|
| `host/src/stream/mod.rs` turn_ice_servers()（约 75-76 行） | username/credential 字面量 |
| `host/src/stream/mod.rs:56` 注释 | 历史埋点记录原文含用户名 |
| `ios-client/Makuris/Services/WebRTCSession.swift` 379-380、388-389 | 两个 RTCIceServer 各一份 |
| `ios-client/.../WebRTCSession.swift:371` 注释 | 同上泄露用户名 |

风险：代码公开 = 云服务器 TURN 密码公开，任何人可消耗带宽、把中继当跳板。

**修复（2026-10-12 已实施）**：coturn 切 `use-auth-secret` 时效凭证（REST API 风格）：
- 云端：`scripts/cloud/switch-turn-rest.sh` 生成 `/etc/makuris/turn.env`（secret 600 权限），coturn 经 wrapper 注入，unit 文件零秘密。
- 信令：`signaling-server` 新增 TurnIssuer，从 `MAKURIS_TURN_SECRET/URLS/REALM/TTL` 环境变量构建，注册成功后向设备下发 `SignalMsg::TurnCredentials`（username=过期时间戳:设备id，credential=Base64(HMAC-SHA1(secret, username))）。
- host：`stream/mod.rs` 硬编码已移除，改为 `set_turn_creds()` 接收信令下发；无凭证时仅直连（局域网场景天然可用）。
- iOS：`SignalMessage/SignalingClient/WebRTCSession` 三文件同步镜像改造。
- 效果：源码零秘密；泄露的凭证数小时后自动失效。

**提交前扫描红线**（以下真实值在任何将入库文件中出现即拒绝提交；值本身不落本文档）：
1. TURN 静态用户名与密码（见云端 turn.env 注释）
2. 接入密钥（见云端 signaling.env / 本机 access-key.txt）
3. PEM 私钥头文本（BEGIN 与 KEY 词组拼接，此处刻意不写完整原文，否则本文件会命中自家扫描规则）
4. coturn realm 与长期 secret 的组合作为字面量出现在源码

## P1 · 个人网络信息（建议参数化，非红线）

- 公网 IP `203.0.113.10`：12 个文件（多为默认值/注释/部署脚本参数，已支持环境变量覆盖）
- 本机用户名 `user`：14 个文件（deploy 脚本默认值、文档示例）
- 局域网 IP `192.168.1.x`：8 处（均为注释、placeholder 或脚本默认值）

开源后中期改进：部署脚本默认值改从 `scripts/.deploy.env` 读取（已被 .gitignore 覆盖）。

## P2 · 已确认安全

- ✅ 接入密钥（access key）：信令端 `load_or_create_key` 读环境变量/文件，客户端用户自填，源码零硬编码。
- ✅ TLS 私钥：`*.pem/*.key` 仅存在于云服务器 `/etc/makuris/tls/`，仓库内无。
- ✅ 证书指纹：`host/src/tls_pin.rs` 以字节数组存 DER SHA256。指纹是公钥哈希、属公开信息，pin 本就是设计行为，可入库。
- ✅ 无隐私产物：源码区无日志/抓包/token/截图；png 均为 logo 资源。
- ✅ WebRTC 框架不入库：经 SPM 远程包（`stasel/WebRTC.git`）拉取，ios-client 仅源码。
- ✅ 构建产物：target/ 8.1G、dist/ 63M 已被 .gitignore 覆盖。
- ✅ deploy/ 配置：systemd 单元经 EnvironmentFile 注入密钥；docker-compose 用 `CHANGE_ME_TURN_SECRET` 占位符。

## 遗留（开源后中期）

- 时效凭证到期/重连时重新获取（host 长驻进程需在断线重连后随注册重新拿凭证；会话中途过期暂不刷新，单会话时长通常小于 TTL）。
- iOS 侧 TURN 因框架丢凭证本就不生效（媒体靠 host 侧 relay），下发凭证对 iOS 仅为配置完整性。
- README 增加第三方许可章节（WebRTC BSD-3、rustls ISC/Apache 等）。
