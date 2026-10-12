import Security

import CryptoKit
import Foundation

/// 已配对电脑的信任令牌，按 host 的 device_id 存。
///
/// 必须落盘（UserDefaults）而不是只留内存：令牌的全部意义就是「配对一次、
/// 以后免输配对码」。只存内存的话每次冷启动都要重新去电脑上抄一遍六位数字，
/// 这个流程没人会愿意用，最后必然被绕过或关掉。
///
/// 令牌本身不是配对码，泄露它只影响这一台被控端；用户可删掉被控端的
/// trusted-tokens.json 使其全部失效。
enum PairingTokenStore {
    private static let key = "makuris.pairingTokens"

    private static func all() -> [String: String] {
        (UserDefaults.standard.dictionary(forKey: key) as? [String: String]) ?? [:]
    }

    static func token(for hostId: String) -> String? {
        let t = all()[hostId]
        // 空串按没有处理：否则会给被控端发一个空令牌，白白多跑一轮挑战。
        return (t?.isEmpty == false) ? t : nil
    }

    static func save(_ token: String, for hostId: String) {
        guard !token.isEmpty else { return }
        var m = all()
        m[hostId] = token
        UserDefaults.standard.set(m, forKey: key)
    }

    static func clear(for hostId: String) {
        var m = all()
        m.removeValue(forKey: hostId)
        UserDefaults.standard.set(m, forKey: key)
    }
}

/// 信令连接状态。
///
/// 早先用一个 Bool 表示连接与否，界面无法区分「正在连」和「连不上」，
/// 用户只能看到一个红点却不知道发生了什么。这里把失败原因也带上，
/// 以便直接显示给用户（例如地址填错、电脑没开机）。
enum SignalingState: Equatable {
    case disconnected
    case connecting
    case connected
    case failed(String)

    var isConnected: Bool {
        if case .connected = self { return true }
        return false
    }

    /// 供界面显示的简短说明
    var label: String {
        switch self {
        case .disconnected: return "未连接"
        case .connecting: return "正在连接…"
        case .connected: return "已就绪"
        case .failed: return "连接失败"
        }
    }
}

/// 信令客户端：通过 WebSocket 连接信令服务器，收发消息。
/// 使用原生 URLSessionWebSocketTask，无第三方依赖。
class SignalingClient: NSObject, ObservableObject {
    @Published var state: SignalingState = .disconnected
    @Published var devices: [DeviceInfo] = []

    /// 兼容旧调用点
    var isConnected: Bool { state.isConnected }

    private var webSocketTask: URLSessionWebSocketTask?
    /// 信令会话：云端信令是 wss + 自签证书，公共信任链校验必失败，
    /// 由 PinnedTrustDelegate 按 SHA256 指纹放行（见文件末尾）。
    /// delegate 用独立单例而非 self：URLSession 强持有 delegate，传 self 会成环。
    private let session = URLSession(configuration: .default,
                                     delegate: PinnedTrustDelegate.shared,
                                     delegateQueue: nil)
    private let decoder = JSONDecoder()
    private let encoder = JSONEncoder()

    /// 收到 SDP 的回调
    var onSdp: ((SdpType, String, String) -> Void)?  // (type, sdp, from)
    /// 收到 ICE candidate 的回调
    var onIce: ((String, String?, Int32, String) -> Void)?  // (candidate, sdpMid, mlineIndex, from)
    /// 收到 Connect 请求的回调（host 端用）
    var onConnect: ((String) -> Void)?  // (from)
    /// 连接断开回调
    var onDisconnect: (() -> Void)?

    /// 收到配对挑战（hostId, nonce）：界面需弹输入框让用户填配对码
    var onAuthRequired: ((String, String) -> Void)?
    /// 配对被拒（hostId, reason）：把原因显示给用户
    var onAuthDenied: ((String, String) -> Void)?
    /// 配对成功（hostId）
    var onAuthGranted: ((String) -> Void)?

    private var deviceId: String = ""
    private var heartbeatTimer: Timer?

    /// 记住本次连接参数，供断线后自动重连复用。
    /// 必须记下来：重连时无从知道该连回哪里——设备列表里的地址属于 host，
    /// 不是信令服务器的地址，拿它去重连连不上。
    private var lastURL: String?
    private var lastRole: Role?

    /// 各 host 待验证的 nonce（收到 auth_required 时记下，提交配对码时用）。
    /// 存字典而非单个变量：同时连多台电脑时，后一台的挑战不能把前一台的冲掉。
    private var pendingNonces: [String: String] = [:]

    /// 信令服务器的接入密钥（服务器未启用鉴权时为空）。
    var accessKey: String = ""

    /// 计算注册证明：HMAC-SHA256(接入密钥, "register|" + device_id) 的小写十六进制。
    ///
    /// 必须与 Rust 侧 `makuris_proto::access::register_proof` 逐字一致——
    /// 算法、那个 "register|" 前缀、大小写，任何一处不同都会表现为「密钥明明
    /// 没填错却注册不上」，而服务器只会回一句「接入密钥不正确」。
    private func registerKeyProof(for deviceId: String) -> String? {
        let key = accessKey.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !key.isEmpty else { return nil }
        let mac = HMAC<SHA256>.authenticationCode(
            for: Data("register|\(deviceId)".utf8),
            using: SymmetricKey(data: Data(key.utf8)))
        return mac.map { String(format: "%02x", $0) }.joined()
    }

    func connect(url: String, deviceId: String, role: Role) {
        self.deviceId = deviceId
        self.lastURL = url
        self.lastRole = role
        guard let wsURL = URL(string: url) else {
            DispatchQueue.main.async { self.state = .failed("地址格式不正确") }
            print("[signaling] invalid URL: \(url)")
            return
        }

        // 重复连接前先收掉旧连接，否则会留下一条永不关闭的 WebSocket。
        //
        // 置 nil 要在 cancel 之前：旧 socket 的失败回调靠「与 webSocketTask 身份
        // 不等」被丢弃，中间若还留着旧引用，那一瞬就可能被认成真失败。
        heartbeatTimer?.invalidate()
        let previous = webSocketTask
        webSocketTask = nil
        previous?.cancel(with: .goingAway, reason: nil)

        DispatchQueue.main.async {
            self.state = .connecting
            // 必须清空上一轮的设备列表。
            //
            // 不清的话会出现一类极难自查的「点了没反应」：第二次点同一个地址时，
            // 服务器回的设备列表与上次内容完全相同，界面上的 onChange(of: devices)
            // 检测的是「值有没有变」，值没变就不触发，于是等设备的那段逻辑
            // （resolvePendingJoin）永远不执行。用户看到的是点了毫无动静，
            // 而日志里一切正常（注册成功、列表也收到了）。
            self.devices = []
        }
        webSocketTask = session.webSocketTask(with: wsURL)
        webSocketTask?.resume()
        receiveMessage()

        // 注册。服务器若要求接入密钥就附上证明，否则会被拒在门外
        //（连设备列表都看不到，界面会表现为「一直找不到电脑」）。
        send(.register(
            device_id: deviceId,
            role: role,
            key_proof: registerKeyProof(for: deviceId)))

        // 心跳
        heartbeatTimer = Timer.scheduledTimer(withTimeInterval: 25, repeats: true) { [weak self] _ in
            self?.send(.ping)
        }
    }

    func disconnect() {
        heartbeatTimer?.invalidate()
        // 同样先摘引用再取消：取消会触发一次失败回调，摘掉后它按身份不等被丢弃
        let task = webSocketTask
        webSocketTask = nil
        task?.cancel(with: .goingAway, reason: nil)
        DispatchQueue.main.async {
            self.state = .disconnected
            self.devices = []
        }
    }

    /// 点选电脑后一直等不到设备时，把原因写进状态供界面显示。
    ///
    /// 补这个是因为「点了没反应」是这种架构下最伤人的失败模式：连接可能成功、
    /// 也可能被静默拒绝，用户既不知道要等，也不知道错在哪，只能反复点。
    /// 宁可给一句笼统但可行动的解释，也不要什么都不说。
    func reportJoinTimeout() {
        DispatchQueue.main.async {
            guard self.devices.isEmpty, !self.state.isConnected else { return }
            self.state = .failed("没找到在线的电脑：请确认被控端已启动，且与本机填的是同一个信令服务器")
        }
    }

    /// 保存的地址无法解析成可连接地址时告知用户。
    func reportInvalidAddress(_ raw: String) {
        DispatchQueue.main.async {
            self.state = .failed("地址无法识别：\(raw)")
        }
    }

    /// 是否已经连着（或正在连）同一个地址。
    ///
    /// 供「返回首页后再点同一台电脑」判定：这条路径下信令本来就是活的
    ///（返回首页只断 WebRTC 会话、不断信令），若再调一次 connect()，就会把一条
    /// 健康的 WebSocket 拆掉重建——旧 socket 的取消错误异步回来先写成
    /// 「连接失败」，新 socket 随后才连上，用户看到的就是「先报失败、再自己重连」。
    func hasLiveConnection(to url: String) -> Bool {
        guard lastURL == url else { return false }
        switch state {
        case .connected, .connecting: return true
        case .disconnected, .failed: return false
        }
    }

    /// 断线后用上次的参数重连，供回到前台时自愈。
    ///
    /// 只在「确实断开」时重连：已就绪或正在连接中都直接返回，避免把一条
    /// 好连接白白拆掉重建（那会黑屏一下），也避免与用户手动触发的连接打架。
    @discardableResult
    func reconnectIfNeeded() -> Bool {
        guard let url = lastURL, let role = lastRole else { return false }
        if state.isConnected { return false }
        if case .connecting = state { return false }
        print("[signaling] 已断开，重连 \(url)")
        connect(url: url, deviceId: deviceId, role: role)
        return true
    }

    func send(_ msg: SignalMsg) {
        guard let data = try? encoder.encode(msg),
              let text = String(data: data, encoding: .utf8) else { return }
        webSocketTask?.send(.string(text)) { error in
            if let error { print("[signaling] send error: \(error)") }
        }
    }

    /// 请求设备列表
    func requestDeviceList() {
        send(.listDevices)
    }

    /// 复用已有连接时刷新设备列表。
    ///
    /// 为什么要先清空：界面判断「设备列表回来了没」靠的是 onChange 检测值变化。
    /// 服务器回的列表与上一轮内容相同时值没变，onChange 不触发，等设备的那段
    /// 逻辑就永远不执行——用户看到的是点了毫无反应，而日志里一切正常。
    /// 这与首次 connect() 里清空 devices 是同一个原因。
    func refreshDeviceList() {
        DispatchQueue.main.async { self.devices = [] }
        requestDeviceList()
    }

    /// 发起连接到目标 host。
    ///
    /// 带上这台电脑的信任令牌（配对过就有）。被控端认令牌就直接放行；
    /// 没配对过则收到 auth_required，走配对流程。
    func connectTo(hostId: String) {
        send(.connect(from: deviceId, to: hostId, token: PairingTokenStore.token(for: hostId)))
    }

    /// 提交配对码：用被控端发来的 nonce 算出证明发回。
    ///
    /// 配对码本身不进网络——链路上只有 nonce 和它的 HMAC 结果。所以哪怕
    /// 信令是明文 ws://，嗅探者也拿不到配对码，只能看到一个无法反推的一次性证明。
    ///
    /// 计算必须与 Rust 侧 `makuris_proto::pairing::proof` 逐字一致：
    /// HMAC-SHA256、十六进制小写。任何偏差都会表现为「码明明输对了却配对失败」，
    /// 且两端都不报错，只静默拒绝——所以这里的两处格式（小写、双位补零）不能动。
    @discardableResult
    func submitPairingCode(_ code: String, for hostId: String) -> Bool {
        guard let nonce = pendingNonces[hostId] else {
            print("[pairing] 没有待验证的 nonce，无法应答（可能已超时）")
            return false
        }
        let key = SymmetricKey(data: Data(code.utf8))
        let mac = HMAC<SHA256>.authenticationCode(for: Data(nonce.utf8), using: key)
        let proof = mac.map { String(format: "%02x", $0) }.joined()
        send(.authResponse(from: deviceId, to: hostId, proof: proof))
        return true
    }

    /// 清掉某台电脑的信任令牌（用户在电脑上撤销了授权时用）。
    func forgetPairing(for hostId: String) {
        PairingTokenStore.clear(for: hostId)
        pendingNonces.removeValue(forKey: hostId)
    }

    /// 发送 SDP
    func sendSdp(type: SdpType, sdp: String, to: String) {
        send(.sdp(from: deviceId, to: to, sdp_type: type, sdp: sdp))
    }

    /// 发送 ICE candidate
    func sendIce(candidate: String, sdpMid: String?, mlineIndex: Int32, to: String) {
        send(.ice(from: deviceId, to: to, candidate: candidate,
                  sdp_mid: sdpMid, sdp_mline_index: mlineIndex))
    }

    // MARK: - Private

    private func receiveMessage() {
        // 记住这条 receive 属于哪个 socket。换连接时旧 socket 会被 cancel，
        // 而它的失败回调是异步回来的——那时 self.webSocketTask 已指向新 socket。
        // 不作身份判断，这条「旧连接的取消」就会被当成真失败写进状态，
        // 表现正是「返回首页后再点同一台电脑，先闪一次连接失败，然后才自己重连」。
        let socket = webSocketTask
        socket?.receive { [weak self] result in
            guard let self, socket === self.webSocketTask else { return }
            switch result {
            case .success(let message):
                self.handleMessage(message)
                self.receiveMessage()  // 继续监听
            case .failure(let error):
                // 主动取消不算失败：那是我们自己换连接时的动作
                if (error as NSError).code == NSURLErrorCancelled { return }
                print("[signaling] receive error: \(error)")
                DispatchQueue.main.async {
                    guard socket === self.webSocketTask else { return }
                    // 连不上时给出人话原因，否则用户只看到一个红点无从判断
                    let reason = Self.friendlyReason(error)
                    self.state = .failed(reason)
                    self.devices = []
                    self.onDisconnect?()
                }
            }
        }
    }

    /// 把 URLSession 的原始错误翻译成用户能看懂的一句话。
    /// 局域网场景下最常见的就是「电脑没开 / 地址不对 / 不在同一网络」。
    private static func friendlyReason(_ error: Error) -> String {
        let ns = error as NSError
        switch ns.code {
        case NSURLErrorCannotConnectToHost, NSURLErrorCannotFindHost:
            return "连不上，请确认电脑已启动且与手机在同一网络"
        case NSURLErrorTimedOut:
            return "连接超时，请检查网络"
        case NSURLErrorNotConnectedToInternet:
            return "手机未联网"
        default:
            return "连接失败（\(ns.code)）"
        }
    }

    private func handleMessage(_ message: URLSessionWebSocketTask.Message) {
        let data: Data
        switch message {
        case .string(let text): data = Data(text.utf8)
        case .data(let d): data = d
        @unknown default: return
        }

        guard let msg = try? decoder.decode(SignalMsg.self, from: data) else {
            print("[signaling] decode failed: \(String(data: data, encoding: .utf8) ?? "")")
            return
        }

        // 交给独立方法处理。原因不只是整洁：分支变多以后，Swift 在闭包内部
        // 报错会漂移到上面那行 decode，报「类型不明」——真正的原因是某个 case
        // 的模式写错了（比如关联值个数对不上），却指着一个完全正确的位置，
        // 排查时极易走偏。抽成方法后错误位置才是准的。
        DispatchQueue.main.async { self.apply(msg) }
    }

    /// 在主线程把收到的消息落到界面状态与回调上。
    private func apply(_ msg: SignalMsg) {
        switch msg {
        case .registered(let id):
            print("[signaling] registered as \(id)")
            self.state = .connected
            self.requestDeviceList()

        case .deviceList(let devs):
            self.devices = devs.filter { $0.role == .host }
            print("[signaling] devices: \(self.devices.map(\.device_id))")

        case .sdp(_, let from, let type, let sdp):
            self.onSdp?(type, sdp, from)

        case .ice(_, let from, let candidate, let mid, let mline):
            self.onIce?(candidate, mid, mline, from)

        // Connect 有三个关联值（from / to / token），模式必须写全，
        // 少写一个会编译不过——而且报错会漂到别处去。
        case .connect(let from, _, _):
            self.onConnect?(from)

        case .authRequired(let from, _, let nonce):
            // 记下 nonce 再通知界面：用户输完码要拿它算证明，界面不该碰这层细节
            self.pendingNonces[from] = nonce
            print("[pairing] 收到 \(from) 的配对挑战")
            self.onAuthRequired?(from, nonce)

        case .authGranted(let from, _, let token):
            // 令牌落盘，之后连这台电脑就不必再输配对码
            PairingTokenStore.save(token, for: from)
            self.pendingNonces.removeValue(forKey: from)
            print("[pairing] 与 \(from) 配对成功")
            self.onAuthGranted?(from)

        case .authDenied(let from, _, let reason):
            print("[pairing] 配对被拒：\(reason)")
            self.onAuthDenied?(from, reason)

        case .authResponse(_, _, _):
            // 只有被控端会收这条；控制端收到说明对端实现异常，忽略即可
            break

        case .turnCredentials(let urls, _, _, _, let ttl):
            // 控制端刻意不消费：本 WebRTC 框架丢失 TURN 凭证（见 WebRTCSession
            // iceServers 注释），媒体中继由 host 侧负责。打一行日志留痕即可。
            print("[signaling] turn credentials received (\(urls), ttl=\(ttl)s), ignored on viewer")

        case .pong:
            break

        case .error(let message):
            // 服务器明确的拒绝必须让用户看见。
            //
            // 以前这里只打一条日志，后果是「接入密钥填错」与「网络不通」在界面上
            // 一模一样：状态条一直停在转圈，最后什么提示都没有。用户只能反复
            // 重试同一个错误配置，而真正的原因（密钥不符）就写在被丢弃的那条
            // 报文里。
            print("[signaling] server error: \(message)")
            self.state = .failed(message)
            self.devices = []

        default:
            break
        }
    }
}


// MARK: - 信令 TLS 信任策略（自签证书 + SHA256 指纹固定）

/// 云端信令用自签证书（部署机没有域名，公共 CA 签不了 IP），
/// URLSession 默认信任链校验必失败。这里不放开校验（那等于没加密），
/// 而是只认 pin 死的这一张证书：中间人拿任何其它证书（包括真 CA 签发的）
/// 指纹都对不上，直接拒绝握手。
///
/// 指纹与 host/src/tls_pin.rs 的 SIGNALING_CERT_SHA256 是同一张证书
/// （云服务器 /etc/makuris/tls/cert.pem，2036 年到期）。服务器换证书
/// 必须同步更新两端 pin，否则连接会在这里被拒——重新生成方法见知识库
/// 《4G公网链路诊断与中继方案》部署节。
final class PinnedTrustDelegate: NSObject, URLSessionDelegate {
    static let shared = PinnedTrustDelegate()

    /// 叶子证书 DER 的 SHA256（openssl x509 -fingerprint -sha256 的十六进制）
    private static let pinned: [UInt8] = [
        0x10, 0x5B, 0x91, 0x8D, 0x46, 0x51, 0xFE, 0xCF,
        0x2A, 0x66, 0x6E, 0xF3, 0xDB, 0x0F, 0xBB, 0x04,
        0x48, 0xA2, 0x0D, 0xEA, 0xD2, 0x62, 0x13, 0x1B,
        0xEE, 0x91, 0xFB, 0x88, 0x96, 0x6D, 0x55, 0x66,
    ]

    func urlSession(_ session: URLSession,
                    didReceive challenge: URLAuthenticationChallenge,
                    completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
              let trust = challenge.protectionSpace.serverTrust,
              let chain = SecTrustCopyCertificateChain(trust) as? [SecCertificate],
              let leaf = chain.first else {
            // 非服务器信任类挑战（如代理认证）交回默认处理
            completionHandler(.performDefaultHandling, nil)
            return
        }
        let der = SecCertificateCopyData(leaf) as Data
        let digest = [UInt8](SHA256.hash(data: der))
        if digest == Self.pinned {
            completionHandler(.useCredential, URLCredential(trust: trust))
        } else {
            // 指纹不符：疑似中间人，或服务器换证书没同步更新 pin。宁断不信任。
            print("[signaling] 证书指纹不符，拒绝连接 \(challenge.protectionSpace.host)")
            completionHandler(.cancelAuthenticationChallenge, nil)
        }
    }
}
