# ios-client

iOS 控制端，Swift + WebRTC（stasel/WebRTC 预编译框架）。

工程：`Makuris.xcodeproj`（bundle id `cc.makwo.makuris`），构建步骤见 [SETUP.md](SETUP.md)。

## 主要模块

- `MakurisApp.swift`：入口；`AppState.normalize()` 把用户输入的 IP 补全为完整 ws 地址
- `SignalingClient.swift`：WebSocket 信令（与 proto 的 SignalMsg JSON 对齐）
- `WebRTCSession.swift`：PeerConnection 管理（收视频流 + DataChannel 发输入事件）
- `BonjourDiscovery.swift`：`NWBrowser` 浏览 `_makuris._tcp` 自动发现局域网电脑
- `SavedComputers.swift`：可增删的手动地址列表（跨网络兜底），JSON 持久化
- `RemoteScreenView.swift`：RTCVideoRenderer 渲染 + 触摸手势 → InputEvent 映射
- `KeyboardInputCaptureView.swift`：键盘输入（注意：监听键盘事件只能用 NotificationCenter，
  不要用 UITextField.addTarget，否则弹键盘时因 selector 签名不匹配而闪退）

## 发现与连接

同一 Wi-Fi 下由 Bonjour 自动发现，点列表即进画面，不需要输入地址；
不在同一网络时用右上角加号保存固定地址。