import Foundation

// MARK: - 信令消息（与 Rust proto SignalMsg JSON 对齐）

enum Role: String, Codable {
    case host
    case viewer
}

enum SdpType: String, Codable {
    case offer
    case answer
}

struct DeviceInfo: Codable, Identifiable {
    var id: String { device_id }
    let device_id: String
    let role: Role
    let platform: String?
}

enum SignalMsg: Codable {
    case register(device_id: String, role: Role, key_proof: String?)
    case registered(device_id: String)
    case deviceList(devices: [DeviceInfo])
    case listDevices
    case connect(from: String, to: String, token: String?)
    /// 需要配对（host -> viewer）：附一次性随机数
    case authRequired(from: String, to: String, nonce: String)
    /// 配对应答（viewer -> host）：proof = HMAC-SHA256(配对码, nonce) 的十六进制小写
    case authResponse(from: String, to: String, proof: String)
    /// 配对通过（host -> viewer）：附后续可复用的信任令牌
    case authGranted(from: String, to: String, token: String)
    /// 配对失败（host -> viewer）：reason 是给用户看的一句话
    case authDenied(from: String, to: String, reason: String)
    case sdp(from: String, to: String, sdp_type: SdpType, sdp: String)
    case ice(from: String, to: String, candidate: String, sdp_mid: String?, sdp_mline_index: Int32)
    case ping
    case pong
    case error(message: String)
    /// 注册成功后服务器下发的时效 TURN 凭证（coturn REST 风格，username 内嵌
    /// 过期时间戳，到期自动作废）。iOS 当前解码后忽略：本 WebRTC 框架在
    /// ObjC→C++ 链路丢失 TURN 凭证，媒体中继由 host 侧负责（见 WebRTCSession
    /// iceServers 注释）。保留解码能力是为协议完整性——不认得的 type 会抛错，
    /// 让服务器未来的下发在日志里表现为持续报错。
    case turnCredentials(urls: [String], realm: String, username: String, credential: String, ttl: UInt64)

    private enum CodingKeys: String, CodingKey {
        case type
        case device_id, role, devices, from, to
        case sdp_type, sdp
        case candidate, sdp_mid, sdp_mline_index
        case nonce, proof, token, reason
        case key_proof
        case message
        case urls, realm, username, credential, ttl
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let type = try c.decode(String.self, forKey: .type)
        switch type {
        case "register":
            self = .register(
                device_id: try c.decode(String.self, forKey: .device_id),
                role: try c.decode(Role.self, forKey: .role),
                key_proof: try c.decodeIfPresent(String.self, forKey: .key_proof)
            )
        case "registered":
            self = .registered(device_id: try c.decode(String.self, forKey: .device_id))
        case "device_list":
            self = .deviceList(devices: try c.decode([DeviceInfo].self, forKey: .devices))
        case "list_devices":
            self = .listDevices
        case "connect":
            self = .connect(
                from: try c.decode(String.self, forKey: .from),
                to: try c.decode(String.self, forKey: .to),
                token: try c.decodeIfPresent(String.self, forKey: .token)
            )
        case "auth_required":
            self = .authRequired(
                from: try c.decode(String.self, forKey: .from),
                to: try c.decode(String.self, forKey: .to),
                nonce: try c.decode(String.self, forKey: .nonce)
            )
        case "auth_response":
            self = .authResponse(
                from: try c.decode(String.self, forKey: .from),
                to: try c.decode(String.self, forKey: .to),
                proof: try c.decode(String.self, forKey: .proof)
            )
        case "auth_granted":
            self = .authGranted(
                from: try c.decode(String.self, forKey: .from),
                to: try c.decode(String.self, forKey: .to),
                token: try c.decode(String.self, forKey: .token)
            )
        case "auth_denied":
            self = .authDenied(
                from: try c.decode(String.self, forKey: .from),
                to: try c.decode(String.self, forKey: .to),
                reason: try c.decode(String.self, forKey: .reason)
            )
        case "sdp":
            self = .sdp(
                from: try c.decode(String.self, forKey: .from),
                to: try c.decode(String.self, forKey: .to),
                sdp_type: try c.decode(SdpType.self, forKey: .sdp_type),
                sdp: try c.decode(String.self, forKey: .sdp)
            )
        case "ice":
            self = .ice(
                from: try c.decode(String.self, forKey: .from),
                to: try c.decode(String.self, forKey: .to),
                candidate: try c.decode(String.self, forKey: .candidate),
                sdp_mid: try c.decodeIfPresent(String.self, forKey: .sdp_mid),
                sdp_mline_index: try c.decode(Int32.self, forKey: .sdp_mline_index)
            )
        case "ping":
            self = .ping
        case "pong":
            self = .pong
        case "error":
            self = .error(message: try c.decode(String.self, forKey: .message))
        case "turn_credentials":
            self = .turnCredentials(
                urls: try c.decode([String].self, forKey: .urls),
                realm: try c.decode(String.self, forKey: .realm),
                username: try c.decode(String.self, forKey: .username),
                credential: try c.decode(String.self, forKey: .credential),
                ttl: try c.decode(UInt64.self, forKey: .ttl)
            )
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .type, in: c, debugDescription: "unknown type: \(type)")
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .register(let device_id, let role, let key_proof):
            try c.encode("register", forKey: .type)
            try c.encode(device_id, forKey: .device_id)
            try c.encode(role, forKey: .role)
            // 服务器没启用接入鉴权时不发该字段，免得让旧服务器因多余字段而困惑
            try c.encodeIfPresent(key_proof, forKey: .key_proof)
        case .registered(let device_id):
            try c.encode("registered", forKey: .type)
            try c.encode(device_id, forKey: .device_id)
        case .deviceList(let devices):
            try c.encode("device_list", forKey: .type)
            try c.encode(devices, forKey: .devices)
        case .listDevices:
            try c.encode("list_devices", forKey: .type)
        case .connect(let from, let to, let token):
            try c.encode("connect", forKey: .type)
            try c.encode(from, forKey: .from)
            try c.encode(to, forKey: .to)
            // 令牌只在与已配对过的电脑通信时才有；没配对过就不发该字段，
            // 由被控端回 auth_required 引导配对。
            try c.encodeIfPresent(token, forKey: .token)
        case .authRequired(let from, let to, let nonce):
            try c.encode("auth_required", forKey: .type)
            try c.encode(from, forKey: .from)
            try c.encode(to, forKey: .to)
            try c.encode(nonce, forKey: .nonce)
        case .authResponse(let from, let to, let proof):
            try c.encode("auth_response", forKey: .type)
            try c.encode(from, forKey: .from)
            try c.encode(to, forKey: .to)
            try c.encode(proof, forKey: .proof)
        case .authGranted(let from, let to, let token):
            try c.encode("auth_granted", forKey: .type)
            try c.encode(from, forKey: .from)
            try c.encode(to, forKey: .to)
            try c.encode(token, forKey: .token)
        case .authDenied(let from, let to, let reason):
            try c.encode("auth_denied", forKey: .type)
            try c.encode(from, forKey: .from)
            try c.encode(to, forKey: .to)
            try c.encode(reason, forKey: .reason)
        case .sdp(let from, let to, let sdp_type, let sdp):
            try c.encode("sdp", forKey: .type)
            try c.encode(from, forKey: .from)
            try c.encode(to, forKey: .to)
            try c.encode(sdp_type, forKey: .sdp_type)
            try c.encode(sdp, forKey: .sdp)
        case .ice(let from, let to, let candidate, let sdp_mid, let sdp_mline_index):
            try c.encode("ice", forKey: .type)
            try c.encode(from, forKey: .from)
            try c.encode(to, forKey: .to)
            try c.encode(candidate, forKey: .candidate)
            try c.encodeIfPresent(sdp_mid, forKey: .sdp_mid)
            try c.encode(sdp_mline_index, forKey: .sdp_mline_index)
        case .ping:
            try c.encode("ping", forKey: .type)
        case .pong:
            try c.encode("pong", forKey: .type)
        case .error(let message):
            try c.encode("error", forKey: .type)
            try c.encode(message, forKey: .message)
        case .turnCredentials(let urls, let realm, let username, let credential, let ttl):
            // 控制端从不主动发这条（只收），编码分支仅为 switch 穷尽性存在
            try c.encode("turn_credentials", forKey: .type)
            try c.encode(urls, forKey: .urls)
            try c.encode(realm, forKey: .realm)
            try c.encode(username, forKey: .username)
            try c.encode(credential, forKey: .credential)
            try c.encode(ttl, forKey: .ttl)
        }
    }
}

// MARK: - 输入事件（与 Rust proto InputEvent JSON 对齐）

/// 修饰键位掩码，取值须与 Rust 侧 `makuris_proto::modifier` 一致。
/// 单键注入无法表达 ⌘C 这类组合，必须把修饰键随按键事件一起发送。
enum InputModifier: UInt8 {
    case shift = 1
    case control = 2
    case alt = 4
    case command = 8
}

/// USB HID Usage ID（Keyboard/Keypad page）。
/// 协议约定 keycode 用 HID 码，由被控端转换为平台虚拟键码，
/// 这样同一套码值在 macOS / Windows / Linux 上都能映射。
enum HIDKey {
    static let a: UInt32 = 0x04
    static let c: UInt32 = 0x06
    static let v: UInt32 = 0x19
    static let x: UInt32 = 0x1B
    static let z: UInt32 = 0x1D
    static let one: UInt32 = 0x1E
    static let two: UInt32 = 0x1F
    static let three: UInt32 = 0x20
    static let enter: UInt32 = 0x28
    static let escape: UInt32 = 0x29
    static let backspace: UInt32 = 0x2A
    static let tab: UInt32 = 0x2B
    static let space: UInt32 = 0x2C
    static let w: UInt32 = 0x1A
    static let f: UInt32 = 0x09
    static let minus: UInt32 = 0x2D
    static let equal: UInt32 = 0x2E
    static let leftBracket: UInt32 = 0x2F
    static let rightBracket: UInt32 = 0x30
    static let semicolon: UInt32 = 0x33
    static let quote: UInt32 = 0x34
    static let grave: UInt32 = 0x35
    static let comma: UInt32 = 0x36
    static let period: UInt32 = 0x37
    static let slash: UInt32 = 0x38
    static let right: UInt32 = 0x4F
    static let left: UInt32 = 0x50
    static let down: UInt32 = 0x51
    static let up: UInt32 = 0x52
}

enum InputEvent: Codable {
    case mouseMove(x: Float, y: Float)
    case mouseButton(x: Float, y: Float, button: UInt8, down: Bool)
    case scroll(dx: Float, dy: Float)
    case key(keycode: UInt32, down: Bool, modifiers: UInt8)
    case text(content: String)

    private enum CodingKeys: String, CodingKey {
        case kind, x, y, button, down, dx, dy, keycode, content, modifiers
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try c.decode(String.self, forKey: .kind)
        switch kind {
        case "mouse_move":
            self = .mouseMove(x: try c.decode(Float.self, forKey: .x), y: try c.decode(Float.self, forKey: .y))
        case "mouse_button":
            self = .mouseButton(
                x: try c.decode(Float.self, forKey: .x), y: try c.decode(Float.self, forKey: .y),
                button: try c.decode(UInt8.self, forKey: .button), down: try c.decode(Bool.self, forKey: .down))
        case "scroll":
            self = .scroll(dx: try c.decode(Float.self, forKey: .dx), dy: try c.decode(Float.self, forKey: .dy))
        case "key":
            // modifiers 用 decodeIfPresent：兼容不带该字段的旧版对端
            self = .key(
                keycode: try c.decode(UInt32.self, forKey: .keycode),
                down: try c.decode(Bool.self, forKey: .down),
                modifiers: try c.decodeIfPresent(UInt8.self, forKey: .modifiers) ?? 0)
        case "text":
            self = .text(content: try c.decode(String.self, forKey: .content))
        default:
            throw DecodingError.dataCorruptedError(forKey: .kind, in: c, debugDescription: "unknown kind: \(kind)")
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .mouseMove(let x, let y):
            try c.encode("mouse_move", forKey: .kind)
            try c.encode(x, forKey: .x); try c.encode(y, forKey: .y)
        case .mouseButton(let x, let y, let button, let down):
            try c.encode("mouse_button", forKey: .kind)
            try c.encode(x, forKey: .x); try c.encode(y, forKey: .y)
            try c.encode(button, forKey: .button); try c.encode(down, forKey: .down)
        case .scroll(let dx, let dy):
            try c.encode("scroll", forKey: .kind)
            try c.encode(dx, forKey: .dx); try c.encode(dy, forKey: .dy)
        case .key(let keycode, let down, let modifiers):
            try c.encode("key", forKey: .kind)
            try c.encode(keycode, forKey: .keycode); try c.encode(down, forKey: .down)
            // 无修饰键时不发该字段，报文更紧凑（对端按 0 处理）
            if modifiers != 0 { try c.encode(modifiers, forKey: .modifiers) }
        case .text(let content):
            try c.encode("text", forKey: .kind)
            try c.encode(content, forKey: .content)
        }
    }
}
