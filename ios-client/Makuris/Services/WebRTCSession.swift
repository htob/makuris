import Foundation
import WebRTC

/// 会话实时指标（供界面显示）。
///
/// 每个字段都是可选的，取不到时显示「—」而不是 0：0 会被读成
/// 「延迟为 0」「帧率掉到 0」，把「还没有数据」误报成「已经卡死」。
struct SessionStats: Equatable {
    /// 媒体往返延迟（毫秒）
    var rttMs: Double?
    /// 解码帧率
    var fps: Double?
    /// 接收码率（Mbps）
    var bitrateMbps: Double?
    /// 远端画面分辨率（如 3024×1964）
    var resolution: String?

    static let empty = SessionStats()
}

/// WebRTC 会话管理：PeerConnection 生命周期、视频接收、DataChannel 输入发送。
/// iOS 端作为 answerer（host 发 offer，iOS 回 answer）。
class WebRTCSession: NSObject, ObservableObject {
    @Published var isConnected = false
    @Published var connectionState: RTCPeerConnectionState = .new

    /// 是否已收到远端首帧（画面真的能看见了）。
    ///
    /// 放在 session 上而不是渲染视图的 metrics 上：session 的生命周期跨
    /// 「返回首页 → 再进来」这整段，能在新会话开始时被重置。放在 metrics
    /// 那种随界面走的对象上，第二次进入时它会带着上次的 true，连接遮罩
    /// 就直接不显示了。
    @Published var firstFrameArrived = false

    /// 远端视频轨道（供 View 渲染）。
    ///
    /// 轨道由 WebRTC 回调交付、渲染器由画面视图创建，两者先后没有保证，因此
    /// 挂载不能由任何一方「一次做完」——由画面视图自己订阅本属性，谁先到都能补上。
    ///
    /// 这里踩过两个坑，都是同一种错误的不同表现：一是只让 didAdd stream 挂载，
    /// 而 unified plan 下轨道只经 didAdd rtpReceiver 到达，于是永远挂不上；二是
    /// 改成会话层持一个 renderer 引用来挂，切线路时轨道先到、被挂到上一个视图的
    /// 渲染器上，新视图再进来就永远挂不上了。两者都表现为「连上了、解码也在跑，
    /// 屏幕上却一片空白」。
    @Published private(set) var remoteVideoTrack: RTCVideoTrack?

    /// 是否已有画面视图真正接上远端轨道。
    ///
    /// 它是「画面能出来」的硬前提：轨道没挂上时，即便解码统计已经好看，
    /// 屏幕上也什么都没有，此时撤掉连接遮罩只会让用户看到一片空白。
    @Published private(set) var rendererAttached = false

    /// 由画面视图在挂载成功后回报。
    func noteRendererAttached() {
        if !rendererAttached { rendererAttached = true }
    }

    /// 实时会话指标（延迟/帧率/码率/分辨率），供状态条显示。
    @Published var stats = SessionStats()
    /// 输入事件 DataChannel
    private var dataChannel: RTCDataChannel?

    private var peerConnection: RTCPeerConnection?
    private let factory: RTCPeerConnectionFactory
    private var signalingClient: SignalingClient?
    private var remoteDeviceId: String = ""

    /// 指标轮询定时器。1 秒一次：再快没有意义（用户看不过来），
    /// 再慢则延迟飙高时反应太迟钝。
    private var statsTimer: Timer?
    /// 上一轮累计接收字节数与采样时刻，用于算实时码率
    /// （statistics 只给累计值，必须自己差分）
    private var lastBytesReceived: Double?
    private var lastBytesSampleTime: Date?
    /// 上一轮累计解码帧数与时间戳（部分 libwebrtc 版本不直接给 fps，
    /// 只能靠差分算）
    private var lastFramesDecoded: Double?
    private var lastFramesTimestamp: Double?
    /// 最近一次已知分辨率。inbound-rtp 有时不报 frameWidth/frameHeight，
    /// 记住上次的值好过让指标整块变空。
    private var lastKnownResolution: String?

    // MARK: - Init

    override init() {
        // 初始化 WebRTC（全局一次）
        RTCInitializeSSL()
        let videoEncoderFactory = RTCDefaultVideoEncoderFactory()
        let videoDecoderFactory = RTCDefaultVideoDecoderFactory()
        factory = RTCPeerConnectionFactory(
            encoderFactory: videoEncoderFactory,
            decoderFactory: videoDecoderFactory
        )
        super.init()
    }

    // MARK: - Public

    /// 作为 viewer 连接远端 host
    func connectAsViewer(signaling: SignalingClient, to hostId: String) {
        self.signalingClient = signaling
        self.remoteDeviceId = hostId

        // 新会话开始，重置首帧标记。
        //
        // 不清的话它还是上一次连接的 true，连接遮罩直接不显示——用户第二次
        // 进来时又会面对一段完全没有反馈的白屏。异步到主线程：@Published
        // 的订阅方是 SwiftUI，赋值要归主线程管。
        DispatchQueue.main.async { self.firstFrameArrived = false }

        // 先收掉可能存在的旧会话。原先这里直接 createPeerConnection()，
        // 旧 PeerConnection 与 DataChannel 就一直活着（还占着 delegate），
        // 既漏内存，又可能把输入事件发到已废弃的通道上。
        teardownPeerConnection()

        createPeerConnection()

        // 设置信令回调
        signaling.onSdp = { [weak self] type, sdp, from in
            self?.handleRemoteSdp(type: type, sdp: sdp, from: from)
        }
        signaling.onIce = { [weak self] candidate, mid, mline, from in
            self?.handleRemoteIce(candidate: candidate, mid: mid, mline: mline)
        }

        // 发起连接请求（host 收到后会创建 offer 发回）
        signaling.connectTo(hostId: hostId)
    }

    /// 会话断开后重建，复用已保存的信令与对端。
    ///
    /// 用于「切到后台被系统掐断、回到前台要自愈」这条路径：界面不需要
    /// 再传一遍参数，host 端收到 Connect 会重新发 offer，走完整协商。
    /// 返回是否真的发起了重建（已连着、或从未连过时返回 false，保证幂等——
    /// 重复调用不会建出第二个 PeerConnection）。
    @discardableResult
    func reconnectIfNeeded() -> Bool {
        guard let signaling = signalingClient, !remoteDeviceId.isEmpty else { return false }
        guard !isConnected else { return false }

        print("[webrtc] 会话未连接，重建到 \(remoteDeviceId)")
        teardownPeerConnection()
        createPeerConnection()
        signaling.connectTo(hostId: remoteDeviceId)
        return true
    }

    /// 发送输入事件到被控端
    func sendInput(_ event: InputEvent) {
        guard let channel = dataChannel,
              channel.readyState == .open,
              let data = try? JSONEncoder().encode(event) else { return }
        let buffer = RTCDataBuffer(data: data, isBinary: false)
        channel.sendData(buffer)
    }

    /// 断开连接
    func disconnect() {
        peerConnection?.close()
        peerConnection = nil
        dataChannel = nil
        remoteVideoTrack = nil
        DispatchQueue.main.async {
            self.isConnected = false
            self.rendererAttached = false
            self.firstFrameArrived = false
            self.connectionState = .closed
        }
    }

    // MARK: - Private

    /// 收掉当前 PeerConnection 与 DataChannel。
    ///
    /// 重建会话前必须先调用：libwebrtc 的 PeerConnection 持有 delegate，
    /// 不 close 就直接再建一个，旧对象会继续活在后台——既漏内存，也可能让
    /// 输入事件发到已废弃的 DataChannel 上（表现为「鼠标点了没反应」）。
    private func teardownPeerConnection() {
        statsTimer?.invalidate()
        statsTimer = nil
        lastBytesReceived = nil
        lastBytesSampleTime = nil
        dataChannel?.close()
        dataChannel = nil
        peerConnection?.close()
        peerConnection = nil
        DispatchQueue.main.async {
            self.remoteVideoTrack = nil
            self.rendererAttached = false
            self.firstFrameArrived = false
            self.isConnected = false
            self.stats = .empty
        }
    }

    // MARK: - 会话指标

    /// 开始每秒拉取一次统计。
    private func startStatsPolling() {
        statsTimer?.invalidate()
        lastBytesReceived = nil
        lastBytesSampleTime = nil
        lastFramesDecoded = nil
        lastFramesTimestamp = nil
        // 首轮先取一次：否则要等满 1 秒才出现数据，刚进画面那几秒是空白
        pollStats()
        statsTimer = Timer.scheduledTimer(withTimeInterval: 1.0, repeats: true) { [weak self] _ in
            self?.pollStats()
        }
    }

    /// 从 WebRTC statistics 里取延迟、帧率、码率、分辨率。
    ///
    /// 这里做的是「尽量取，取不到就留空」：不同版本的 libwebrtc 字段名有差异，
    /// 某项取不到时显示「—」即可，不必让整块指标消失，更不该崩掉。
    private func pollStats() {
        guard let pc = peerConnection else { return }
        pc.statistics { [weak self] report in
            guard let self else { return }

            var new = SessionStats()

            // 分辨率从视频轨道的远端入参看最直接
            if let track = self.remoteVideoTrack {
                // 尺寸由渲染层回调，这里仅作兜底，不覆盖已有值
                _ = track
            }
            new.resolution = self.lastKnownResolution

            for (_, entry) in report.statistics {
                switch entry.type {
                case "candidate-pair":
                    // 只在被选中（正在使用）的候选对上取 RTT，
                    // 否则会把未使用路径的 0 值读成「零延迟」
                    let isSelected = (entry.values["selected"] as? Bool) ?? false
                    let stateOk = (entry.values["state"] as? String) == "succeeded"
                    let nominated = (entry.values["nominated"] as? Bool) ?? false
                    if (isSelected || nominated) && stateOk {
                        if let rtt = entry.values["currentRoundTripTime"] as? Double {
                            new.rttMs = rtt * 1000.0
                        }
                    }

                case "inbound-rtp":
                    // 只关心视频流；音频轨我们没有
                    if let kind = entry.values["kind"] as? String, kind != "video" { continue }
                    if let mediaType = entry.values["mediaType"] as? String, mediaType != "video" { continue }

                    if let fps = entry.values["framesPerSecond"] as? Double {
                        new.fps = fps
                    } else if let frames = entry.values["framesDecoded"] as? Double,
                              let ts = entry.values["timestamp"] as? Double,
                              let lastTs = self.lastFramesTimestamp,
                              let lastFrames = self.lastFramesDecoded,
                              ts > lastTs {
                        // 没有现成 fps 字段时，用累计解码帧数差分算
                        let delta = frames - lastFrames
                        let seconds = (ts - lastTs) / 1000.0
                        if seconds > 0.2 { new.fps = max(0, delta / seconds) }
                    }
                    self.lastFramesDecoded = entry.values["framesDecoded"] as? Double
                    self.lastFramesTimestamp = entry.values["timestamp"] as? Double

                    // 解码统计只作诊断，不再用来撤遮罩。
                    //
                    // 曾经用它兜底置 firstFrameArrived，结果制造了更糟的一幕：
                    // 解码明明在跑（说明媒体到达了），但渲染器还没接上轨道，
                    // 遮罩被提前撤掉，用户看到的是一片空白——比「一直转圈」
                    // 更难判断。出画与否取决于渲染器，这里是它的下游，不能反过来
                    // 当门控。仅打印一次，便于和渲染器日志对照排查。

                    // 码率：statistics 给的是累计字节数，差分才是瞬时速率。
                    // 时间基准用本机 Date()：libwebrtc 154 的 values 字典里
                    // 并没有 "timestamp" 这个键（实测 fps 能取到是因为走了
                    // framesPerSecond 直取分支），拿它做差分永远算不出码率。
                    if let bytes = entry.values["bytesReceived"] as? Double {
                        let now = Date()
                        if let lastBytes = self.lastBytesReceived,
                           let lastTime = self.lastBytesSampleTime {
                            let seconds = now.timeIntervalSince(lastTime)
                            let deltaBytes = bytes - lastBytes
                            if seconds > 0.2 && deltaBytes >= 0 {
                                new.bitrateMbps = deltaBytes * 8.0 / seconds / 1_000_000.0
                            }
                        }
                        self.lastBytesReceived = bytes
                        self.lastBytesSampleTime = now
                    }

                    if let w = entry.values["frameWidth"] as? Double,
                       let h = entry.values["frameHeight"] as? Double,
                       w > 0, h > 0 {
                        new.resolution = "\(Int(w))×\(Int(h))"
                        self.lastKnownResolution = new.resolution
                    }

                default:
                    break
                }
            }

            DispatchQueue.main.async { self.stats = new }
        }
    }

    private func createPeerConnection() {
        let config = RTCConfiguration()
        config.iceServers = [
            // 顺序与端口的选择以实测为准，不能凭「Google 在境内被阻断」的常识推断。
            //
            // 曾把 cloudflare:3478 挪到首位（10-08 17:58），理由是 Google STUN 境内
            // 不稳。结果 iPhone 在蜂窝网下 srflx 直接归零：改前最后一次成功会话
            // （10-08 17:48:48）候选里有 3~4 条 srflx 203.0.113.41，改后 10-09
            // 的 6 次会话 answer 内 srflx 全为 0 条，只剩运营商内网 10.0.0.90 与
            // 全局 IPv6，而家里两台机器都没有全局 IPv6，双方凑不出候选对，ICE 一律
            // 30 秒 Failed、推送 0 帧。
            //
            // 差别在端口：19302 是 Google 自定的非常用端口，运营商一般不干预；
            // 3478 是 STUN 的标准端口，蜂窝网下常被限速或直接丢弃。LibWebRTC 对
            // 多个 iceServer 是并发收集而非失败重试，所以「排第一」不是「优先失败
            // 后回退」，而是决定谁的结果进不了候选表——把不通的放前面等于自断。
            //
            // 保留后两者作为补充（家宽下它们可用，能多几条候选），但 19302 必须在
            // 首位。若将来接入 TURN，应改用 turns:443——蜂窝下 TCP 443 实测通畅
            // （信令正是靠它连上的），而 3478 同样会被掐。
            RTCIceServer(urlStrings: ["stun:stun.l.google.com:19302"]),
            RTCIceServer(urlStrings: ["stun:stun1.l.google.com:19302"]),
            RTCIceServer(urlStrings: ["stun:stun.cloudflare.com:3478"]),
            RTCIceServer(urlStrings: ["stun:stun.miwifi.com:3478"]),
            // TURN 中继（自建 coturn，部署在带公网 IP 的云服务器上）。
            //
            // 为什么必须有它：iPhone 蜂窝在运营商 CGNAT 后面，「家里→iPhone」方向
            // 的 UDP 被入站过滤全部丢弃（host 侧 trace 实测 prflx 恒为 0，重试窗口
            // 放宽到 16 秒仍零入包），srflx×srflx 打洞在此网络下无解。TURN 让
            // iPhone 主动向中继发起 Allocate——出站方向实测通畅（STUN 经 UDP 19302
            // 能拿到 srflx 即证据），中继再把 host 的数据转给 iPhone，全程都是可达方向。
            //
            // 为什么中继不能放家里：家里光猫的 WAN 口是 10.x/100.x 私网地址（运营商级
            // CGNAT），公网入站根本到不了家，任何端口转发都无效——实测海外节点探测
            // 家里出口 IP 的 3478/49160/49161 全部超时。
            //
            // 地址与凭证是当前个人部署的固定值；开源发布前必须改为设置页录入或
            // 由信令服务器下发（见 TODO）。ICE 会自动先试 srflx 直连、失败才落
            // relay，局域网场景直连优先、不会被中继拖慢。
            // 中继在云服务器（203.0.113.10，coturn），监听多个入口端口。
            //
            // 首选 49161 而不是标准的 3478：蜂窝网对 UDP 3478 有运营商级 QoS 丢弃风险
            // （同族证据见上方 STUN 注释——把 stun.cloudflare.com:3478 挪到 iceServers
            // 首位后，iPhone 蜂窝的 srflx 直接归零；3478 是 STUN 标准端口，运营商常干预，
            // 49xxx 高位端口不在常见拦截名单里）。实测依据：上一版只配 :3478 时，
            // iPhone 蜂窝下 TURN 的 Allocate 包在云服务器抓包为 0 条，而同源同机走
            // 家宽发 :3478 正常——差异只在蜂窝这一跳。
            // 曾试过 19302（Google STUN 端口）作蜂窝入口，但云防火墙的 UDP 19302 规则
            // 实测不生效（tcpdump：UDP 3478 入包 20 条、UDP 19302 入包 0 条），故改用
            // 早已放行的 UDP 49160-49300 段内的 49161。
            //
            // 三条 URL 并发收集，谁通用谁：LibWebRTC 对多个 iceServer 是并发而非失败重试。
            // TCP 变体（?transport=tcp）作兜底：TCP 出站几乎不被运营商干预。
            // 不用 turns:443 —— 443 已被服务器上既有 nginx 占用，且自签证书会被
            // iOS libwebrtc 拒绝校验（已实测）。
            // TURN 中继不在本端配置（2026-10-12 开源改造定案）：
            // ①本 WebRTC.xcframework 在 ObjC→C++ 传递链路丢失 TURN 长期凭证，
            //   实测 iPhone 的 Allocate 永远停在 20/28 字节裸请求、带凭证第二步
            //   零出现（两种初始化器均验证过），框架层缺陷业务代码不可修；
            // ②媒体中继由 host 侧自写 TURN 客户端负责：host 向 coturn Allocate
            //   出 relay 候选后 trickle 给 iPhone，iPhone 只需连普通 UDP 地址；
            // ③长期凭证已从源码移除——信令服务器注册后随 turn_credentials 下发
            //   coturn 时效凭证（HMAC，数小时自动过期），源码零秘密。
        ]

        config.sdpSemantics = .unifiedPlan
        config.continualGatheringPolicy = .gatherContinually

        let constraints = RTCMediaConstraints(
            mandatoryConstraints: nil,
            optionalConstraints: ["DtlsSrtpKeyAgreement": "true"]
        )

        peerConnection = factory.peerConnection(
            with: config, constraints: constraints, delegate: self)
    }

    private func handleRemoteSdp(type: SdpType, sdp: String, from: String) {
        guard let pc = peerConnection else { return }
        let rtcType: RTCSdpType = (type == .offer) ? .offer : .answer
        let description = RTCSessionDescription(type: rtcType, sdp: sdp)

        pc.setRemoteDescription(description) { [weak self] error in
            if let error {
                print("[webrtc] setRemoteDescription error: \(error)")
                return
            }
            // 如果是 offer，创建 answer 并发回
            if type == .offer {
                self?.createAnswer()
            }
        }
    }

    private func createAnswer() {
        guard let pc = peerConnection else { return }
        let constraints = RTCMediaConstraints(
            mandatoryConstraints: [
                "OfferToReceiveVideo": "true",
                "OfferToReceiveAudio": "false"
            ],
            optionalConstraints: nil
        )

        pc.answer(for: constraints) { [weak self] sdp, error in
            guard let self, let sdp, error == nil else {
                print("[webrtc] createAnswer error: \(error?.localizedDescription ?? "unknown")")
                return
            }
            pc.setLocalDescription(sdp) { error in
                if let error {
                    print("[webrtc] setLocalDescription error: \(error)")
                    return
                }
                // 等 ICE 收集完成再发 answer，把候选写进 SDP 一起带过去。
                //
                // 原先设完 local description 立刻发送，而那一刻收集还没开始，
                // answer 里一条候选都没有，连通性就全靠 trickle 通道单独送候选。
                // 实测经信令的会话中 host 侧候选对始终为 0（日志 "no candidate
                // pairs" 连续出现、0 帧），而同一部手机走局域网时正常——差别正是
                // 候选有没有随 SDP 到达。内联后不再依赖 trickle 这一条链路。
                self.sendAnswerAfterGathering(fallback: sdp.sdp)
            }
        }
    }

    /// 等 ICE 收集完成（最多 2 秒）再发 answer，让候选随 SDP 一起送达。
    ///
    /// 之所以不是无限等：公网下某个 STUN 不通时收集要等它超时才收尾，
    /// 拖太久用户只会看到卡住。2 秒是「本地/国内 STUN 早就回来了、
    /// 外网 STUN 该放弃」的经验值，超时后用当前已有的候选照发。
    private func sendAnswerAfterGathering(fallback: String) {
        let deadline = Date().addingTimeInterval(2.0)

        func emit(_ pc: RTCPeerConnection) {
            let sdp = pc.localDescription?.sdp ?? fallback
            self.signalingClient?.sendSdp(type: .answer, sdp: sdp, to: self.remoteDeviceId)
            // 候选数一并打印：万一仍是 0，一眼就能看出是收集没出候选而不是没发出去
            let count = sdp.components(separatedBy: "a=candidate").count - 1
            print("[webrtc] answer sent to \(self.remoteDeviceId)（含候选 \(count) 条）")
        }

        func poll() {
            guard let pc = peerConnection else { return }
            if pc.iceGatheringState == .complete || Date() >= deadline {
                emit(pc)
                return
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { poll() }
        }

        poll()
    }

    private func handleRemoteIce(candidate: String, mid: String?, mline: Int32) {
        let iceCandidate = RTCIceCandidate(sdp: candidate, sdpMLineIndex: mline, sdpMid: mid)
        peerConnection?.add(iceCandidate)
    }
}

// MARK: - RTCPeerConnectionDelegate

extension WebRTCSession: RTCPeerConnectionDelegate {

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange stateChanged: RTCSignalingState) {
        print("[webrtc] signaling state: \(stateChanged.rawValue)")
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didAdd stream: RTCMediaStream) {
        print("[webrtc] didAdd stream: video=\(stream.videoTracks.count) audio=\(stream.audioTracks.count)")
        guard let videoTrack = stream.videoTracks.first else { return }
        DispatchQueue.main.async {
            // 赋值触发 didSet 里的挂载，无需在这里碰渲染器
            self.remoteVideoTrack = videoTrack
            print("[webrtc] remote video track set on MAIN thread (stream)")
        }
    }

    /// unified plan 下远端轨道实际走这条回调（didAdd stream 不再触发）。
    ///
    /// 原先这里只在 remoteVideoTrack 为 nil 时才赋值、且完全不碰渲染器，
    /// 结果是「轨道记下了、画面始终不出来」：渲染器接不上轨道就不会有首帧
    /// 回调，firstFrameArrived 永远为 false，连接遮罩就一直卡在
    /// 「正在建立视频通道」不撤——host 那边其实早就在正常推流了。
    func peerConnection(_ peerConnection: RTCPeerConnection, didAdd rtpReceiver: RTCRtpReceiver, streams mediaStreams: [RTCMediaStream]) {
        print("[webrtc] didAdd rtpReceiver: kind=\(rtpReceiver.track?.kind ?? "nil")")
        guard let videoTrack = rtpReceiver.track as? RTCVideoTrack else { return }
        DispatchQueue.main.async {
            self.remoteVideoTrack = videoTrack
            print("[webrtc] remote video track set on MAIN thread (rtpReceiver)")
        }
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove stream: RTCMediaStream) {}

    func peerConnectionShouldNegotiate(_ peerConnection: RTCPeerConnection) {}

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceConnectionState) {
        print("[webrtc] ICE connection state: \(newState.rawValue)")
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCIceGatheringState) {
        NSLog("[webrtc] ICE gathering state -> \(newState)")
    }

    /// TURN 候选收集失败的确切原因只能从这里拿到。
    ///
    /// 为什么必须实现它：2026-10-10 服务器 eth0 抓包实测，iPhone 发往 UDP 3478 的 Allocate
    /// 全部只有 20/28 字节（裸 STUN 头 + REQUESTED-TRANSPORT，不含 USERNAME/REALM/NONCE），
    /// 服务器回 92 字节（401 挑战携带 REALM/NONCE）之后再无后续——即认证停在第一步、
    /// 客户端没有带凭证重试。业务层此前看不到这件事（本回调未实现、框架内部日志也未打开），
    /// 只能靠抓包尺寸反推。errorCode / errorText 会直接给出是 401 凭证问题、还是端口不通、
    /// 还是 URL 被拒。
    func peerConnection(_ peerConnection: RTCPeerConnection,
                        didFailToGatherIceCandidate event: RTCIceCandidateErrorEvent) {
        NSLog("[webrtc] TURN gather FAILED url=\(event.url) addr=\(event.address):\(event.port) "
              + "code=\(event.errorCode) text=\(event.errorText)")
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didGenerate candidate: RTCIceCandidate) {
        // 发送本地 ICE candidate 给 host
        signalingClient?.sendIce(
            candidate: candidate.sdp,
            sdpMid: candidate.sdpMid,
            mlineIndex: candidate.sdpMLineIndex,
            to: remoteDeviceId
        )
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove candidates: [RTCIceCandidate]) {}

    func peerConnection(_ peerConnection: RTCPeerConnection, didOpen dataChannel: RTCDataChannel) {
        // Host 创建的 DataChannel（用于接收输入事件）
        self.dataChannel = dataChannel
        dataChannel.delegate = self
        print("[webrtc] data channel opened: \(dataChannel.label)")
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didChange newState: RTCPeerConnectionState) {
        DispatchQueue.main.async {
            self.connectionState = newState
            self.isConnected = (newState == .connected)
        }
        // 连上才开始拉指标，断开就停：没人看的时候不必每秒唤醒一次 CPU。
        // 注意 async 到主线程再启停——Timer 与 @Published 都归主线程管。
        DispatchQueue.main.async {
            if newState == .connected {
                self.startStatsPolling()
            } else if newState == .closed || newState == .failed || newState == .disconnected {
                self.statsTimer?.invalidate()
                self.statsTimer = nil
            }
        }
        print("[webrtc] peer connection state: \(newState.rawValue)")
    }
}

// MARK: - RTCDataChannelDelegate

extension WebRTCSession: RTCDataChannelDelegate {
    func dataChannelDidChangeState(_ dataChannel: RTCDataChannel) {
        print("[webrtc] data channel state: \(dataChannel.readyState.rawValue)")
    }

    func dataChannel(_ dataChannel: RTCDataChannel, didReceiveMessageWith buffer: RTCDataBuffer) {
        // Host -> Viewer 的消息（暂不使用）
    }
}
