import AppKit

// Makuris 菜单栏助手：把被控端的配对码常驻显示在系统菜单栏。
//
// 为什么需要它：被控端是 LSUIElement=true 的纯后台进程，没有窗口、没有
// Dock 图标，配对码此前只写在 ~/Library/Application Support/Makuris/
// pairing-code.txt 和日志里——普通用户根本无从查看，而 iOS 配对页又提示
// 「在电脑端的 Makuris 里查看配对码」，形成断链。
//
// 由 makuris-host 在 .app bundle 内启动，参数：
//   --parent <pid>   宿主进程 pid，宿主退出即自退（图标不残留）
//   --code <码>      启动时的配对码
//   --code-file <路径> 码文件；定时重读，码被重置后菜单栏自动跟着变
//
// 职责只有展示：不参与配对逻辑，也不提供关闭被控端的入口
// （退出助手只是摘掉图标，服务照跑；真正退出请杀 makuris-host）。

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var item: NSStatusItem!
    private var timer: Timer?
    private let parentPid: pid_t
    private let codeFile: String
    private var lastCode: String = ""

    init(parentPid: pid_t, codeFile: String, initialCode: String) {
        self.parentPid = parentPid
        self.codeFile = codeFile
        self.lastCode = initialCode
        super.init()
    }

    func applicationDidFinishLaunching(_ note: Notification) {
        // 菜单栏只放一枚图标：位置有限，长文字「Makuris · 码」会挤占别人的
        // 空间（用户反馈）。配对码收进下拉菜单首项，点开即见。
        item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        if let button = item.button {
            // 用户反馈默认图标太小：SF Symbol 不设 pointSize 时按正文尺寸
            // （约 15pt）绘制，在 22pt 高的菜单栏里显得孤小。显式配 18pt
            // semibold，视觉占满菜单栏又不至于被裁。
            let config = NSImage.SymbolConfiguration(pointSize: 18, weight: .semibold)
            let img = NSImage(systemSymbolName: "m.circle.fill",
                              accessibilityDescription: "Makuris 配对")?
                .withSymbolConfiguration(config)
            img?.isTemplate = true
            button.image = img
        }
        rebuildMenu()

        timer = Timer.scheduledTimer(withTimeInterval: 2.0, repeats: true) { [weak self] _ in
            self?.tick()
        }
    }

    private func tick() {
        // 宿主没了（被控端退出/崩溃，本进程会被 launchd 收养、ppid 改变）
        // → 立刻退出，图标绝不能在被控端已停时残留误导用户
        if getppid() != parentPid {
            NSApp.terminate(nil)
            return
        }
        // 码可能被重置（删文件重启即换码），以文件为准保持同步
        if let s = try? String(contentsOfFile: codeFile, encoding: .utf8) {
            let code = s.trimmingCharacters(in: .whitespacesAndNewlines)
            if !code.isEmpty && code != lastCode {
                lastCode = code
                rebuildMenu()
            }
        }
    }

    /// 下拉菜单：首项直接展示配对码（点一下即复制），随后是码文件与退出。
    /// 菜单栏本体只留图标（用户反馈：位置有限，长文字挤占其它图标）。
    private func rebuildMenu() {
        let menu = NSMenu()
        let codeItem = NSMenuItem(title: "配对码 \(lastCode)（点击复制）",
                                  action: #selector(copyCode), keyEquivalent: "c")
        codeItem.target = self
        menu.addItem(codeItem)
        let revealItem = NSMenuItem(title: "在访达中显示码文件",
                                    action: #selector(revealFile), keyEquivalent: "")
        revealItem.target = self
        menu.addItem(revealItem)
        menu.addItem(.separator())
        menu.addItem(NSMenuItem(title: "退出菜单栏图标",
                                action: #selector(NSApplication.terminate(_:)),
                                keyEquivalent: "q"))
        item.menu = menu
    }

    @objc private func copyCode() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(lastCode, forType: .string)
    }

    @objc private func revealFile() {
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: codeFile)])
    }
}

func arg(_ name: String, _ argv: [String]) -> String? {
    if let i = argv.firstIndex(of: name), i + 1 < argv.count { return argv[i + 1] }
    return nil
}

// 参数优先从环境变量读：host 侧用 `open --env` 拉起（LaunchServices
// 启动 bundle 无法可靠传 argv），open 会把 --env 键值注入子进程环境。
// argv 形式保留作兜底，供开发时终端直接跑二进制调试。
let env = ProcessInfo.processInfo.environment
let argv = CommandLine.arguments
guard let p = env["MAKURIS_MENUBAR_PARENT"] ?? arg("--parent", argv),
      let pid = pid_t(p),
      let f = env["MAKURIS_MENUBAR_CODE_FILE"] ?? arg("--code-file", argv),
      let c = env["MAKURIS_MENUBAR_CODE"] ?? arg("--code", argv) else {
    FileHandle.standardError.write("用法: makuris-menubar --parent <pid> --code <码> --code-file <路径>\n".data(using: .utf8)!)
    exit(2)
}
let app = NSApplication.shared
let delegate = AppDelegate(parentPid: pid, codeFile: f, initialCode: c)
app.delegate = delegate
// accessory 必须在 run() 之前设定：放进 applicationDidFinishLaunching 会与
// LaunchServices 继承自宿主 bundle 的 LSUIElement 状态打架，status item
// 会「进程活着但菜单栏没有」——11:32 部署实测正是这个现象
// （System Events 查询该进程只有 1 个 menu bar，状态栏项根本没创建）。
// 打包成独立 .app 后本包 Info.plist 已声明 LSUIElement，这里再设一次
// 保证 cargo run 直调裸二进制时行为一致。
app.setActivationPolicy(.accessory)
app.run()
