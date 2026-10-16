import Foundation

/// 用户手动保存的电脑条目（公网/跨网段场景的兜底）。
///
/// 为什么需要它：Bonjour 只在同一局域网内有效。一旦手机在外网、
/// 或电脑跨网段，就必须知道对方的固定地址。这类地址用户是会越来越多的
/// （家里的 mac、公司的 windows、云上的服务器），所以不能像之前那样
/// 只留一个输入框——那样存了新的就冲掉旧的。
///
/// 因此这里做成一个可增删的列表，每条自带名字，界面上好认。
struct SavedComputer: Identifiable, Codable, Equatable {
    var id: UUID
    /// 用户可读的名字，如「家里的 Mac」。留空时界面回退显示地址。
    var name: String
    /// 用户填的原始地址（可能只是 IP，也可能带端口/路径）
    var address: String
    /// 这条地址所属信令服务器的接入密钥。留空表示沿用设置里的默认密钥。
    ///
    /// 密钥跟着地址走，是因为两者本就是一体的：地址指向哪台服务器，密钥就是哪台
    /// 服务器的。原先只有全局一个密钥，隐含了「只有一个信令服务器」的假设；用户加
    /// 第二台服务器时必然冲突——改全局会把第一台弄坏，不改则第二台认证不过。所以
    /// 每条地址各自带一份，全局那个降级为默认值。
    var accessKey: String = ""

    init(id: UUID = UUID(), name: String = "", address: String, accessKey: String = "") {
        self.id = id
        self.name = name
        self.address = address
        self.accessKey = accessKey
    }

    /// 旧版本存的数据里没有 accessKey 字段，缺字段时按「用默认密钥」处理。
    ///
    /// 合成的解码器遇到缺字段会直接抛错，那会让整个列表都读不出来——用户看到的是
    /// 已保存的电脑全没了。所以这里显式用 decodeIfPresent 兜住。
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(UUID.self, forKey: .id)
        name = try c.decodeIfPresent(String.self, forKey: .name) ?? ""
        address = try c.decode(String.self, forKey: .address)
        accessKey = try c.decodeIfPresent(String.self, forKey: .accessKey) ?? ""
    }

    /// 实际连接时该用的密钥：本条填了就用本条的，没填回退到默认值。
    func effectiveAccessKey(fallback: String) -> String {
        let own = accessKey.trimmingCharacters(in: .whitespacesAndNewlines)
        return own.isEmpty ? fallback : own
    }

    /// 是否单独配了密钥（界面上据此决定要不要标注）。
    var hasOwnAccessKey: Bool {
        !accessKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    /// 界面主标题
    var displayName: String {
        name.isEmpty ? address : name
    }

    /// 规范化后的可连接地址
    var normalizedURL: String {
        AppState.normalize(address)
    }
}

/// 已保存电脑列表的持久化存储。
final class SavedComputersStore: ObservableObject {
    @Published private(set) var computers: [SavedComputer] = []

    private static let key = "makuris.savedComputers"

    init() {
        load()
    }

    func add(name: String, address: String, accessKey: String = "") {
        let trimmed = address.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        // 同一地址不重复添加，否则列表里会出现两条一模一样的
        guard !computers.contains(where: {
            $0.normalizedURL == AppState.normalize(trimmed)
        }) else { return }
        computers.append(SavedComputer(name: name, address: trimmed, accessKey: accessKey))
        save()
    }

    func update(_ computer: SavedComputer) {
        guard let idx = computers.firstIndex(where: { $0.id == computer.id }) else { return }
        computers[idx] = computer
        save()
    }

    func remove(_ computer: SavedComputer) {
        computers.removeAll { $0.id == computer.id }
        save()
    }

    private func load() {
        guard let data = UserDefaults.standard.data(forKey: Self.key),
              let list = try? JSONDecoder().decode([SavedComputer].self, from: data) else {
            // 迁移：早期版本（更名前叫 OpenDesk）只存了单个地址，
            // 把它转成列表里的第一条，老用户升级后不必重新填一遍
            if let legacy = UserDefaults.standard.string(forKey: "makuris.serverAddress"),
               !legacy.isEmpty {
                computers = [SavedComputer(name: L10n.t(.myComputer), address: legacy)]
                save()
            }
            return
        }
        computers = list
    }

    private func save() {
        guard let data = try? JSONEncoder().encode(computers) else { return }
        UserDefaults.standard.set(data, forKey: Self.key)
    }
}