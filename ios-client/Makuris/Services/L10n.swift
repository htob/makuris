//
//  L10n.swift
//  Makuris
//
//  界面语言：一套自建的小型本地化层。
//
//  为什么不用系统的 String Catalog（Localizable.xcstrings）：
//  本项目要的是「App 内切换语言」，而 SwiftUI 的自动本地化只在
//  Text("字面量") 这条路径上生效——本工程里大量文案是存在 String 属性里的
//  （信令状态、通道失败原因、连接阶段说明），那些走不到自动本地化，
//  逐处包 String(localized:) 改造面更大、也更容易漏翻。这里用一个
//  集中的文案表，取值点显式调用 L10n.t(.key)，中英两版在同一处对照，
//  漏翻一眼就能看出来。
//
//  语言切换后立即生效，不需要重启 App：L10nStore 是 ObservableObject，
//  所有显示文案的视图都用 @EnvironmentObject 订阅它，切换时统一重算。
//

import Foundation
import Combine

/// 界面语言选项。
enum AppLanguage: String, CaseIterable, Identifiable {
    /// 跟随手机系统语言（默认值）
    case system
    case zhHans = "zh-Hans"
    case en

    var id: String { rawValue }

    /// 设置页里显示的名字。语言名按惯例用该语言自身书写（简体中文 / English），
    /// 只有「跟随系统」需要翻译。
    var displayName: String {
        switch self {
        case .system: return L10n.t(.languageFollowSystem)
        case .zhHans: return "简体中文"
        case .en: return "English"
        }
    }

    /// 落到实际使用的语言：system 时看系统首选语言。
    /// 中文系一律按简体处理——本项目只提供简体一套中文文案。
    var resolved: AppLanguage {
        guard self == .system else { return self }
        let preferred = Locale.preferredLanguages.first ?? "en"
        return preferred.hasPrefix("zh") ? .zhHans : .en
    }
}

/// 文案表。新增界面文案时：先在 Key 里加一项，再在 table 里补齐中英两版。
enum L10n {
    /// 语言设置的存储键。
    static let storageKey = "makuris.appLanguage"

    enum Key: String {
        // 通用动作
        case done
        case cancel
        case save
        case connect

        // 语言设置
        case languageSection
        case languageFollowSystem
        case languageHint

        // 首页
        case appTagline
        case lanChannel
        case autoScan
        case addComputer
        case noComputersFound
        case searching
        case noOnlineComputers
        case deleteChannel
        case cannotConnectWithReason
        case cannotSearchLan
        case lanPermissionHint
        case statusOnline
        case statusConnecting
        case statusNotConnected

        // 添加电脑
        case addComputerTitle
        case nameLabel
        case namePlaceholder
        case addressLabel
        case addressHint
        case willConnect
        case invalidAddress
        case accessKeyOptionalLabel
        case accessKeyOptionalPlaceholder
        case ownAccessKeyBadge

        // 设置页
        case settingsTitle
        case signalingAccessKey
        case accessKeyPlaceholder
        case accessKeyHint
        case deviceIdLabel
        case deviceIdHint

        // 信令状态与连接错误
        case stateConnecting
        case stateReady
        case connectionFailed
        case invalidAddressFormat
        case noOnlineDeviceHint
        case addressUnrecognized
        case cannotConnectHint
        case connectTimeoutCheckNetwork
        case phoneOffline
        case connectFailedWithCode

        // 通道扫描
        case addressUnrecognizedShort
        case cannotConnectShort

        // 连接遮罩 / 配对
        case connectingToServer
        case connectionNotEstablished
        case waitingForComputer
        case receivingScreen
        case establishingVideoChannel
        case connectTimeoutHint
        case pairingSessionExpired
        case firstConnectTakesSeconds
        case pairingRequired
        case pairingHint

        // 远程会话界面
        case backToHome
        case remoteSession
        case remoteKeyboard
        case metricFps
        case metricLatency
        case metricBitrate
        case metricResolution

        // 远程键盘工具条
        case keyboardHide

        // 旧数据迁移时补的默认名
        case myComputer
    }

    /// 中英对照表。占位符写 {0}、{1}，按顺序替换。
    private static let table: [Key: (zh: String, en: String)] = [
        // 通用动作
        .done: ("完成", "Done"),
        .cancel: ("取消", "Cancel"),
        .save: ("保存", "Save"),
        .connect: ("连接", "Connect"),

        // 语言设置
        .languageSection: ("语言", "Language"),
        .languageFollowSystem: ("跟随系统", "Follow System"),
        .languageHint: (
            "切换后立即生效，无需重启。",
            "Takes effect immediately, no restart needed."
        ),

        // 首页
        .appTagline: ("远程操控你的电脑", "Control your computer remotely"),
        .lanChannel: ("局域网", "Local Network"),
        .autoScan: ("自动扫描", "Auto scan"),
        .addComputer: ("添加电脑", "Add Computer"),
        .noComputersFound: ("没找到电脑", "No computers found"),
        .searching: ("正在查找…", "Searching…"),
        .noOnlineComputers: ("没有在线的电脑", "No computers online"),
        .deleteChannel: ("删除通道", "Remove Channel"),
        .cannotConnectWithReason: ("连不上：{0}", "Can't connect: {0}"),
        .cannotSearchLan: ("无法搜索局域网", "Can't Search the Local Network"),
        .lanPermissionHint: (
            "请在「设置 → 隐私与安全性 → 本地网络」中允许 Makuris 访问，或点下方「添加电脑」手填地址连接。",
            "Allow Makuris to access the local network under Settings → Privacy & Security → Local Network, or tap \"Add Computer\" below and enter an address manually."
        ),
        .statusOnline: ("在线", "Online"),
        .statusConnecting: ("连接中", "Connecting"),
        .statusNotConnected: ("未连接", "Not connected"),

        // 添加电脑
        .addComputerTitle: ("添加电脑", "Add Computer"),
        .nameLabel: ("名称", "Name"),
        .namePlaceholder: ("家里的 Mac", "Home Mac"),
        .addressLabel: ("地址", "Address"),
        .addressHint: (
            "只填 IP 即可，端口与路径自动补全。",
            "An IP is enough — port and path are filled in automatically."
        ),
        .willConnect: ("将连接", "Will connect to"),
        .invalidAddress: ("地址无效", "Invalid address"),
        .accessKeyOptionalLabel: ("接入密钥（可选）", "Access Key (optional)"),
        .accessKeyOptionalPlaceholder: (
            "留空则用默认密钥",
            "Leave empty to use the default key"
        ),
        .ownAccessKeyBadge: ("专用密钥", "own key"),

        // 设置页
        .settingsTitle: ("设置", "Settings"),
        .signalingAccessKey: ("默认接入密钥", "Default Access Key"),
        .accessKeyPlaceholder: (
            "服务器无密钥则留空",
            "Leave empty if the server has no key"
        ),
        .accessKeyHint: (
            "两端填同一串。这里是默认值，单条地址可单独覆盖。",
            "Both ends use the same string. This is the default; any address can override it."
        ),
        .deviceIdLabel: ("本机标识", "This Device ID"),
        .deviceIdHint: (
            "电脑端用它区分是哪台手机。",
            "The computer uses this to tell which phone is connecting."
        ),

        // 信令状态与连接错误
        .stateConnecting: ("正在连接…", "Connecting…"),
        .stateReady: ("已就绪", "Ready"),
        .connectionFailed: ("连接失败", "Connection failed"),
        .invalidAddressFormat: ("地址格式不正确", "Invalid address format"),
        .noOnlineDeviceHint: (
            "没找到在线的电脑：请确认被控端已启动，且与本机填的是同一个信令服务器",
            "No computers online: make sure the host is running and that both ends use the same signaling server."
        ),
        .addressUnrecognized: ("地址无法识别：{0}", "Unrecognized address: {0}"),
        .cannotConnectHint: (
            "连不上，请确认电脑已启动且与手机在同一网络",
            "Can't connect — make sure the computer is on and on the same network as the phone."
        ),
        .connectTimeoutCheckNetwork: (
            "连接超时，请检查网络",
            "Connection timed out. Check your network."
        ),
        .phoneOffline: ("手机未联网", "Phone is offline"),
        .connectFailedWithCode: ("连接失败（{0}）", "Connection failed ({0})"),

        // 通道扫描
        .addressUnrecognizedShort: ("地址无法识别", "Unrecognized address"),
        .cannotConnectShort: ("连不上", "Can't connect"),

        // 连接遮罩 / 配对
        .connectingToServer: ("正在连接服务器…", "Connecting to the server…"),
        .connectionNotEstablished: ("连接未能建立", "Connection couldn't be established"),
        .waitingForComputer: ("正在等待电脑响应…", "Waiting for the computer…"),
        .receivingScreen: ("正在接收画面…", "Receiving the screen…"),
        .establishingVideoChannel: (
            "正在建立视频通道…",
            "Setting up the video channel…"
        ),
        .connectTimeoutHint: (
            "连接超时。请确认电脑已开机并运行 Makuris，且网络可达。",
            "Connection timed out. Make sure the computer is on, running Makuris, and reachable on the network."
        ),
        .pairingSessionExpired: (
            "配对会话已失效，请重新连接",
            "The pairing session has expired. Please connect again."
        ),
        .firstConnectTakesSeconds: (
            "首次连接需要几秒钟",
            "The first connection takes a few seconds"
        ),
        .pairingRequired: ("需要配对", "Pairing Required"),
        .pairingHint: (
            "在电脑端的 Makuris 里查看 6 位配对码，填入后即可控制 {0}。",
            "Find the 6-digit pairing code in Makuris on the computer, then enter it to control {0}."
        ),

        // 远程会话界面
        .backToHome: ("返回主页", "Back to Home"),
        .remoteSession: ("远程会话", "Remote Session"),
        .remoteKeyboard: ("远程键盘", "Remote Keyboard"),
        .metricFps: ("帧率", "FPS"),
        .metricLatency: ("延迟", "Latency"),
        .metricBitrate: ("码率", "Bitrate"),
        .metricResolution: ("分辨率", "Resolution"),

        // 远程键盘工具条
        .keyboardHide: ("收起", "Hide"),

        // 旧数据迁移
        .myComputer: ("我的电脑", "My Computer"),
    ]

    /// 当前设置的语言。
    static var current: AppLanguage {
        let raw = UserDefaults.standard.string(forKey: storageKey)
            ?? AppLanguage.system.rawValue
        return AppLanguage(rawValue: raw) ?? .system
    }

    /// 取当前语言的文案。
    static func t(_ key: Key) -> String {
        guard let entry = table[key] else { return key.rawValue }
        return current.resolved == .zhHans ? entry.zh : entry.en
    }

    /// 带占位符的文案。
    ///
    /// 自己替换 {0} 而不是用 String(format:)：文案里出现 % 或引号时，
    /// 格式串那条路要额外转义，容易留下只在某个语言下才炸的坑。
    static func t(_ key: Key, _ arg: CVarArg) -> String {
        t(key).replacingOccurrences(of: "{0}", with: "\(arg)")
    }
}

/// 语言状态：写入 UserDefaults，并通过 ObservableObject 通知界面刷新。
///
/// 每个显示文案的视图都用 @EnvironmentObject 订阅它，因此切换语言后
/// 不需要重启 App——SwiftUI 会重算这些视图的 body，取值点拿到新语言的文案。
final class L10nStore: ObservableObject {
    static let shared = L10nStore()

    @Published var language: AppLanguage {
        didSet {
            guard language != oldValue else { return }
            UserDefaults.standard.set(language.rawValue, forKey: L10n.storageKey)
        }
    }

    private init() {
        let raw = UserDefaults.standard.string(forKey: L10n.storageKey)
            ?? AppLanguage.system.rawValue
        language = AppLanguage(rawValue: raw) ?? .system
    }

    /// 视图里用 l10n.t(.key) 取文案：既完成订阅，也让取值点集中在一处。
    func t(_ key: L10n.Key) -> String { L10n.t(key) }

    func t(_ key: L10n.Key, _ arg: CVarArg) -> String { L10n.t(key, arg) }
}
