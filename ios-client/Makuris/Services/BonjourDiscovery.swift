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

    /// 浏览器代次。每次 start/stop 递增，回调据此判断自己是否属于当前这一代。
    ///
    /// 为什么必须有：cancel 一个 NWBrowser 之后，它的状态回调与结果回调
    /// 仍会异步到达，而那时 self.browser 已经指向新建的那一个。不认身份就会
    /// 出两类错——旧浏览器的 failed 把新浏览器的状态写坏（实测：下拉刷新后
    /// 旧浏览器报 -65569 DefunctConnection，被误判成「本地网络权限被拒」，
    /// 首页整块换成权限提示，看起来就是「局域网里一台电脑都没有」），
    /// 以及旧结果覆盖刚取回的新列表。
    private var browserToken = 0

    /// 服务类型必须与 host 端广播的一致（host/src/lan.rs 的 REGTYPE）
    private static let serviceType = "_makuris._tcp"

    func start() {
        guard browser == nil else { return }

        // 每次启动都换一代，旧代次的回调（含取消后仍会异步到达的那些）随之作废。
        browserToken &+= 1
        let token = browserToken
        // 重新开始浏览就复位权限判定。它只该反映「本次浏览是否被系统拒绝」：
        // 不复位的话，一次误判（或用户后来去设置里把权限打开了）会让
        // 「无法搜索局域网」永久留在首页——列表从此再也不显示任何电脑。
        permissionDenied = false

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
                // 先认身份再认状态：不是当前这一代浏览器的回调一律丢弃。
                guard let self, self.browserToken == token else { return }
                switch state {
                case .ready:
                    self.isReady = true
                case .failed(let error):
                    // 只有系统明确表示「本地网络被策略拒绝」才认定权限问题。
                    //
                    // 不能沿用 ns.domain.contains("Network") 这种写法：NWError
                    // 桥接出来的 NSError，domain 就是 "Network"，那个条件对任何
                    // 网络错误都成立——实测把「-65569 DefunctConnection」（浏览器
                    // 被取消后的常见报错）也判成权限被拒，首页整段变成权限提示，
                    // 用户看到的就是「局域网里一台电脑都没有」。
                    if (error as NSError).code == -65555 {
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
                // 同样认身份：旧浏览器被取消后仍可能送来一次结果，
                // 不拦就会用陈旧列表覆盖刚取回的新列表。
                guard let self, self.browserToken == token else { return }
                self.computers = found
                print("[bonjour] 发现 \(found.count) 台电脑: \(found.map(\.serviceName))")
            }
        }

        browser.start(queue: .main)
        self.browser = browser
    }

    func stop() {
        // 先换一代，再取消：这样被取消浏览器随后到达的回调都会被判为旧代次丢掉。
        browserToken &+= 1
        browser?.cancel()
        browser = nil
        isReady = false
    }

    /// 手动重扫（下拉刷新用）：重建浏览器，强制重新发起一次查询。
    ///
    /// 不能沿用「把 browseResults 原样回放给回调」的老做法——那只是把同一份
    /// 列表再贴一遍，网段里新上线的电脑不会因此出现，用户以为刷过了其实什么
    /// 都没变（假刷新比没有刷新更误导）。重建浏览器才会重新发一次 mDNS 查询，
    /// 局域网应答在毫秒级，下拉手势的转圈期间就能拿到最新结果。
    ///
    /// stop() 会把 isReady 置回 false，界面那段「正在查找附近的电脑…」会短暂
    /// 重现，这也是真实的：此刻确实在重新找。若本来就有结果，列表不会变空，
    /// 用户看到的就是转圈一闪、列表还在。
    func restart() {
        stop()
        start()
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