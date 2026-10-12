import Foundation
import Network

/// 通过 Bonjour 自动发现的电脑。
///
/// 地址取自 host 广播的 TXT 记录（ip/port），而不是把服务的域名交给系统去解析。
/// 原因：本机存在代理的 utun 虚拟网卡（198.18.0.1），域名解析可能命中错误的
/// 网卡地址；而 TXT 里的 ip 是 host 自己枚举网卡后认准的局域网地址。
struct DiscoveredComputer: Identifiable, Equatable {
    /// 用 Bonjour 实例名作唯一标识
    var id: String { serviceName }
    let serviceName: String
    let deviceId: String
    let platform: String
    let host: String
    let port: Int

    /// 可直接连接的 WebSocket 地址（与本项目其它来源统一走同一协议）
    var normalizedURL: String {
        host.isEmpty ? "" : "ws://\(host):\(port)/ws"
    }

    /// 界面副标题
    var subtitle: String {
        platform.isEmpty ? host : "\(platform) · \(host)"
    }
}

/// Bonjour 局域网发现。
///
/// 为什么要有它：局域网里手机和电脑在同一网段、互相可见，却要用户去查 IP
/// 和端口再手敲——这是把实现细节当成了配置项。Bonjour 让这一步变成零操作。
///
/// 注意 iOS 14+ 的硬性要求（缺一个就静默失败，不报错）：
///   1. Info.plist 必须声明 NSBonjourServices 且包含 `_makuris._tcp`；
///   2. 必须有 NSLocalNetworkUsageDescription，否则连授权弹窗都不会出现。
/// 两者都已写入 Makuris/Info.plist。
@MainActor
final class BonjourDiscovery: ObservableObject {
    @Published private(set) var computers: [DiscoveredComputer] = []
    /// 是否已完成启动（用于界面区分「没找到」和「还没开始找」）
    @Published private(set) var isReady = false
    /// 本地网络权限被拒时置位，界面据此提示用户去设置里开启
    @Published private(set) var permissionDenied = false

    private var browser: NWBrowser?

    /// 服务类型必须与 host 端广播的一致（host/src/lan.rs 的 REGTYPE）
    private static let serviceType = "_makuris._tcp"

    func start() {
        guard browser == nil else { return }

        let params = NWParameters()
        // 只走常规网络，不开点对点（AWDL），否则会在列表里冒出连不上的条目
        params.includePeerToPeer = false

        // 必须用 bonjourWithTXTRecord：普通的 .bonjour 描述符不会去取 TXT 记录，
        // result.metadata 会一直是 .none，于是拿不到 host 广播的 ip/port，
        // 只能回退到「实例名.local」这种域名。而 .local 要靠 mDNS 解析，
        // 在手机上可能命中代理 utun / 虚拟网卡 / link-local 地址
        // （实测被控端看到的来源是 169.254.x.x），表现就是「列表里明明有
        // 这台电脑，点了却连不上」。TXT 里的 ip 才是 host 自己枚举网卡后
        // 认准的局域网地址，也是「已保存的电脑」那条直接填 IP 的路径能连通的原因。
        let browser = NWBrowser(
            for: .bonjourWithTXTRecord(type: Self.serviceType, domain: nil),
            using: params)

        browser.stateUpdateHandler = { [weak self] state in
            Task { @MainActor in
                guard let self else { return }
                switch state {
                case .ready:
                    self.isReady = true
                case .failed(let error):
                    // 被拒时系统给的是 -65555（policy denied）之类的错误
                    let ns = error as NSError
                    if ns.code == -65555 || ns.domain.contains("Network") {
                        self.permissionDenied = true
                    }
                    print("[bonjour] 浏览失败: \(error)")
                case .cancelled:
                    self.isReady = false
                default:
                    break
                }
            }
        }

        browser.browseResultsChangedHandler = { [weak self] results, _ in
            let found = results.compactMap(Self.parse)
            Task { @MainActor in
                self?.computers = found
                print("[bonjour] 发现 \(found.count) 台电脑: \(found.map(\.serviceName))")
            }
        }

        browser.start(queue: .main)
        self.browser = browser
    }

    func stop() {
        browser?.cancel()
        browser = nil
        isReady = false
    }

    /// 手动重扫：先用浏览器概览一次全量结果
    func rescan() {
        guard let browser else { return }
        // 重新发起一次查询，不需要重建 browser
        browser.browseResultsChangedHandler?(browser.browseResults, [])
    }

    /// 从浏览结果中解析出 TXT 记录里的连接信息
    private static func parse(_ result: NWBrowser.Result) -> DiscoveredComputer? {
        guard case .service(let name, _, _, _) = result.endpoint else { return nil }

        var deviceId = name
        var platform = ""
        var host = ""
        var port = 8444

        if case .bonjour(let txt) = result.metadata {
            let dict = txt.dictionary
            if let v = dict["id"], !v.isEmpty { deviceId = v }
            platform = dict["platform"] ?? ""
            host = dict["ip"] ?? ""
            if let raw = dict["port"], let parsed = Int(raw) { port = parsed }
        }

        // 优先用 TXT 里的 ip：那是 host 自己枚举网卡后认准的局域网地址。
        // 万一没有（老版本 host，或该平台广播未带此字段），回退到 Bonjour 的
        // .local 主机名——系统会把它解析到正确的局域网地址。两级兜底，
        // 避免出现「列表里有这台电脑，点了却毫无反应」。
        let resolvedHost = host.isEmpty ? "\(name).local" : host

        return DiscoveredComputer(
            serviceName: name,
            deviceId: deviceId,
            platform: platform,
            host: resolvedHost,
            port: port)
    }
}