import SwiftUI

struct ContentView: View {
    @EnvironmentObject var appState: AppState
    @StateObject private var signaling = SignalingClient()
    @StateObject private var rtcSession = WebRTCSession()
    @StateObject private var discovery = BonjourDiscovery()
    @StateObject private var store = SavedComputersStore()

    /// 应用前后台状态：切后台时启动保活，回前台时检查并恢复会话
    @Environment(\.scenePhase) private var scenePhase

    @State private var showRemoteScreen = false
    @State private var showAddSheet = false
    /// 设置页（信令接入密钥等）
    @State private var showSettings = false
    /// 回前台时信令若已断，先等它重连成功再重建 WebRTC。
    /// 顺序不能反：Connect 消息要靠信令那条 WebSocket 发出去，
    /// 信令没通就发，重建请求会被静默丢掉——表现就是回到前台一直黑屏。
    @State private var pendingRTCReconnect = false
    /// 待进入的电脑。连接是异步的（要等信令把设备列表发回来），
    /// 所以先把意图记在这里，等设备出现后再真正进入。
    @State private var pendingJoin: PendingJoin?
    /// 点完之后等设备列表的超时任务。
    ///
    /// 没有它的话「点了没反应」会无限期持续：可能连上了但列表里没有目标设备
    /// （被控端没启动、密钥不符被驳回），也可能压根连不上服务器。这些情况
    /// 界面上都是安静地什么都没发生，用户只能反复点、自己猜。
    @State private var joinTimeout: Task<Void, Never>?

    private enum PendingJoin: Equatable {
        /// 直连某一台确定的电脑（局域网发现，连上就是它）
        case specific(String)
        /// 连上后进入列表里的第一台（中心服务器场景，地址本身不代表某台电脑）
        case firstAvailable
    }

    var body: some View {
        NavigationStack {
            List {
                brandHeader
                discoverySection
                savedSection
                if !signaling.devices.isEmpty { onlineSection }
            }
            .listStyle(.insetGrouped)
            // 下拉刷新：一次把「同一 Wi-Fi 下」与「外面的网络也能连」两块都重新取
            .refreshable { await refreshHome() }
            // 隐藏系统列表底与导航栏底，让暖色纸背景整屏透出来。
            // 不改的话，这块会是系统白（浅色模式）——就是「整个背景是白色」的来源。
            .scrollContentBackground(.hidden)
            .background(Brand.paper.ignoresSafeArea())
            // 纸底是浅色，界面文字用的是深棕（Brand.ink）。必须把外观锁成浅色：
            // 否则系统处于深色模式时 .primary 变白字，又回到「浅底白字」的坑。
            .environment(\.colorScheme, .light)
            // 导航栏同步透明：状态徽标与加号直接落在纸面上，不留一条突兀的白带
            .toolbarBackground(.hidden, for: .navigationBar)
            // 品牌头部已表明这是 Makuris，标题留空把纵向空间让给它，
            // 页面归属由分组标题（同一 Wi-Fi 下 / 已保存）自己说清楚。
            .navigationTitle("")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) { statusBadge }
                ToolbarItem(placement: .topBarTrailing) {
                    HStack(spacing: 16) {
                        Button {
                            showSettings = true
                        } label: {
                            Image(systemName: "gearshape")
                                .foregroundColor(Brand.ink.opacity(0.75))
                        }
                        .accessibilityLabel("设置")

                        Button {
                            showAddSheet = true
                        } label: {
                            Image(systemName: "plus")
                                .foregroundColor(Brand.accentDeep)
                        }
                        .accessibilityLabel("添加电脑")
                    }
                }
            }
            .sheet(isPresented: $showAddSheet) {
                AddComputerView(store: store)
            }
            .sheet(isPresented: $showSettings) {
                SettingsView(appState: appState)
            }
            .onAppear {
                // 局域网发现：打开就开始找，用户不需要做任何事
                discovery.start()
            }
            .onChange(of: scenePhase) { phase in
                handleScenePhase(phase)
            }
            .onChange(of: showRemoteScreen) { shown in
                // 退出远程画面后不该再留「待恢复」的意图
                if !shown { pendingRTCReconnect = false }
            }
            // 用 onReceive 订阅 @Published 本体，而不是用 onChange 检测派生值。
            //
            // 为什么必须换：等设备一步靠的是「值变了没有」。刷新设备列表的做法是
            // 先清空、再请求（清空是为了让复用连接时也能触发），可局域网下一个
            // 往返只要一两毫秒，空列表与回填后的列表极可能落在同一次界面渲染之前，
            // onChange 看到的前后值一模一样（都是 [host-mac-01]），于是不触发，
            // 后面 resolvePendingJoin 永不执行。表现就是「第一次点能进，从远程
            // 界面返回后再点同一台就毫无反应」，而日志里注册与设备列表都正常。
            // @Published 每次赋值都发布，与新旧值是否相同无关，从根上避开这个坑。
            //
            // 延后一轮主队列再读：@Published 是在 willSet 阶段发布的，发布时
            // signaling.devices 还是旧值，立刻读会拿到清空前的列表。
            .onReceive(signaling.$devices) { _ in
                DispatchQueue.main.async { resolvePendingJoin() }
            }
            .onChange(of: signaling.state) { _ in
                resolvePendingJoin()
                // 信令刚恢复：这时才发得出 Connect，可以重建 WebRTC 了
                if signaling.state.isConnected && pendingRTCReconnect {
                    pendingRTCReconnect = false
                    rtcSession.reconnectIfNeeded()
                }
            }
            // 等待反馈统一由远程界面那层遮罩承担，主页不再叠一层。
            //
            // 「等设备列表」与「建视频通道」本来就是同一次连接的前后两段，
            // 拆到两个界面就会出现两个转圈接连弹出，用户会当成两次不同的
            // 等待——实际是一次连接走完了两步而已。
            .fullScreenCover(isPresented: $showRemoteScreen, onDismiss: {
                // 退出即撤销本次加入意图。不清的话，残留的 pendingJoin
                // 会让下次进主页时遮罩又冒一次。
                pendingJoin = nil
                joinTimeout?.cancel()
                joinTimeout = nil
            }) {
                RemoteScreenContainer(
                    session: rtcSession,
                    signaling: signaling,
                    isPresented: $showRemoteScreen
                ) {
                    signaling.requestDeviceList()
                }
            }
        }
    }

    // MARK: - 分区

    /// 同一 Wi-Fi 下自动发现的电脑（零配置，主路径）
    @ViewBuilder
    private var discoverySection: some View {
        Section {
            if discovery.permissionDenied {
                permissionHint
            } else if discovery.computers.isEmpty {
                HStack(spacing: 10) {
                    ProgressView()
                    Text(discovery.isReady ? "正在查找附近的电脑…" : "正在启动…")
                        .font(.footnote)
                        .foregroundColor(Brand.ink.opacity(0.6))
                }
                .padding(.vertical, 4)
                .listRowBackground(Brand.cardPaper)
            } else {
                ForEach(discovery.computers) { computer in
                    Button {
                        joinDiscovered(computer)
                    } label: {
                        computerRow(
                            title: computer.serviceName,
                            subtitle: computer.subtitle,
                            icon: "desktopcomputer")
                    }
                    .buttonStyle(.plain)
                }
            }
        } header: {
            Text("同一 Wi-Fi 下")
                .foregroundColor(Brand.ink.opacity(0.5))
        } footer: {
            if !discovery.permissionDenied && !discovery.computers.isEmpty {
                Text("打开被控电脑上的 Makuris 即会自动出现在这里。")
                    .foregroundColor(Brand.ink.opacity(0.48))
            }
        }
    }

    /// 手动保存的电脑（跨网络/公网场景的兜底，可增删）
    @ViewBuilder
    private var savedSection: some View {
        Section {
            if store.computers.isEmpty {
                Text("还没有保存的电脑")
                    .font(.footnote)
                    .foregroundColor(Brand.ink.opacity(0.55))
                    .listRowBackground(Brand.cardPaper)
            } else {
                ForEach(store.computers) { computer in
                    Button {
                        joinSaved(computer)
                    } label: {
                        computerRow(
                            title: computer.displayName,
                            subtitle: computer.address,
                            icon: "externaldrive.connected.to.line.below")
                    }
                    .buttonStyle(.plain)
                }
                .onDelete { indexSet in
                    for idx in indexSet {
                        store.remove(store.computers[idx])
                    }
                }
            }
        } header: {
            Text("已保存")
                .foregroundColor(Brand.ink.opacity(0.5))
        } footer: {
            Text("用于不在同一 Wi-Fi 的电脑，点右上角加号添加。左滑可删除。")
                .foregroundColor(Brand.ink.opacity(0.48))
        }
    }

    /// 连上信令服务器后实际在线的电脑。
    ///
    /// 它和上面「同一 Wi-Fi 下」列的是同一类东西——都是电脑，不是另一种设备。
    /// 差别只在走哪条路：上面那条是局域网直连（Bonjour 发现，快到毫秒级），
    /// 这条是经信令服务器中转牵线，手机在外面（蜂窝网/别的 Wi-Fi）时只有它能用。
    /// 所以同一台电脑在两条都通时会各出现一次，这是正常的，不是两台机器。
    @ViewBuilder
    private var onlineSection: some View {
        Section {
            ForEach(signaling.devices) { device in
                Button {
                    enter(deviceId: device.device_id)
                } label: {
                    computerRow(
                        title: device.device_id,
                        subtitle: device.platform ?? "未知系统",
                        icon: "bolt.horizontal.circle")
                }
                .buttonStyle(.plain)
            }
        } header: {
            Text("外面的网络也能连")
                .foregroundColor(Brand.ink.opacity(0.5))
        } footer: {
            Text("这些是连在同一信令服务器上的电脑。和上面「同一 Wi-Fi 下」可能是同一台——这里走服务器中转，不在家时用得到。")
                .foregroundColor(Brand.ink.opacity(0.48))
        }
    }

    private var permissionHint: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("无法搜索局域网")
                .font(.subheadline).bold()
            Text("请在「设置 → 隐私与安全性 → 本地网络」中允许 Makuris 访问，"
                 + "或改用「已保存」里的地址手动连接。")
                .font(.footnote)
                .foregroundColor(Brand.ink.opacity(0.62))
        }
        .padding(.vertical, 2)
        .listRowBackground(Brand.cardPaper)
    }

    private func computerRow(title: String, subtitle: String, icon: String) -> some View {
        HStack(spacing: 14) {
            // 淡琥珀圆底衬图标：手绘风不用系统那种实心色块，圆底与 logo 的信号弧同色
            ZStack {
                Circle()
                    .fill(Brand.accent.opacity(0.18))
                    .frame(width: 38, height: 38)
                Image(systemName: icon)
                    .font(.system(size: 18, weight: .medium))
                    .foregroundColor(Brand.accentDeep)
            }
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .font(.headline)
                    .foregroundColor(Brand.ink)
                Text(subtitle)
                    .font(.caption)
                    .foregroundColor(Brand.ink.opacity(0.55))
            }
            Spacer()
            Image(systemName: "chevron.right")
                .font(.caption.weight(.semibold))
                .foregroundColor(Brand.ink.opacity(0.32))
        }
        .padding(.vertical, 6)
        .contentShape(Rectangle())
        // 暖纸白行底：insetGrouped 默认行底是纯白，搁在暖色纸背景上
        // 就是一块块突兀的白，所以每行都得跟着换
        .listRowBackground(Brand.cardPaper)
    }

    @ViewBuilder
    private var statusBadge: some View {
        HStack(spacing: 6) {
            Circle()
                .fill(statusColor)
                .frame(width: 8, height: 8)
            Text(statusText)
                .font(.caption2)
                .foregroundColor(Brand.ink.opacity(0.6))
        }
    }

    private var statusColor: Color {
        switch signaling.state {
        case .connected: return .green
        case .connecting: return .orange
        case .failed: return .red
        case .disconnected: return .gray
        }
    }

    private var statusText: String {
        switch signaling.state {
        case .connected: return "在线"
        case .connecting: return "连接中"
        case .failed(let reason): return reason
        case .disconnected: return "未连接"
        }
    }

    // MARK: - 下拉刷新

    /// 下拉刷新：重扫局域网 + 重取在线设备。
    ///
    /// 只刷「会变」的两块。「已保存的」是本地数据，刷新不会让它变——刷它等于
    /// 让用户做了个动作却不可能产生任何结果，反倒让人怀疑刷新没生效。
    ///
    /// 为什么必须兜底等一秒：局域网一个来回只要几毫秒，刷新函数会立刻返回，
    /// 系统的下拉转圈一闪而过（有时根本看不见），用户判定「没刷成功」就会
    /// 反复下拉。这里等的是观感，不是数据——两次刷新在这一秒内早已发出去，
    /// 新结果由各自的回调推回界面。
    ///
    /// 为什么不提示成功/失败：刷新结果在界面上本来就自明（列表变了没有、
    /// 状态徽标是绿是灰）。从没连过信令服务器时也只刷局域网、不弹提示——
    /// 那条路径下「外面的网络也能连」本来就该是空的，弹一句话反而像出了错。
    @MainActor
    private func refreshHome() async {
        // ① 局域网：重建浏览器，强制重新查询一次
        discovery.restart()

        // ② 在线设备：连着就重新要一份列表；断开但之前连过就顺手重连——
        // 重连成功后服务器会把设备列表一起推回来，不必再要一次。
        // 从没连过（没有上次地址）时什么都不做：没有可连的地址。
        if signaling.state.isConnected {
            signaling.refreshDeviceList()
        } else {
            signaling.reconnectIfNeeded()
        }

        try? await Task.sleep(nanoseconds: 1_000_000_000)
    }

    // MARK: - 连接

    /// 点局域网里发现的电脑：它自己就是信令端点，连上即进入
    private func joinDiscovered(_ computer: DiscoveredComputer) {
        let url = computer.normalizedURL
        guard !url.isEmpty else { return }
        pendingJoin = .specific(computer.deviceId)
        // 局域网直连（Bonjour 发现）不过中心信令服务器，用不到接入密钥；
        // 这里清空避免把服务器密钥带到直连端点上——那台电脑的日志里
        // 就会出现一串与本机无关的密钥，属于无谓的凭据扩散。
        signaling.accessKey = ""
        connectOrReuse(url: url)
        beginJoinWait()
        // 立刻进远程界面。等待反馈全部由那一层遮罩承担——「等设备列表」
        // 与「建视频通道」是同一次连接的两段，分在两个界面就会出现两个
        // 转圈接连弹出，用户会当成两次等待，体感极差。
        showRemoteScreen = true
    }

    /// 点已保存的电脑：地址可能指向中心服务器（下面挂多台），
    /// 因此连上后进入第一台在线设备
    private func joinSaved(_ computer: SavedComputer) {
        let url = computer.normalizedURL
        // 地址解析不出来时也要说话。原先这里静默 return，用户点了毫无反应，
        // 而原因（保存的地址格式不对）就藏在那条被吞掉的返回值里。
        guard !url.isEmpty else {
            signaling.reportInvalidAddress(computer.address)
            return
        }
        pendingJoin = .firstAvailable
        // 走中心服务器才需要接入密钥。没填也能连，只是服务器若启用了鉴权
        // 会把注册驳回，届时界面上是「一直找不到电脑」。
        signaling.accessKey = appState.accessKey
        connectOrReuse(url: url)
        beginJoinWait()
        // 与局域网路径同理：立刻进远程界面，等待反馈只保留那一层遮罩
        showRemoteScreen = true
    }

    /// 连接信令服务器；若与同一地址的连接还活着，就直接复用、不重连。
    ///
    /// 关键在于「不重连」：从远程界面返回首页时只断了 WebRTC 会话，信令连接
    /// 还活着（心跳在跑、注册状态也在）。这时若照旧调一次 connect()，就会把
    /// 一条健康的 WebSocket 拆掉重建——被取消的旧 socket 其失败回调随后到达，
    /// 被写成「连接失败」，新 socket 再连上。用户看到的就是「先报一次连接失败，
    /// 然后才开始重连」，既困惑又像是哪里坏了。
    private func connectOrReuse(url: String) {
        if signaling.hasLiveConnection(to: url) {
            print("[join] 复用已有信令连接：\(url)")
            // 列表必须重新要一次：那台电脑可能已不在线、也可能多了几台；
            // 而且刷新会先清空再请求，界面等设备的那段逻辑才会被触发
            //（列表内容与上次完全相同的话，onChange 检测不到变化就不执行）。
            signaling.refreshDeviceList()
        } else {
            signaling.connect(url: url, deviceId: appState.deviceId, role: .viewer)
        }
    }

    /// 开始等待设备列表，超时后清掉等待意图。
    ///
    /// 25 秒这个值是被公网逼出来的：原先取 8 秒，那时等待期没有任何界面反馈，
    /// 短一点好歹能早点告诉用户「没成」。现在等待期有了连接遮罩（会分阶段
    /// 显示「正在连接服务器…」「正在等待电脑响应…」并有转圈），久等不再等于
    /// 卡死，超时就该按真实链路耗时来定——公网首次连接要走隧道握手 + 打洞，
    /// 十几秒是常事，8 秒会把人从正在正常推进的连接里踢出去。
    ///
    /// 这里不再自己报错：失败原因统一由遮罩下的 connectFailure 呈现，
    /// 两处都写会出现「遮罩还在转、底下已经报错」的矛盾画面。
    private func beginJoinWait() {
        joinTimeout?.cancel()
        joinTimeout = Task { @MainActor in
            try? await Task.sleep(nanoseconds: 25_000_000_000)
            guard !Task.isCancelled, pendingJoin != nil else { return }
            pendingJoin = nil
            signaling.reportJoinTimeout()
        }
    }

    /// 设备列表回来后再真正进入远程画面
    private func resolvePendingJoin() {
        guard let pending = pendingJoin else { return }
        switch pending {
        case .specific(let id):
            guard signaling.devices.contains(where: { $0.device_id == id }) else { return }
            pendingJoin = nil
            enter(deviceId: id)
        case .firstAvailable:
            guard let first = signaling.devices.first else { return }
            pendingJoin = nil
            enter(deviceId: first.device_id)
        }
    }

    private func enter(deviceId: String) {
        rtcSession.connectAsViewer(signaling: signaling, to: deviceId)
        showRemoteScreen = true
    }

    // MARK: - 前后台切换

    /// 进后台启动保活，回前台恢复会话。
    ///
    /// 两件事都要做：保活让连接在后台「大概率不断」，但 iOS 在内存紧张时
    /// 仍可能回收进程、网络切换时也会掐断 socket。只做保活的话，一旦真断了
    /// 用户切回来就是一屏黑，只能退出重进；所以回前台再补一次自愈检查。
    private func handleScenePhase(_ phase: ScenePhase) {
        switch phase {
        case .background:
            // 只有正在远程会话时才需要保活，否则白耗电
            if showRemoteScreen {
                BackgroundKeepAlive.shared.start()
            }
        case .active:
            BackgroundKeepAlive.shared.stop()
            restoreSession()
        case .inactive:
            break
        @unknown default:
            break
        }
    }

    /// 回到前台后检查会话是否还活着，断了就重建。
    /// 幂等：连接正常时两个 reconnectIfNeeded() 都直接返回，不会白拆连接。
    private func restoreSession() {
        guard showRemoteScreen else { return }
        if signaling.state.isConnected {
            rtcSession.reconnectIfNeeded()
        } else {
            pendingRTCReconnect = true
            signaling.reconnectIfNeeded()
        }
    }
}

/// 设置页。
///
/// 目前只有一项：信令服务器的接入密钥。之所以值得单独开一页而不是塞进
/// 「添加电脑」表单——它是服务器级的、一次配置全体生效，塞进每台电脑的
/// 表单里会让人以为每台都要填一遍，反复填写又容易填错。
///
/// 局域网直连（同一 Wi-Fi 下自动发现的那台电脑）不走信令服务器，
/// 完全用不到这个值，所以页面上要写清楚，免得用户以为不填就不能用。
struct SettingsView: View {
    @ObservedObject var appState: AppState
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("留空表示服务器未启用鉴权", text: $appState.accessKey)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .font(.system(.body, design: .monospaced))
                } header: {
                    Text("信令接入密钥")
                } footer: {
                    Text("只有通过中心服务器连接时才需要。密钥在电脑端启动信令服务器时"
                         + "打印在日志里，两端填同一串。\n"
                         + "同一 Wi-Fi 下直接发现的电脑不走服务器，无需填写。")
                }

                Section {
                    LabeledContent("本机标识", value: appState.deviceId)
                        .font(.footnote)
                } footer: {
                    Text("电脑端会看到这个标识，用于区分是哪台手机在连接。")
                }
            }
            .scrollContentBackground(.hidden)
            .background(Brand.paper.ignoresSafeArea())
            .environment(\.colorScheme, .light)
            .toolbarBackground(.hidden, for: .navigationBar)
            .navigationTitle("设置")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("完成") { dismiss() }
                }
            }
        }
    }
}

/// 添加一台电脑（名称 + 地址）
struct AddComputerView: View {
    @ObservedObject var store: SavedComputersStore
    @Environment(\.dismiss) private var dismiss

    @State private var name = ""
    @State private var address = ""

    var body: some View {
        NavigationStack {
            Form {
                Section("名称") {
                    TextField("家里的 Mac", text: $name)
                }
                Section {
                    TextField("192.168.1.3", text: $address)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.URL)
                        .font(.system(.body, design: .monospaced))
                } header: {
                    Text("地址")
                } footer: {
                    Text("只填 IP 即可，端口与路径会自动补全。"
                         + "不在同一 Wi-Fi 时填这台电脑可被访问的地址。")
                }
                Section {
                    LabeledContent("将连接", value: previewAddress)
                        .font(.footnote)
                }
            }
            // 这一页同样用暖色纸底，跟主界面成套；锁浅色外观避免深色模式下白字
            .scrollContentBackground(.hidden)
            .background(Brand.paper.ignoresSafeArea())
            .environment(\.colorScheme, .light)
            .toolbarBackground(.hidden, for: .navigationBar)
            .navigationTitle("添加电脑")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("取消") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("保存") {
                        store.add(name: name, address: address)
                        dismiss()
                    }
                    .disabled(address.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            }
        }
    }

    private var previewAddress: String {
        let result = AppState.normalize(address)
        return result.isEmpty ? "地址无效" : result
    }
}

/// 远程屏幕容器（全屏 + 退出按钮 + 键盘开关）
struct RemoteScreenContainer: View {
    @ObservedObject var session: WebRTCSession
    @ObservedObject var signaling: SignalingClient
    @Binding var isPresented: Bool
    /// 退出回调：用于让主界面刷新设备列表
    var onExit: (() -> Void)?

    /// 待输入的配对请求。被控端发现令牌不认时先发挑战，用户得在这里把码填上。
    ///
    /// 这个浮层必须挂在远程画面容器里，不能挂在主界面上：配对挑战是在
    /// 「已经进入远程画面」之后才到的，而那时全屏画面（fullScreenCover）
    /// 已经把主界面整块盖住，挂在主界面的弹窗根本显示不出来——用户只会
    /// 看到一块永远黑着的屏，无从知道还需要配对。
    @State private var pairingRequest: PairingRequest?
    @State private var pairingCode = ""
    @State private var pairingError: String?
    @State private var pairingSubmitting = false

    /// 一次配对请求：记下是哪台 host、以及它给的挑战随机数。
    private struct PairingRequest: Equatable {
        let hostId: String
        let nonce: String
    }
    /// 键盘由画面视图内部直接唤起，这里只持有控制器用于按钮触达
    @StateObject private var keyboard = RemoteKeyboardController()
    /// 画面几何信息（尺寸/宽高比/安全区/键盘高度），由 UIKit 侧回传
    @StateObject private var metrics = RemoteScreenMetrics()
    /// 连接超时计时任务
    @State private var connectDeadline: Task<Void, Never>?
    /// 是否已超时（区分「还在连」与「连不上」，两者的界面表现必须不同）
    @State private var connectTimedOut = false

    /// 状态条手动拖动的位移（全局坐标偏移）。
    /// 初值仍由 StatusBarLayout 按黑边自动摆位，这里是叠加在上面的用户位移。
    @State private var statusBarDrag: CGSize = .zero
    /// 起拖瞬间的位移基准：DragGesture 的 translation 相对起点计算，
    /// 每次 onChanged 必须基于基准重算，否则旧位移会被反复累加、越拖越快。
    @State private var statusBarDragBase: CGSize = .zero
    /// 卡片实测尺寸（含内边距的完整尺寸），钳制拖动边界用。
    /// 写死估算值会在横竖屏、紧凑版切换时把边界算错。
    @State private var statusBarSize: CGSize = .zero
    /// 是否正在拖卡片：起拖第一帧把基准锁成当前位移，
    /// DragGesture 的 translation 都相对基准算，onChanged 里才不会累加。
    @State private var barDragging = false

    /// 把状态条中心钳回屏幕内：卡片不许被拖出边缘（留 6pt 呼吸）；
    /// 极端情况（卡片比屏幕还宽）退化为屏幕中心，保证钳制区间恒有效。
    private func clampBarCenter(_ value: CGFloat, half: CGFloat, span: CGFloat) -> CGFloat {
        let lo = min(half + 6, span / 2)
        let hi = max(span - half - 6, span / 2)
        // 不用 .clamped(to:)：那是别处的 package 级扩展，本 target 访问不到
        return Swift.max(lo, Swift.min(value, hi))
    }

    /// 状态条摆位：按当前几何算出放哪、多大
    /// （几何全部用全局坐标，与画面视图同一坐标系）
    private func layout(in geo: GeometryProxy) -> StatusBarLayout {
        // 安全区取 UIKit 侧回传的真实值：geom 的 safeAreaInsets 在
        // ignoresSafeArea 下恒为 0，用它会导致状态条压住 Home indicator
        let insets = metrics.safeInsets
        let safe = EdgeInsets(top: insets.top, leading: insets.left,
                              bottom: insets.bottom, trailing: insets.right)
        let l = StatusBarLayout.compute(
            size: geo.size,
            safeInsets: safe,
            videoAspect: metrics.videoAspect,
            keyboardHeight: geometryKeyboardHeight,
            zoomScale: metrics.zoomScale
        )
        return l
    }

    /// 键盘高度统一取父视图实测值：它不是从画面视图自身底边量的，
    /// 而是把画面容器（kv.keyboardFrame）换算到本视图坐标系得到的，
    /// 因此这个坐标系与 geo 一致，可直接参与摆位计算。
    private var geometryKeyboardHeight: CGFloat {
        metrics.keyboardHeight
    }

    var body: some View {
        GeometryReader { geo in
            let l = layout(in: geo)
            ZStack {
                // 画面铺满整个屏幕（含安全区），黑边由 scaleAspectFit 自己留
                RemoteScreenView(session: session, keyboard: keyboard, metrics: metrics)
                    .ignoresSafeArea()

                // 页面背景压在画面「之上」：它的暖色只在 letterbox 上出现
                // （画面那块被 even-odd 挖空了），故不遮视频一丝。
                // 之所以不能放在画面下层——RTCMTLVideoView 内部是 MTKView，
                // 它自己的 drawable 会把 letterbox 清成不透明黑，下层什么都看不见。
                // must 关掉命中测试：它盖在画面上，开着就会把触摸手势全吞掉，
                // 鼠标、缩放、拖拽立刻失效。
                RemoteBackdrop(videoAspect: metrics.videoAspect,
                               zoomScale: metrics.zoomScale,
                               transformedVideoRect: metrics.transformedVideoRect)
                    .ignoresSafeArea()
                    .allowsHitTesting(false)

                // 状态条用绝对定位落位：center 已经算好在全局坐标里的位置，
                // 不依赖任何容器的对齐规则，也就不会再出现「以为在贴边、
                // 实际压在画面中间」这种事。
                //
                // 可拖拽：初值仍是黑边自动摆位（layout 算出的 center），
                // 用户拖动只是在其上叠加一个位移 statusBarDrag。
                // 这里不用 .offset 而继续用 .position，是为了让 .position
                // 报告的尺寸就是卡片真实尺寸，钳制计算才准确（offset 不改变
                // position 坐标系里的落点语义，两者混用容易算错边界）。
                // 卡片本体不接收画面手势，单指拖它不会与双指缩放/平移冲突。
                SessionStatusBar(
                    session: session,
                    keyboard: keyboard,
                    vertical: l.vertical,
                    overlay: l.overlay,
                    compact: l.compact,
                    layoutSize: l.size,
                    onHome: leave
                )
                // 竖向按内容理想高度（不被压扁），横向允许压缩——
                // 横屏黑边只有 76pt 宽，用 .fixedSize() 会让文字拒绝收缩、
                // 直接溢出状态条。
                .fixedSize(horizontal: false, vertical: true)
                // 实测卡片尺寸：钳制边界必须用真实大小，估算值在
                // 横竖屏/紧凑版切换时会算错，把卡片钳出屏幕或卡住不动
                .background(GeometryReader { g in
                    Color.clear
                        .onAppear { statusBarSize = g.size }
                        .onChange(of: g.size) { statusBarSize = $0 }
                })
                .position(
                    x: clampBarCenter(l.center.x + statusBarDrag.width,
                                      half: statusBarSize.width / 2,
                                      span: geo.size.width),
                    y: clampBarCenter(l.center.y + statusBarDrag.height,
                                      half: statusBarSize.height / 2,
                                      span: geo.size.height)
                )
                .gesture(
                    DragGesture(minimumDistance: 6)
                        .onChanged { v in
                            // 起拖第一帧锁定基准；之后每帧都从「自动摆位点 +
                            // 基准位移 + 本次 translation」重算候选位置。
                            // 不基于基准重算的话，translation 会被逐帧累加，
                            // 卡片越拖越快、直接飞出去。
                            if !barDragging {
                                barDragging = true
                                statusBarDragBase = statusBarDrag
                            }
                            let candX = l.center.x + statusBarDragBase.width + v.translation.width
                            let candY = l.center.y + statusBarDragBase.height + v.translation.height
                            let nx = clampBarCenter(candX,
                                                    half: statusBarSize.width / 2,
                                                    span: geo.size.width)
                            let ny = clampBarCenter(candY,
                                                    half: statusBarSize.height / 2,
                                                    span: geo.size.height)
                            statusBarDrag = CGSize(width: nx - l.center.x,
                                                   height: ny - l.center.y)
                        }
                        .onEnded { _ in
                            barDragging = false
                            statusBarDragBase = statusBarDrag
                            UIImpactFeedbackGenerator(style: .light).impactOccurred()
                        }
                )

                // 配对码输入：被控端要求配对时盖在画面上。
                // 必须是「盖」而不是「跳去另一页」——配对完成后会话要接着走，
                // 跳页会把刚建立的协商状态打断，用户还得再点一次进入。
                if let req = pairingRequest {
                    PairingPrompt(
                        hostId: req.hostId,
                        code: $pairingCode,
                        error: pairingError,
                        submitting: pairingSubmitting,
                        onCancel: {
                            pairingRequest = nil
                            pairingCode = ""
                            pairingError = nil
                            leave()
                        },
                        onSubmit: { submitPairing(for: req) }
                    )
                }

                // 连接遮罩：真正出画之前一直盖着。
                //
                // 判定用「渲染器已接上轨道 + 首帧已到」两个条件，而不是只看
                // 首帧标记。切线路时曾只认首帧，而解码统计一路上涨、画面却没有：
                // 渲染器还没接上轨道，遮罩一撤，用户看到的就是一片空白，
                // 比一直转圈更让人无从判断。渲染器接上是出画的硬前提。
                //
                // 配对面板出现时让位：那一步用户要输码，挡住就没法操作了。
                if !(session.rendererAttached && session.firstFrameArrived)
                    && pairingRequest == nil {
                    ConnectOverlay(
                        message: connectMessage,
                        failure: connectFailure,
                        onCancel: leave
                    )
                    .transition(.opacity)
                }
            }
        }
        .ignoresSafeArea()
        .onAppear {
            wirePairingCallbacks()
            startConnectWatch()
        }
        // 出画（渲染器接上轨道且首帧到达）即撤遮罩，并停掉超时计时。
        // 两处都订阅：只订阅首帧的话，渲染器接上后若首帧回调因尺寸未变而缺席，
        // 遮罩还会白等；只订阅渲染器则会在真正出画前提前撤掉。
        .onChange(of: session.firstFrameArrived) { _ in
            if session.rendererAttached && session.firstFrameArrived {
                connectDeadline?.cancel()
            }
        }
        .onChange(of: session.rendererAttached) { _ in
            if session.rendererAttached && session.firstFrameArrived {
                connectDeadline?.cancel()
            }
        }
    }

    /// 当前连接阶段的说明文字。
    ///
    /// 分阶段说出来，是因为这句文案正是「慢」与「卡死」的分界：公网上
    /// 每一步都可能耗上几秒，只显示一个转圈的话，用户无法判断该继续等
    /// 还是该重来。
    private var connectMessage: String {
        switch signaling.state {
        case .connecting:
            return "正在连接服务器…"
        case .disconnected:
            return "正在连接…"
        case .failed:
            return "连接未能建立"
        case .connected:
            if signaling.devices.isEmpty {
                return "正在等待电脑响应…"
            }
            // 成员名是 session：这个容器持有的是外部传入的共享 WebRTCSession，
            // 与主界面里那个 @StateObject rtcSession 是同一个对象的不同叫法
            if session.connectionState == .connected {
                return "正在接收画面…"
            }
            return "正在建立视频通道…"
        }
    }

    /// 失败原因，nil 表示仍在推进中。
    private var connectFailure: String? {
        // 超时优先于具体错误：走到这一步通常没有任何错误可归因
        // （信令可能连上了、会话也在建），只说「超时」最贴近事实
        if connectTimedOut {
            return "连接超时。请确认电脑已开机并运行 Makuris，且网络可达。"
        }
        if case .failed(let reason) = signaling.state {
            return reason
        }
        return nil
    }

    /// 开始连接超时计时。
    ///
    /// 30 秒是给公网首次连接的宽裕上限：正常两三秒就该出画，30 秒还没出，
    /// 基本可判定这次连不上。没有这道兜底，遮罩会一直转下去——比原来的
    /// 「白屏」更糟的是，它看起来像是在正常工作。
    private func startConnectWatch() {
        connectDeadline?.cancel()
        connectTimedOut = false
        // 这里不再把 firstFrameArrived 归零。
        //
        // 归零的本意是「新一次尝试要有反馈」，但它在时序上不安全：本函数在
        // 远程界面出现时执行，而切换线路时连接极快，首帧很可能已经先到了。
        // 归零就把到手的首帧抹掉，而尺寸不再变化便没有第二次回调，遮罩反而
        // 永远撤不掉——正是「卡在建立视频通道」的样子。清零已由 connectAsViewer
        // 负责：那才是每次新建会话的唯一入口，且一定早于协商开始。
        connectDeadline = Task { @MainActor in
            try? await Task.sleep(nanoseconds: 30_000_000_000)
            guard !Task.isCancelled else { return }
            if !(session.rendererAttached && session.firstFrameArrived) { connectTimedOut = true }
        }
    }

    /// 接上信令的配对回调。
    ///
    /// 放在 onAppear 里而不是 init：signaling 是外部传进来的共享实例，
    /// 在 init 里赋值会被后续创建覆盖掉，回调就静默失效了。
    private func wirePairingCallbacks() {
        signaling.onAuthRequired = { hostId, nonce in
            pairingRequest = PairingRequest(hostId: hostId, nonce: nonce)
            pairingCode = ""
            pairingError = nil
            pairingSubmitting = false
        }
        signaling.onAuthDenied = { _, reason in
            // 只清输入与提交态，不关掉面板：失败是常态（输错一位很常见），
            // 关掉后用户得从头再来一遍，体验上像是「卡住了」。
            pairingSubmitting = false
            pairingError = reason
        }
        signaling.onAuthGranted = { _ in
            pairingRequest = nil
            pairingCode = ""
            pairingError = nil
            pairingSubmitting = false
        }
    }

    /// 提交配对码。
    ///
    /// 提交期间禁止重复点：被控端对失败次数会翻倍锁定（5 次起锁 30 秒），
    /// 网络慢时用户很容易连点几下，把剩余尝试次数白白耗光。
    private func submitPairing(for req: PairingRequest) {
        guard !pairingSubmitting else { return }
        pairingError = nil
        pairingSubmitting = true
        if !signaling.submitPairingCode(pairingCode, for: req.hostId) {
            pairingSubmitting = false
            pairingError = "配对会话已失效，请重新连接"
        }
    }

    /// 返回主页：断开会话、关掉全屏画面、让主界面刷新设备列表
    private func leave() {
        session.disconnect()
        isPresented = false
        onExit?()
    }
}

#Preview {
    ContentView()
        .environmentObject(AppState())
}

/// 连接进度遮罩。
///
/// 盖在远程画面上，直到远端首帧真的到达才撤掉。公网首次连接要走六七步：
/// 连信令服务器 → 注册 → 等被控端回设备列表 → 建 WebRTC → ICE 打洞 →
/// 首帧解码上屏。全程两三秒起步，慢的时候十几秒。原先这段时间直接把人
/// 丢进远程界面，那里除了黑边什么都没有，看着就是卡死了。
///
/// 为什么不改成「独立的连接页、连上再跳进去」：远程界面的视频渲染器是在
/// 它自己出现时才注入并挂载轨道的（历史上踩过「先建会话、后挂载渲染器
/// → 永久白屏」的坑）。跳页必须动这条时序，风险高而收益为零——遮罩盖着
/// 时用户看到的就是一个连接页，连上后淡出即见画面。
struct ConnectOverlay: View {
    /// 当前阶段的说明（「正在连接服务器…」等）
    let message: String
    /// 失败原因；nil 表示仍在推进中
    let failure: String?
    var onCancel: () -> Void

    /// 是否处于失败态。失败时不能再转圈——转圈传达的是「还在努力」，
    /// 而这时其实已经停了，继续转会让人一直等下去。
    private var failed: Bool { failure != nil }

    var body: some View {
        ZStack {
            Brand.paper.ignoresSafeArea()

            VStack(spacing: 22) {
                MakurisLogo(side: 78)

                if failed {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .font(.system(size: 30))
                        .foregroundColor(Brand.accentDeep)
                } else {
                    ProgressView()
                        .progressViewStyle(.circular)
                        .tint(Brand.accentDeep)
                        .scaleEffect(1.15)
                }

                VStack(spacing: 9) {
                    Text(failed ? "连接失败" : message)
                        .font(.system(size: 17, weight: .semibold, design: .rounded))
                        .foregroundColor(Brand.ink)

                    if let failure {
                        Text(failure)
                            .font(.footnote)
                            .foregroundColor(Brand.ink.opacity(0.62))
                            .multilineTextAlignment(.center)
                            .padding(.horizontal, 34)
                    } else {
                        Text("首次连接需要几秒钟")
                            .font(.footnote)
                            .foregroundColor(Brand.ink.opacity(0.5))
                    }
                }

                Button(action: onCancel) {
                    Text(failed ? "返回主页" : "取消")
                        .font(.system(size: 15, weight: .semibold, design: .rounded))
                        .foregroundColor(Brand.ink)
                        .padding(.horizontal, 26)
                        .padding(.vertical, 11)
                        .background(
                            RoundedRectangle(cornerRadius: 12, style: .continuous)
                                .fill(Brand.cardPaper))
                        .overlay(
                            RoundedRectangle(cornerRadius: 12, style: .continuous)
                                .stroke(Brand.ink.opacity(0.25), lineWidth: 1))
                }
                .padding(.top, 6)
            }
        }
    }
}

/// 配对码输入面板。
///
/// 首次连接某台电脑、或那台电脑撤销过授权时会弹出来。做成画面上的浮层而不是
/// 跳转新页面：配对成功后紧接着就是 SDP 协商，跳页会把刚建立的会话打断，
/// 用户还得重新点一次进入。
struct PairingPrompt: View {
    let hostId: String
    @Binding var code: String
    /// 失败原因（由被控端给的一句话），nil 表示还没失败过
    let error: String?
    let submitting: Bool
    var onCancel: () -> Void
    var onSubmit: () -> Void

    @FocusState private var focused: Bool

    /// 配对码位数，须与被控端 `pairing::CODE_LEN` 一致。
    private let codeLength = 6

    var body: some View {
        ZStack {
            // 半透明底：一是让面板从画面里浮出来，二是吞掉误触——
            // 底下就是远程画面的鼠标/手势区，不挡住的话输码时会顺手点到电脑。
            Color.black.opacity(0.45)
                .ignoresSafeArea()
                .onTapGesture {}

            VStack(spacing: 14) {
                Image(systemName: "lock.shield")
                    .font(.system(size: 30))
                    .foregroundColor(Brand.accentDeep)

                Text("需要配对")
                    .font(.headline)
                    .foregroundColor(Brand.ink)

                Text("在电脑端的 Makuris 里查看 6 位配对码，填入后即可控制 \(hostId)。")
                    .font(.footnote)
                    .multilineTextAlignment(.center)
                    .foregroundColor(Brand.ink.opacity(0.75))
                    .fixedSize(horizontal: false, vertical: true)

                TextField("000000", text: $code)
                    .keyboardType(.numberPad)
                    .textContentType(.oneTimeCode)
                    .multilineTextAlignment(.center)
                    .font(.system(size: 26, weight: .semibold, design: .monospaced))
                    .foregroundColor(Brand.ink)
                    .padding(.vertical, 10)
                    .background(Color.white.opacity(0.7))
                    .cornerRadius(10)
                    .overlay(
                        RoundedRectangle(cornerRadius: 10)
                            .stroke(Brand.ink.opacity(0.25), lineWidth: 1))
                    .focused($focused)
                    .disabled(submitting)
                    .onChange(of: code) { v in
                        // 只留数字并截到 6 位。数字键盘也可能粘进带空格的内容，
                        // 而超过 6 位会让 HMAC 拿错输入、明明输对却被拒。
                        let clipped = String(v.filter { $0.isNumber }.prefix(codeLength))
                        if clipped != v { code = clipped }
                    }

                if let error {
                    Text(error)
                        .font(.footnote)
                        .foregroundColor(Color(red: 0.72, green: 0.22, blue: 0.15))
                        .multilineTextAlignment(.center)
                        .fixedSize(horizontal: false, vertical: true)
                }

                HStack(spacing: 12) {
                    Button("取消", action: onCancel)
                        .foregroundColor(Brand.ink.opacity(0.7))
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 11)
                        .background(Color.white.opacity(0.6))
                        .cornerRadius(10)

                    Button(action: submit) {
                        Group {
                            if submitting {
                                ProgressView().tint(.white)
                            } else {
                                Text("连接")
                            }
                        }
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 11)
                        // 提交中或位数不足时置灰：被控端对失败会翻倍锁定，
                        // 半截的码提交一次就白费一次尝试机会。
                        .background(canSubmit ? Brand.accentDeep : Brand.accentDeep.opacity(0.35))
                        .foregroundColor(.white)
                        .cornerRadius(10)
                    }
                    .disabled(!canSubmit)
                }
                .font(.body.weight(.medium))
            }
            .padding(20)
            .frame(maxWidth: 330)
            .background(Brand.cardPaper)
            .cornerRadius(18)
            .shadow(color: .black.opacity(0.22), radius: 18, y: 6)
            .padding(.horizontal, 32)
        }
        .onAppear { focused = true }
    }

    private var canSubmit: Bool {
        !submitting && code.count == codeLength
    }

    private func submit() {
        guard canSubmit else { return }
        onSubmit()
    }
}

// MARK: - 品牌视觉

/// 主界面的暖色手绘配色。与远程面板（SessionStatusBar）同一色系：
/// 墨棕做线条与文字、琥珀做重色、暖纸做底，两处界面看着是一套东西。
enum Brand {
    /// 墨棕：描边与文字的正色
    static let ink = Color(red: 0.40, green: 0.25, blue: 0.13)
    /// 琥珀：logo 信号弧等点睛处
    static let accent = Color(red: 0.93, green: 0.58, blue: 0.20)
    /// 深琥珀：图标与可点元素，比 accent 略深以保证浅底上的对比
    static let accentDeep = Color(red: 0.80, green: 0.44, blue: 0.10)
    /// 卡片纸白：列表行的底色，比整屏底略亮，形成层次
    static let cardPaper = Color(red: 1.0, green: 0.988, blue: 0.957)

    /// 整屏底：米白到浅驼的竖向渐变，像一张受光的纸
    static var paper: LinearGradient {
        LinearGradient(
            colors: [Color(red: 0.996, green: 0.973, blue: 0.914),
                     Color(red: 0.945, green: 0.876, blue: 0.757)],
            startPoint: .top, endPoint: .bottom)
    }
}

extension ContentView {
/// 品牌头部：手绘 logo + 名字 + 一句话说明。
/// 放在列表首屏最上方，既回答了「这是什么 App」，也把原来那片空白用起来。
@ViewBuilder var brandHeader: some View {
    VStack(spacing: 12) {
        MakurisLogo(side: 74)
        VStack(spacing: 5) {
            Text("Makuris")
                .font(.system(size: 30, weight: .bold, design: .rounded))
                .foregroundColor(Brand.ink)
            Text("远程操控你的电脑")
                .font(.system(size: 14, weight: .medium, design: .rounded))
                .foregroundColor(Brand.ink.opacity(0.62))
        }
    }
    .frame(maxWidth: .infinity)
    .padding(.top, 22)
    .padding(.bottom, 12)
    // 这一行不属于任何卡片，去掉底色与分隔线，让它直接落在纸面上
    .listRowBackground(Color.clear)
    .listRowSeparator(.hidden)
    .listRowInsets(EdgeInsets())
}
}

// MARK: - Logo

/// Makuris 的 mark：品牌 logo（母版 brand/logo-1024.png，派生进 Logo.imageset）。
/// 圆角比例对齐 iOS 图标的 squircle（≈0.225），加一层浅投影让它在暖纸背景上
/// 有实体应用图标的浮起感，而不是平面贴图。
struct MakurisLogo: View {
    var side: CGFloat = 74

    var body: some View {
        Image("Logo")
            .resizable()
            .interpolation(.high)
            .aspectRatio(1, contentMode: .fit)
            .frame(width: side, height: side)
            .clipShape(RoundedRectangle(cornerRadius: side * 0.225, style: .continuous))
            .shadow(color: Brand.ink.opacity(0.18), radius: side * 0.08, x: 0, y: side * 0.03)
            .accessibilityLabel("Makuris")
    }
}