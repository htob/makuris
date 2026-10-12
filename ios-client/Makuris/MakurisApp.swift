import SwiftUI
import AVFoundation

@main
struct MakurisApp: App {
    @StateObject private var appState = AppState()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(appState)
        }
    }
}

/// 后台保活：让远程会话在 app 切到后台后不断。
///
/// 原理：没有后台模式的 app 被切到后台后，系统会在几秒内挂起整个进程——
/// WebRTC 的信令 socket、信令心跳、视频解码全部停摆，用户切回来只看到
/// 「未连接」。声明 UIBackgroundModes=audio 后，只要存在**活跃的音频会话**
/// 进程就会继续运行，连接得以存活。
///
/// 这里播放的是绝对静音：AVAudioSourceNode 的渲染回调把输出缓冲清零，
/// 用户听不到任何声音，也不会占用麦克风（只用 .playback，不录音）。
/// 这是远程桌面类应用的通行做法，未触碰私有 API。
///
/// 注意：不进后台时不要启动它——没有必要常驻一个音频会话，
/// 白耗电。只在进入后台时 start()，回到前台就 stop()。
final class BackgroundKeepAlive {
    static let shared = BackgroundKeepAlive()

    private var engine: AVAudioEngine?
    private(set) var isRunning = false

    private init() {}

    /// 启动静音播放以维持后台运行。
    func start() {
        guard !isRunning else { return }

        // .playback 才能进后台（.ambient 会被静音并可能被挂起）；
        // .mixWithOthers 避免打断用户正在听的音乐。
        do {
            let session = AVAudioSession.sharedInstance()
            try session.setCategory(.playback, mode: .default, options: [.mixWithOthers])
            try session.setActive(true)
        } catch {
            print("[keepalive] 音频会话激活失败: \(error)")
            return
        }

        let engine = AVAudioEngine()
        guard let format = AVAudioFormat(standardFormatWithSampleRate: 44100, channels: 2) else {
            print("[keepalive] 音频格式创建失败")
            return
        }

        // 渲染回调：把所有输出缓冲清零 = 静音。
        // 不清零的话读到的会是未初始化内存，可能放出噪声。
        let source = AVAudioSourceNode { _, _, _, audioBufferList -> OSStatus in
            let buffers = UnsafeMutableAudioBufferListPointer(audioBufferList)
            for buffer in buffers {
                if let data = buffer.mData {
                    memset(data, 0, Int(buffer.mDataByteSize))
                }
            }
            return noErr
        }

        engine.attach(source)
        engine.connect(source, to: engine.mainMixerNode, format: format)
        // 双保险：即使回调有残留，混音器音量也归零
        engine.mainMixerNode.outputVolume = 0

        do {
            try engine.start()
        } catch {
            print("[keepalive] 音频引擎启动失败: \(error)")
            try? AVAudioSession.sharedInstance().setActive(false)
            return
        }

        self.engine = engine
        isRunning = true
        print("[keepalive] 后台保活已启动")
    }

    /// 停止保活并释放音频会话（回到前台后调用，避免长期白耗电）。
    func stop() {
        guard isRunning else { return }
        engine?.stop()
        engine = nil
        isRunning = false
        // notifyOthersOnDeactivation 让被我们抢占的音频恢复正常
        try? AVAudioSession.sharedInstance().setActive(false, options: [.notifyOthersOnDeactivation])
        print("[keepalive] 后台保活已停止")
    }
}

/// 全局应用状态
class AppState: ObservableObject {
    /// 被控电脑地址。刻意允许只填一个 IP（如 192.168.1.3）：
    /// 协议、端口、路径都由 `normalizedURL` 补全——这些是协议细节，
    /// 不该要求用户记住，普通用户只需要知道「家里那台电脑的地址」。
    /// 写入 UserDefaults，下次启动自动带上，不必重填。
    @Published var serverAddress: String {
        didSet { UserDefaults.standard.set(serverAddress, forKey: Self.addressKey) }
    }
    @Published var deviceId: String

    /// 信令服务器的接入密钥（服务器没启用鉴权时留空）。
    ///
    /// 它是「服务器级的」而不是「每台电脑级的」：一个信令服务器配一把密钥，
    /// 所有连它的设备共用。所以放在全局状态里，不必在添加每台电脑时都填一遍
    /// ——重复填写是用户最容易漏、也最容易被嫌麻烦而放弃的一环。
    ///
    /// 局域网直连（Bonjour 发现的那台电脑）根本不过信令服务器，用不到这个值。
    @Published var accessKey: String {
        didSet { UserDefaults.standard.set(accessKey, forKey: Self.accessKeyKey) }
    }

    private static let addressKey = "makuris.serverAddress"
    private static let accessKeyKey = "makuris.accessKey"
    /// 与 signaling-server 的默认监听端口一致
    static let defaultPort = 8443
    static let defaultPath = "/ws"

    /// 按协议给默认端口。
    ///
    /// 不能一律补 8443：公网地址通常是反向代理/隧道域名（自带 TLS，只监听 443），
    /// 补成 8443 会直接连不上，而用户看到的错误只是「连接失败」，无从判断。
    /// wss 走 443 与 https 惯例一致；ws 才回落到自建的 8443。
    static func defaultPort(forScheme scheme: String) -> Int {
        (scheme == "wss" || scheme == "https") ? 443 : defaultPort
    }

    /// 判断主机名是否属于「公网域名」（而非局域网地址）。
    ///
    /// 判据刻意保守：只要看着像局域网（IPv4、.local、无点短名）就按局域网处理。
    /// 宁可把公网域名误判成局域网（用户填错了立刻连不上，改成手写 wss:// 即可），
    /// 也不要把局域网误判成公网——那会让原本好好能用的局域网连接突然要求 TLS，
    /// 用户面对的是「昨天还能连，今天就不行了」这种无从下手的变化。
    static func looksLikePublicHost(_ host: String) -> Bool {
        if host.isEmpty { return false }
        // 只由数字和点组成 → IPv4，内网直填的典型形态
        if host.allSatisfy({ $0.isNumber || $0 == "." }) { return false }
        // mDNS 局域网名（Bonjour 场景）
        if host.lowercased().hasSuffix(".local") { return false }
        // 无点短名是内网主机名，如 localhost、my-mac
        if !host.contains(".") { return false }
        return true
    }

    init() {
        // 云信令已 TLS 化（wss + 指纹固定），默认地址直接用 wss。
        // 存量装机在 UserDefaults 里存的是旧的明文 ws://203.0.113.10…，
        // 服务器已不再接受明文连接，不迁移的话升级后用户面对的是
        // 莫名其妙的「连接失败」——这里做一次性前缀迁移。
        let saved = UserDefaults.standard.string(forKey: Self.addressKey)
        if let s = saved, s.hasPrefix("ws://203.0.113.10") {
            serverAddress = "wss" + s.dropFirst(2)
        } else {
            serverAddress = saved ?? "wss://203.0.113.10:8443/ws"
        }
        // 带 didSet 的属性不能写默认值（Swift 不允许），必须在 init 里显式赋值，
        // 否则编译器直接报「未初始化所有存储属性」。
        accessKey = UserDefaults.standard.string(forKey: Self.accessKeyKey) ?? ""
        deviceId = "ios-\(UIDevice.current.name.replacingOccurrences(of: " ", with: "-"))"
    }

    /// 规范化为可直接连接的 WebSocket 地址；返回空字符串表示配置无效。
    var normalizedURL: String { Self.normalize(serverAddress) }

    /// 规范化的纯函数版本：不读也不写实例状态，供界面预览草稿时调用。
    /// （若用实例属性算预览，就得先篡改 serverAddress，那会顺手把草稿
    ///  持久化下去——用户点了取消也已经写进去了。）
    static func normalize(_ input: String) -> String {
        var text = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return "" }

        // 没写协议时按主机形态推断，这样只填一个地址就能用：
        //   域名 或 公网 IP → 默认 wss（信令迁云后公网服务器已 TLS 化，
        //                    明文 ws 会把配对流量暴露给嗅探）
        //   .local / 无点短名 → 局域网直连，走 ws + 8443
        // 不能一律补 ws://：域名配 8443 恰恰是连不上的组合，而用户看到的只是
        // 「连接失败」，根本联想不到端口头上——这种错误最难自查。
        // 注意：公网 IP 直连云信令也走 wss（203.0.113.10 即此形态），
        // 与局域网内网 IP 的区分靠主机名形态，与历史行为一致。
        if !text.contains("://") {
            let bareHost = text.split(separator: ":").first.map(String.init) ?? text
            // 云信令只服务 wss（自签证书 + 指纹固定），这个 IP 必须特判；
            // 其余裸 IPv4 仍按局域网直连补 ws——内网自建 host 没有 TLS。
            text = (Self.looksLikePublicHost(bareHost) || bareHost == "203.0.113.10"
                ? "wss://" : "ws://") + text
        }
        // 存量地址语义迁移：云信令（203.0.113.10）已 TLS-only，服务器不再接受
        // 明文，任何指向它的 ws://（无论来自 AppState 设置、已保存电脑列表还是
        // legacy 单地址迁移）都必须改写为 wss，否则用户看到的只是莫名 -1005。
        // 放在 normalize 而不是 AppState.init：init 够不着 SavedComputers 里
        // 持久化的列表项（2026-10-12 验收时 iPhone 就是这么连不上的）。
        // 幂等：已是 wss 的不受影响，内网 IP 的 ws:// 不动。
        if text.hasPrefix("ws://203.0.113.10") {
            text = "wss" + text.dropFirst(2)
        }
        // http(s) 是常见笔误，WebSocket 需要对应的 ws(s)
        text = text.replacingOccurrences(of: "https://", with: "wss://")
                   .replacingOccurrences(of: "http://", with: "ws://")

        guard let url = URL(string: text), let host = url.host else { return "" }
        let scheme = url.scheme ?? "ws"
        let port = url.port ?? Self.defaultPort(forScheme: scheme)
        let path = (url.path.isEmpty || url.path == "/") ? Self.defaultPath : url.path
        return "\(scheme)://\(host):\(port)\(path)"
    }

    /// 界面展示用地址（去掉协议与默认路径，只留「主机:端口」）
    var displayAddress: String {
        guard let url = URL(string: normalizedURL), let host = url.host else {
            return serverAddress
        }
        return "\(host):\(url.port ?? Self.defaultPort(forScheme: url.scheme ?? "ws"))"
    }
}
