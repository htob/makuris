import Combine
import Foundation

/// 一条通道的扫描结果。
enum ChannelScanState {
    /// 正在连这条地址
    case scanning
    /// 拿到了它下面的电脑。空数组表示这条地址连得上，但它上面没有在线的电脑。
    case online([DeviceInfo])
    /// 连不上，或被服务器拒绝：把原因留下来给人看。
    /// 存的是可翻译的失败种类（SignalFailure），不是成品句子——
    /// 界面语言切换后，同一条通道的提示要能立刻换成另一种语言。
    case failed(SignalFailure)
}

/// 通道扫描器：给每条已保存的地址各开一条独立连接，取回这条通道下面的电脑。
///
/// 为什么要独立连接：信令服务器按 device_id 路由，同一个 device_id 再次注册会把
/// 先前那条连接替换掉（signaling-server/src/main.rs:237）。若图省事复用主连接的
/// 身份，进一次首页就会把用户正在用的那条连接踢下线。所以探测连接另起一个后缀
/// 身份，角色用 viewer（不会出现在任何人的设备列表里）。
///
/// 为什么查完不断开：要的是「一打开首页就看到每条通道下有哪些电脑」，保持连接
/// 才能让列表跟着被控端的上下线自己变。每条通道一条 WebSocket 加 25 秒心跳，
/// 地址就那么几条，代价可以接受。
@MainActor
final class ChannelScanner: ObservableObject {
    /// 一条通道，对应一条用户添加的地址
    struct Channel: Identifiable {
        let id: UUID
        /// 显示名：用户起的名字，没起就用地址
        let title: String
        /// 展示用的原始地址
        let address: String
        var state: ChannelScanState
    }

    @Published private(set) var channels: [Channel] = []

    /// 每条通道的探测客户端，按通道 id 存
    private var clients: [UUID: SignalingClient] = [:]
    private var subscriptions: [UUID: Set<AnyCancellable>] = [:]
    /// 扫描超时任务：到点必须给结论，不能让界面一直转圈
    private var timeouts: [UUID: Task<Void, Never>] = [:]

    /// 扫描超时。
    ///
    /// 局域网一个来回只要毫秒，公网中继也就一两秒；留 8 秒是为了容忍冷启动的
    /// DNS 与 TLS 握手，再久就该有结论了。
    private static let scanTimeoutNanos: UInt64 = 8_000_000_000

    /// 按最新的地址列表重扫。
    ///
    /// 同一条地址已经在扫、或已经扫出结果时不重建连接：下拉刷新一次就把所有连接
    /// 拆掉重来，既慢，又会让整个列表闪一下。
    func scan(_ saved: [SavedComputer], deviceId: String, accessKey: String, force: Bool = false) {
        // 用户删掉的地址：连同它的连接一起收掉
        let alive = Set(saved.map(\.id))
        for id in clients.keys where !alive.contains(id) {
            teardown(id)
        }
        channels.removeAll { !alive.contains($0.id) }

        for item in saved {
            let address = item.normalizedURL
            guard !address.isEmpty else {
                apply(.failed(.addressUnrecognizedShort), for: item, address: item.address)
                continue
            }
            // 复用要同时满足两条：地址没改，而且那条连接确实还活着。
            //
            // 只看「地址没改」不够。切网之后那条连接会进入半开：客户端还以为
            // 自己连着，实际消息发不出去也收不回。若照旧复用，这条通道就会一直
            // 停在失败或空白上，下拉刷新也救不回来，只能重启 App——重建才拉得回来。
            if !force,
               let known = channels.first(where: { $0.id == item.id }),
               known.address == item.address,
               isAlive(item.id) {
                // 连接还在，只有名字可能被改过
                apply(known.state, for: item, address: item.address)
                // 但列表必须重新问一次：服务端不会主动把「谁刚上线」推给我，
                // 只在我问的时候才答。用户重启了一台被控端、或另一台电脑刚开机，
                // 全靠这一次请求才会出现在这条通道下面——只靠首次连接时的
                // 那一次请求，界面会一直停在上次看到的那些电脑上。
                if clients[item.id]?.state.isConnected == true {
                    clients[item.id]?.refreshDeviceList()
                }
            } else {
                start(item, address: address, deviceId: deviceId, accessKey: accessKey)
            }
        }

        // 显示顺序跟随用户的保存顺序，别让扫描结果把顺序打乱
        let order = saved.map(\.id)
        channels.sort {
            (order.firstIndex(of: $0.id) ?? 0) < (order.firstIndex(of: $1.id) ?? 0)
        }
    }

    /// 收掉全部探测连接。离开首页（或应用退到后台）时用。
    func stopAll() {
        for id in clients.keys { teardown(id) }
        channels = []
    }

    // MARK: - 内部

    private func start(_ item: SavedComputer, address: String, deviceId: String, accessKey: String) {
        teardown(item.id)
        apply(.scanning, for: item, address: item.address)

        let client = SignalingClient()
        client.accessKey = accessKey
        clients[item.id] = client

        var bag = Set<AnyCancellable>()
        client.$devices
            .sink { devices in
                Task { @MainActor [weak self] in
                    // 空数组不代表结果：注册瞬间列表本来就是空的，
                    // 真正的「这条通道上没有电脑」由超时那一步给结论。
                    guard !devices.isEmpty else { return }
                    self?.apply(.online(devices), for: item, address: item.address)
                }
            }
            .store(in: &bag)
        client.$state
            .sink { state in
                Task { @MainActor [weak self] in
                    guard case .failed(let reason) = state else { return }
                    self?.apply(.failed(reason), for: item, address: item.address)
                }
            }
            .store(in: &bag)
        subscriptions[item.id] = bag

        client.connect(url: address, deviceId: probeId(deviceId, item.id), role: .viewer)
        scheduleTimeout(for: item)
    }

    private func scheduleTimeout(for item: SavedComputer) {
        timeouts[item.id] = Task { [weak self] in
            try? await Task.sleep(nanoseconds: Self.scanTimeoutNanos)
            guard let self else { return }
            guard let channel = self.channels.first(where: { $0.id == item.id }),
                  case .scanning = channel.state else { return }
            if self.clients[item.id]?.state.isConnected == true {
                // 连上了却没有电脑：这条地址上没有在线的被控端
                self.apply(.online([]), for: item, address: item.address)
            } else {
                self.apply(.failed(.cannotConnectShort), for: item, address: item.address)
            }
        }
    }

    private func apply(_ state: ChannelScanState, for item: SavedComputer, address: String) {
        let channel = Channel(id: item.id, title: item.displayName, address: address, state: state)
        if let i = channels.firstIndex(where: { $0.id == item.id }) {
            channels[i] = channel
        } else {
            channels.append(channel)
        }
    }

    /// 这条通道的探测连接是否还有效。
    ///
    /// 已就绪或正在连都算有效；失败、断开、或压根没建过就都算无效，
    /// 调用方据此决定是复用还是重建。
    private func isAlive(_ id: UUID) -> Bool {
        guard let client = clients[id] else { return false }
        switch client.state {
        case .connected, .connecting: return true
        case .disconnected, .failed: return false
        }
    }

    private func teardown(_ id: UUID) {
        timeouts[id]?.cancel()
        timeouts.removeValue(forKey: id)
        subscriptions.removeValue(forKey: id)
        clients[id]?.disconnect()
        clients.removeValue(forKey: id)
    }

    /// 探测身份：主身份加一个稳定的短后缀。
    ///
    /// 不能直接复用主 device_id，原因见类型注释。后缀取通道 id 前 8 位——
    /// 同一台手机重启后仍然稳定，也不会与人手输入的 id 撞车。
    private func probeId(_ base: String, _ channelId: UUID) -> String {
        "\(base)-ch\(channelId.uuidString.prefix(8).lowercased())"
    }
}
