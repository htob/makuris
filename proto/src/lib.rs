//! Makuris 共享协议定义：信令消息 + 输入事件。
//! 控制端(iOS)与被控端(Host)共用这套类型，保证两端一致。
use serde::{Deserialize, Serialize};

/// 设备角色
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// 被控端（电脑）
    Host,
    /// 控制端（iOS）
    Viewer,
}

/// SDP 类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SdpType {
    Offer,
    Answer,
}

/// 信令消息：经 WebSocket 交换，由信令服务器转发。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalMsg {
    /// 设备注册
    ///
    /// `key_proof` 是信令接入证明（见 `access` 模块）。信令服务器若配了接入密钥，
    /// 就会要求证明：否则任何能连上信令端口的人都能枚举出你家在线的电脑，
    /// 还能顶着别人的 device_id 注册、把本该发往那台电脑的信令引到自己手里。
    ///
    /// `#[serde(default)]` 让不带该字段的旧客户端仍能解析，不至于因协议升级
    /// 直接连不上——服务器再按来源地址决定放不放行。
    ///
    /// `skip_serializing_if` 与 `default` 必须成对：只写 default 的话，值为 None
    /// 时 serde 仍会发一个 `"key_proof":null`。多余字段本身无害，但它会让
    /// 「没配密钥就不带该字段」这句注释变成假话，也白白增大每条注册报文。
    Register {
        device_id: String,
        role: Role,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key_proof: Option<String>,
        /// 被控端上报的可读名字（主机名，如 `my-MacBook-Pro`）。
        ///
        /// 为什么需要它：`device_id` 是路由键（形如 `host-mac-01`），用来
        /// 定位连接、不能随便改，但用户读不懂这串机器标识。名字只在展示层
        /// 用，换名字不影响路由与已配对令牌。
        ///
        /// 仍然可选：老版本被控端不带这个字段，界面退回显示 `device_id`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// 注册成功确认
    Registered { device_id: String },
    /// 设备列表（信令服务器 -> viewer）
    DeviceList { devices: Vec<DeviceInfo> },
    /// 请求设备列表
    ListDevices,
    /// 发起连接（viewer -> host）
    ///
    /// `token` 是上一次配对成功后由 host 签发的信任令牌。带上的情况下 host
    /// 直接放行（已配对设备免输配对码）；不带或不被认可时，host 会回
    /// `AuthRequired` 把流程引向配对。
    ///
    /// 为什么要这个字段：公网暴露后，若收到 `Connect` 就建会话，等于任何
    /// 摸到信令端点的人都能看屏幕、动鼠标。局域网里「同一 Wi-Fi」勉强还能
    /// 当信任边界，公网完全没有边界可言。
    Connect {
        from: String,
        to: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// 需要配对（host -> viewer）：附一次性随机数
    ///
    /// 不直接收配对码，而是走挑战应答：viewer 用配对码和这个 nonce 算出
    /// HMAC 证明。这样配对码本身不会出现在链路上——否则信令通道一旦被嗅探
    /// （局域网 ws://、或未配 TLS 的公网），码就直接泄露，等于没有准入。
    AuthRequired { from: String, to: String, nonce: String },
    /// 配对应答（viewer -> host）：proof = HMAC-SHA256(配对码, nonce) 的十六进制
    AuthResponse { from: String, to: String, proof: String },
    /// 配对通过（host -> viewer）：附后续可复用的信任令牌
    AuthGranted { from: String, to: String, token: String },
    /// 配对失败（host -> viewer）
    ///
    /// `reason` 只写通用话术（如「配对码不正确」），不能回带任何可用于
    /// 试探的信息（比如「前三位对了」），否则等于把配对码变成可暴力枚举的。
    AuthDenied { from: String, to: String, reason: String },
    /// SDP offer/answer
    Sdp {
        from: String,
        to: String,
        sdp_type: SdpType,
        sdp: String,
    },
    /// ICE candidate
    Ice {
        from: String,
        to: String,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: i32,
    },
    /// 心跳
    Ping,
    /// 心跳回应
    Pong,
    /// 错误
    Error { message: String },
    /// 注册成功后服务器下发的 TURN 时效凭证（coturn REST 风格：
    /// username = "<unix 过期时间戳>:<设备id>"，
    /// credential = Base64(HMAC-SHA1(共享密钥, username))，到期自动失效）。
    ///
    /// 为什么走下发而不是写死在客户端源码：长期凭证入源码 = 开源即泄露
    /// （见 docs/OPEN_SOURCE_AUDIT.md P0）。下发时效凭证后，源码零秘密，
    /// 泄露的凭证几小时后自动作废。
    TurnCredentials {
        urls: Vec<String>,
        realm: String,
        username: String,
        credential: String,
        ttl: u64,
    },
}

/// 设备信息（用于设备列表）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub device_id: String,
    pub role: Role,
    pub platform: Option<String>,
    /// 电脑自己上报的名字（主机名）。缺省时展示层退回 `device_id`。
    ///
    /// `default` 必须写：老服务器的设备列表里没有这个字段，不写的话
    /// 新 App 解析老响应会直接失败。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl SignalMsg {
    /// 返回消息的目标设备 ID（若有），供信令服务器转发。
    pub fn target(&self) -> Option<&str> {
        match self {
            SignalMsg::Connect { to, .. }
            | SignalMsg::AuthRequired { to, .. }
            | SignalMsg::AuthResponse { to, .. }
            | SignalMsg::AuthGranted { to, .. }
            | SignalMsg::AuthDenied { to, .. }
            | SignalMsg::Sdp { to, .. }
            | SignalMsg::Ice { to, .. } => Some(to),
            _ => None,
        }
    }
}

/// 设备配对：挑战应答 + 信任令牌。
///
/// 为什么不用「直接发配对码让被控端比对」这种直白做法：那会把配对码本身
/// 放在链路上。局域网是 ws://、公网未配 TLS 时也是明文，嗅探到码就等于绕过
/// 全部准入——安全设计里最忌讳的就是「凭据在信道上裸奔」。
///
/// 改成挑战应答后，链路上只有 nonce 与 HMAC 结果：
///   - nonce 是一次性随机数，重放同一个 proof 无效（被控端每次发新的）；
///   - proof 只是 HMAC(配对码, nonce)，无法反推配对码；
///   - 配对码始终留在两端本地，不落链路、不落日志。
///
/// 配对成功后被控端签发信任令牌，下次连接直接带令牌，不必再输码。
pub mod pairing {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    /// 配对码位数：6 位。
    ///
    /// 这是「手抄一次的容错」与「在线暴破成本」的折中。10^6 的空间单独看
    /// 不够大，所以被控端必须限速（见 host 侧实现），否则每秒上百次尝试下
    /// 几分钟就能穷举完。
    pub const CODE_LEN: usize = 6;

    /// 生成 n 字节的随机十六进制串（2n 个字符）。
    ///
    /// 直接用系统随机数。曾经想过用时间戳糊弄——那会让 nonce 可预测，
    /// 攻击者能预先算好 proof 重放，等于把挑战应答退化成明文口令。
    pub fn random_hex(bytes: usize) -> String {
        let mut buf = vec![0u8; bytes];
        // 随机数拿不到是极严重的情况（配出的码/令牌可被预测），
        // 这里宁可 panic 也不能退化成弱随机——那比直接崩掉危险得多。
        getrandom::getrandom(&mut buf).expect("系统随机数不可用");
        buf.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// 生成一位十进制数字（配对码的组成单元）。
    fn random_digit() -> char {
        // 用拒绝采样避开取模偏置：256 % 10 != 0 时，直接取模会让 0~5 出现得
        // 比 6~9 频繁，缩小有效搜索空间。这里是 6 位小空间，偏置更不该留。
        loop {
            let mut one = [0u8; 1];
            getrandom::getrandom(&mut one).expect("系统随机数不可用");
            if (one[0] as usize) < 250 {
                return char::from(b'0' + (one[0] % 10));
            }
        }
    }

    /// 生成配对码（纯数字，便于在电脑上读、在手机上敲）。
    pub fn generate_code() -> String {
        (0..CODE_LEN).map(|_| random_digit()).collect()
    }

    /// 计算配对证明：HMAC-SHA256(配对码, nonce) 的十六进制（小写）。
    ///
    /// 两端必须逐字一致：算法、编码、大小写任何一处不同，都会表现为
    /// 「配对码明明输对了却配对失败」，且不报错、只静默拒绝，极难排查。
    pub fn proof(code: &str, nonce: &str) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(code.as_bytes()).expect("HMAC 接受任意长度密钥");
        mac.update(nonce.as_bytes());
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// 恒定时间比较：避免通过响应时间侧信道逐字节猜出证明。
    ///
    /// 普通字符串比较会在第一个不同字节处提前返回，攻击者可据此一位一位地
    /// 试探。这里固定遍历全部字节，把差异累积起来最后判断。
    pub fn verify(expected: &str, got: &str) -> bool {
        let (a, b) = (expected.as_bytes(), got.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        let mut diff = 0u8;
        for i in 0..a.len() {
            diff |= a[i] ^ b[i];
        }
        diff == 0
    }
}

/// 信令接入证明：证明「这台设备有权占用它声称的 device_id」。
///
/// 与 `pairing` 的分工要说清，两者管的不是一件事：
///   - `pairing`（设备配对）管「控制端能不能操作这台被控端」，发生在被控端，
///     决定要不要开始推屏、接不接受输入。
///   - `access`（信令接入）管「这台设备能不能用信令服务器转发消息」，
///     发生在服务器，决定你能否出现在设备列表里、能否发信令。
///
/// 少了 access 这层，服务器对谁都不设防：任何人连上 8443 就能列出你家所有
/// 在线电脑（这本身就是隐私泄露），还能用别人的 device_id 注册把信令劫走。
/// 配对码挡不住这些——它保护的环节在更后面。
pub mod access {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    /// 计算注册证明：HMAC-SHA256(接入密钥, "register|" + device_id) 的小写十六进制。
    ///
    /// 用 HMAC 而不是让客户端直接发密钥：链路上只出现一个与 device_id 绑定的
    /// 值，嗅探到也无法反推密钥，也不能拿去注册另一个 device_id。
    ///
    /// "register|" 前缀是域分隔——将来若给别的动作也加证明，同一串 (密钥, id)
    /// 不会在两个动作间通用，避免一处泄露、处处可用。
    ///
    /// 两端必须逐字一致：算法、前缀、大小写任一不同，都会表现为「密钥明明
    /// 没填错却注册不上」，且不报错，只是被静默拒绝。
    pub fn register_proof(key: &str, device_id: &str) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC 接受任意长度密钥");
        mac.update(b"register|");
        mac.update(device_id.as_bytes());
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// 生成接入密钥（服务器首次启动、用户没指定时用）。
    pub fn generate_key() -> String {
        super::pairing::random_hex(24)
    }

    /// 校验注册证明，恒定时间比较（见 `pairing::verify`）。
    pub fn verify_register(key: &str, device_id: &str, got: &str) -> bool {
        super::pairing::verify(&register_proof(key, device_id), got)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn proof_is_stable_and_bound_to_device() {
            let a = register_proof("secret", "host-mac-01");
            assert_eq!(a, register_proof("secret", "host-mac-01"), "同输入必须同输出");
            assert_ne!(a, register_proof("secret", "host-win-01"), "换 device_id 证明必须变");
            assert_ne!(a, register_proof("other", "host-mac-01"), "换密钥证明必须变");
            assert_eq!(a.len(), 64, "SHA256 的十六进制应是 64 字符");
        }

        #[test]
        fn verify_rejects_wrong_proof() {
            let key = "k";
            let good = register_proof(key, "dev");
            assert!(verify_register(key, "dev", &good));
            assert!(!verify_register(key, "dev", ""));
            assert!(!verify_register(key, "other", &good), "证明不能跨 device_id 复用");
        }
    }
}

/// 修饰键位掩码（用于 `InputEvent::Key`）。
///
/// 单键注入无法表达 ⌘C / ⌘V / ⌘Tab 这类组合——必须把修饰键作为事件的一个维度，
/// 否则只会在被控端输入字符 "c"。用位掩码而不是数组，是为了保持协议紧凑，
/// 且能在无修饰键时完全省略该字段。
pub mod modifier {
    /// Shift
    pub const SHIFT: u8 = 1 << 0;
    /// Control
    pub const CONTROL: u8 = 1 << 1;
    /// Option / Alt
    pub const ALT: u8 = 1 << 2;
    /// Command（macOS）/ Win（Windows）
    pub const COMMAND: u8 = 1 << 3;
}

/// 输入事件：控制端 -> 被控端，经 WebRTC DataChannel 传输。
/// 坐标使用归一化值(0.0-1.0)，被控端按屏幕分辨率换算为真实像素。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputEvent {
    /// 鼠标移动
    MouseMove { x: f32, y: f32 },
    /// 鼠标按键（button: 0=左 1=右 2=中）
    MouseButton { x: f32, y: f32, button: u8, down: bool },
    /// 滚轮
    Scroll { dx: f32, dy: f32 },
    /// 键盘（keycode 使用平台无关扫描码，被控端转换）
    ///
    /// `modifiers` 是 `modifier` 模块里各位的按位或；`#[serde(default)]`
    /// 让旧版控制端（不带该字段）仍能正常解析，不至于因协议升级直接断连。
    Key {
        keycode: u32,
        down: bool,
        #[serde(default)]
        modifiers: u8,
    },
    /// 文本输入
    Text { content: String },
}

/// 控制端上报的链路质量：经 DataChannel 每秒一条，被控端据此自适应调整视频码率。
///
/// 为什么走 DataChannel 而不是信令：信令要绕云端多转发一圈，既慢又白占公网流量；
/// 而这条控制通道本来就跑在同一对 ICE 候选上，它的往返开销与视频同路，最能反映
/// 「此刻这条视频链路到底堵不堵」。
///
/// 为什么判据是丢包率而不是 RTT：丢包率直接说明「发得比链路能吃下的多」，而 RTT
/// 混着排队延迟，在只有一路视频流时它更慢也更抖，拿来做阈值容易误判。
///
/// 字段全带 `#[serde(default)]`：老版本被控端读到未知 JSON 只会解析失败并打一条
/// 警告，不会断流——协议升级不该把正在用的连接搞断。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct NetReport {
    /// 最近一秒的丢包率（0.0~1.0，取不到的轮次填 0）。
    pub loss: f32,
    /// 最近一秒的平均 RTT（毫秒，取不到时填 0），仅用于日志。
    #[serde(default)]
    pub rtt_ms: f32,
    /// 接收端估计的网络抖动（毫秒，取不到时填 0），仅用于日志。
    #[serde(default)]
    pub jitter_ms: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_msg_roundtrip() {
        let msg = SignalMsg::Register {
            device_id: "host-1".into(),
            role: Role::Host,
            key_proof: None,
            name: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        // 不带接入证明时该字段应被省略，老服务器才认得这条报文
        assert!(!json.contains("key_proof"), "None 时不应序列化 key_proof");
        let json = serde_json::to_string(&msg).unwrap();
        let back: SignalMsg = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, SignalMsg::Register { .. }));
    }

    #[test]
    fn sdp_msg_roundtrip() {
        let msg = SignalMsg::Sdp {
            from: "h".into(),
            to: "v".into(),
            sdp_type: SdpType::Offer,
            sdp: "v=0...".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"sdp_type\":\"offer\""));
        let back: SignalMsg = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, SignalMsg::Sdp { sdp_type: SdpType::Offer, .. }));
    }

    #[test]
    fn ice_msg_roundtrip() {
        let msg = SignalMsg::Ice {
            from: "h".into(),
            to: "v".into(),
            candidate: "candidate:1 1 udp 2130706431 192.168.1.1 5000 typ host".into(),
            sdp_mid: Some("0".into()),
            sdp_mline_index: 0,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: SignalMsg = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, SignalMsg::Ice { sdp_mline_index: 0, .. }));
    }

    #[test]
    fn input_event_roundtrip() {
        let ev = InputEvent::MouseMove { x: 0.5, y: 0.5 };
        let json = serde_json::to_string(&ev).unwrap();
        let back: InputEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, InputEvent::MouseMove { .. }));
    }

    /// 被控端必须能解析 iOS 端手写的键盘报文。
    ///
    /// iOS 侧（SignalMessage.swift）的 encode 是手写的，没有 serde 保证。
    /// 只要 kind 字符串或 modifiers 位掩码有一处对不上，⌘C 这类组合键就会
    /// 静默失效——不报错，只是没反应。所以这里用字面 JSON 锁死两端约定。
    #[test]
    fn key_event_parses_ios_wire_format() {
        // 无修饰键：iOS 端省略 modifiers 字段
        let plain = r#"{"kind":"key","keycode":41,"down":true}"#;
        let ev: InputEvent = serde_json::from_str(plain).unwrap();
        assert!(
            matches!(ev, InputEvent::Key { keycode: 41, down: true, modifiers: 0 }),
            "省略 modifiers 时应默认 0（兼容旧客户端）"
        );

        // 带 ⌘：iOS 端 InputModifier.command.rawValue == 8
        let with_cmd = r#"{"kind":"key","keycode":6,"down":true,"modifiers":8}"#;
        let ev: InputEvent = serde_json::from_str(with_cmd).unwrap();
        match ev {
            InputEvent::Key { keycode, modifiers, .. } => {
                assert_eq!(keycode, 6, "C 键的 HID 码应为 0x06");
                assert_eq!(modifiers, modifier::COMMAND, "⌘ 的位掩码应为 8");
            }
            other => panic!("应解析为 Key，实际为 {other:?}"),
        }
    }

    /// 位掩码取值是两端手写对齐的，锁死防止单边改动
    #[test]
    fn modifier_bitmask_matches_ios() {
        assert_eq!(modifier::SHIFT, 1);
        assert_eq!(modifier::CONTROL, 2);
        assert_eq!(modifier::ALT, 4);
        assert_eq!(modifier::COMMAND, 8);
    }

    #[test]
    fn target_extraction() {
        let msg = SignalMsg::Connect { from: "v".into(), to: "h".into(), token: None };
        assert_eq!(msg.target(), Some("h"));
        assert_eq!(SignalMsg::Ping.target(), None);
    }

    #[test]
    fn device_list_roundtrip() {
        let msg = SignalMsg::DeviceList {
            devices: vec![DeviceInfo {
                device_id: "host-1".into(),
                role: Role::Host,
                platform: Some("macos".into()),
                name: Some("my-MacBook-Pro".into()),
            }],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: SignalMsg = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, SignalMsg::DeviceList { .. }));
    }
}
