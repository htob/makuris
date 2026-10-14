# iOS 客户端构建指南

## 前置条件

- macOS + **完整 Xcode 16+**（App Store 下载，约 12GB）
- iOS 真机（推荐）或模拟器
- 被控端（host）已在局域网运行（Bonjour 自动发现）或信令服务器可达

## 项目结构

工程文件已自带，直接用 Xcode 打开 `Makuris.xcodeproj` 即可，无需手工创建：

```
ios-client/
├── Makuris.xcodeproj
├── project.yml                        # XcodeGen 配置（可选，用于重新生成工程）
└── Makuris/
    ├── MakurisApp.swift               # App 入口 + 地址规范化
    ├── Info.plist                     # Bonjour 权限声明（关键，见下）
    ├── Models/
    │   └── SignalMessage.swift        # 信令消息 + 输入事件（与 Rust proto 对齐）
    ├── Services/
    │   ├── SignalingClient.swift      # WebSocket 信令客户端
    │   ├── WebRTCSession.swift        # PeerConnection 管理
    │   ├── BonjourDiscovery.swift     # 局域网自动发现
    │   └── SavedComputers.swift       # 手动保存的电脑列表
    └── Views/
        ├── ContentView.swift          # 电脑列表 + 添加/删除
        ├── RemoteScreenView.swift     # 视频渲染 + 触摸手势
        └── KeyboardInputCaptureView.swift # 键盘输入
```

## 局域网自动发现（Bonjour）的两个硬性要求

iOS 14+ 起，浏览/连接局域网设备需要声明，**缺任何一个都会静默失败且不报错**：

1. `NSBonjourServices` 必须包含 `_makuris._tcp`（与 host 端 `host/src/lan.rs` 的 `REGTYPE` 逐字一致）
2. 必须有 `NSLocalNetworkUsageDescription`

两者都在 `Makuris/Info.plist` 中，并通过 pbxproj 的 `INFOPLIST_FILE = Makuris/Info.plist` 生效。
改动后可用下面的命令核对产物里是否真的写进去了：

```bash
plutil -p <构建产物>/Makuris.app/Info.plist | grep -E "NSBonjourServices|NSLocalNetwork"
```

首次运行会弹「允许访问本地网络」，**必须点允许**，否则列表为空（app 会提示去设置开启）。

## 添加 WebRTC 依赖

工程已通过 Swift Package Manager 引用预编译 WebRTC 框架：

- URL: `https://github.com/stasel/WebRTC.git`（Dependency Rule: Up to Next Major）

如引用丢失，在 Xcode → File → Add Package Dependencies 里重新添加 `WebRTC` 到 Makuris target。
备选：从 https://webrtc.org/download 下载官方 framework 手动集成。

## 运行

1. 确保被控端运行：`cargo run -p makuris-host`（Mac）
2. （可选，跨网络才需要）信令服务器：`cargo run -p makuris-signaling`
3. Xcode 中选择真机，Build & Run
4. 同一 Wi-Fi 下 app 会自动列出电脑，点击即进入；不在同一网络时用右上角加号手动添加地址

## 触摸手势映射

| 手势 | 操作 |
|------|------|
| 单指拖动 | 鼠标移动 |
| 单指点击 | 鼠标左键 |
| 双指点击 | 鼠标右键 |
| 双指滑动 | 滚轮 |
| 长按 | 鼠标左键按住（拖拽） |
| 键盘按钮 | 文本输入 |

## 注意事项

- 真机调试需要 Apple Developer 账号（免费账号可 sideload，7天有效期）
- 模拟器无法测试 WebRTC 视频渲染（无 GPU 直通），建议用真机
- 命令行部署用 `xcrun devicectl`；注意 `xcodebuild -destination` 需要设备 UDID，不是 devicectl 里的 coredevice ID