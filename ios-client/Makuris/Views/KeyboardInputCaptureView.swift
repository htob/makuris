import SwiftUI
import UIKit

/// 远程键盘输入捕获视图 + 贴键盘的快捷键条。
///
/// 为什么要重做：早先的实现用 `.sheet` 弹出 280pt 的输入面板，面板里再唤起系统
/// 键盘，两层叠加把整个远程画面盖住——而远程桌面的核心场景恰恰是「一边看着远端
/// 屏幕一边打字」，被遮住就等于没法用。这里改为在画面上挂一个几乎不可见的输入
/// 控件，由它直接唤起系统键盘：画面不再被模态面板遮挡，只让出键盘自身的高度。
///
/// 常用键与组合键放进 inputAccessoryView——它贴在键盘上沿，属于键盘的既有高度，
/// 不在屏幕上额外占地。
final class KeyboardInputCaptureView: UIView, UITextFieldDelegate {

    /// 文本上屏回调（已排除拼音/手写合成中的中间态）
    var onText: ((String) -> Void)?
    /// 功能键回调；keycode 为 USB HID 码，modifiers 为 InputModifier 位掩码
    var onKey: ((UInt32, UInt8) -> Void)?
    /// 键盘高度变化（供画面自适应让位）
    var onKeyboardHeightChange: ((CGFloat) -> Void)?

    private let field = UITextField()
    private lazy var accessory = KeyboardAccessoryBar(
        onKey: { [weak self] code, mods in self?.onKey?(code, mods) },
        onDismiss: { [weak self] in self?.resignKeyboard() }
    )

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .clear

        // 输入框几乎不可见但仍参与响应链：完全隐藏（isHidden 或 alpha=0）的视图
        // 无法成为第一响应者，inputAccessoryView 也不会显示，所以留 1% 透明度。
        field.frame = CGRect(x: 0, y: 0, width: 1, height: 1)
        field.alpha = 0.01
        field.delegate = self
        // 远程输入要原样送到被控端，本机的联想/纠错/智能标点全部关掉，
        // 否则打字时会被本机悄悄改写内容。
        field.autocorrectionType = .no
        field.autocapitalizationType = .none
        field.spellCheckingType = .no
        field.smartQuotesType = .no
        field.smartDashesType = .no
        field.smartInsertDeleteType = .no
        field.inputAccessoryView = accessory
        addSubview(field)

        NotificationCenter.default.addObserver(
            self,
            selector: #selector(keyboardFrameChanged(_:)),
            name: UIResponder.keyboardWillChangeFrameNotification,
            object: nil)
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(keyboardDidHide(_:)),
            name: UIResponder.keyboardDidHideNotification,
            object: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override var canBecomeFirstResponder: Bool { true }

    var isKeyboardVisible: Bool { field.isFirstResponder }

    /// 唤起系统键盘
    func showKeyboard() {
        field.becomeFirstResponder()
    }

    /// 收起键盘
    func resignKeyboard() {
        field.resignFirstResponder()
    }

    func toggleKeyboard() {
        isKeyboardVisible ? resignKeyboard() : showKeyboard()
    }

    // MARK: - 逐字上屏

    /// 拦截输入并立即发送，同时不把字符写入 field。
    ///
    /// 返回 false 是有意的：field 只作输入捕获用，不承载内容。若让它累积文本，
    /// 中文输入法上屏时会整段替换 marked text，导致同一批字符被重复发送。
    func textField(_ textField: UITextField,
                   shouldChangeCharactersIn range: NSRange,
                   replacementString string: String) -> Bool {
        // 合成中（拼音候选未定、手写未上屏）不发，避免把半成品送到远端
        if textField.markedTextRange != nil { return false }

        if string.isEmpty {
            // 删除键：range.length 表示被删掉的字符数
            if range.length > 0 {
                for _ in 0..<range.length {
                    onKey?(HIDKey.backspace, 0)
                }
            }
            return false
        }

        // 粘贴等多字符输入逐字发送，让远端逐字符生效（与真实键盘行为一致）
        for ch in string {
            if ch == "\n" {
                onKey?(HIDKey.enter, 0)
            } else {
                onText?(String(ch))
            }
        }
        return false
    }

    // MARK: - 键盘高度

    /// 键盘尺寸/位置变化（弹出、切换输入法、收起的中间态都会触发）。
    /// 通过 NotificationCenter 注册是安全的：它按签名回调，会真的传 Notification。
    /// 注意不要改用 UITextField.addTarget(_, action:, for:) 来监听——那条路径会把
    /// UITextField 自身当作参数回传，与这里的 Notification 签名不匹配，
    /// 运行时会因 unrecognized selector 直接崩溃。
    @objc private func keyboardFrameChanged(_ note: Notification) {
        reportKeyboardHeight(note)
    }

    @objc private func keyboardDidHide(_ note: Notification) {
        onKeyboardHeightChange?(0)
    }

    /// 计算键盘盖住了「远程画面」多少。
    ///
    /// 基准必须是父视图（RemoteScreenUIView，即整屏画面），不能用本控件自己：
    /// 本控件是左上角 1x1 的隐形视图，拿它的底边去比键盘上沿会恒得 0，
    /// 让位逻辑就永远不会触发。故这里换算父视图的底边。
    private func reportKeyboardHeight(_ note: Notification) {
        guard let end = note.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect,
              let kbWindow = window else { return }
        let ref: UIView = superview ?? kbWindow
        let refBottomInWindow = ref.convert(ref.bounds, to: kbWindow).maxY
        // end 是屏幕坐标，直接与窗口坐标比较（window 坐标系原点即屏幕原点）
        let overlap = max(0, refBottomInWindow - end.minY)
        onKeyboardHeightChange?(overlap)
    }
}

/// 贴在系统键盘上沿的快捷键条（作为 inputAccessoryView）。
///
/// 横向可滚动，所以功能键数量不受屏幕宽度限制；布局用手写 frame 而非 Auto Layout，
/// 因为 inputAccessoryView 对自动布局的高度推导并不可靠。
final class KeyboardAccessoryBar: UIView {

    private let onKey: (UInt32, UInt8) -> Void
    private let onDismiss: () -> Void

    private let scrollView = UIScrollView()
    private let stack = UIStackView()
    private let dismissButton = UIButton(type: .system)
    private static let height: CGFloat = 44
    private static let dismissWidth: CGFloat = 56

    init(onKey: @escaping (UInt32, UInt8) -> Void, onDismiss: @escaping () -> Void) {
        self.onKey = onKey
        self.onDismiss = onDismiss
        super.init(frame: CGRect(x: 0, y: 0, width: UIScreen.main.bounds.width, height: Self.height))
        autoresizingMask = .flexibleWidth
        setup()
    }

    required init?(coder: NSCoder) { fatalError() }

    override func layoutSubviews() {
        super.layoutSubviews()
        let w = bounds.width
        let h = bounds.height
        dismissButton.frame = CGRect(x: w - Self.dismissWidth, y: 0,
                                     width: Self.dismissWidth, height: h)
        scrollView.frame = CGRect(x: 0, y: 0, width: max(0, w - Self.dismissWidth), height: h)
        stack.frame = CGRect(x: 8, y: 0, width: stack.systemLayoutSizeFitting(
            CGSize(width: CGFloat.greatestFiniteMagnitude, height: h)).width, height: h)
        scrollView.contentSize = CGSize(width: stack.frame.width + 16, height: h)
    }

    private func setup() {
        // 半透明背景跟随系统，深浅色模式下都可读
        backgroundColor = .secondarySystemBackground

        stack.axis = .horizontal
        stack.spacing = 6
        stack.alignment = .center

        for spec in Self.keySpecs {
            stack.addArrangedSubview(makeKeyButton(spec))
        }

        scrollView.addSubview(stack)
        scrollView.showsHorizontalScrollIndicator = false
        addSubview(scrollView)

        var cfg = UIButton.Configuration.plain()
        cfg.title = "收起"
        cfg.baseForegroundColor = .systemBlue
        dismissButton.configuration = cfg
        dismissButton.addAction(UIAction { [weak self] _ in self?.onDismiss() },
                                for: .touchUpInside)
        addSubview(dismissButton)
    }

    private func makeKeyButton(_ spec: KeySpec) -> UIButton {
        var cfg = UIButton.Configuration.gray()
        cfg.title = spec.title
        cfg.baseForegroundColor = .label
        cfg.contentInsets = NSDirectionalEdgeInsets(top: 4, leading: 9, bottom: 4, trailing: 9)
        let btn = UIButton(type: .system, primaryAction: UIAction { [weak self] _ in
            self?.onKey(spec.code, spec.modifiers)
        })
        btn.configuration = cfg
        btn.titleLabel?.font = .systemFont(ofSize: 14, weight: .medium)
        return btn
    }

    private struct KeySpec {
        let title: String
        let code: UInt32
        let modifiers: UInt8
    }

    /// 键位按使用频率排：整键在前，组合键在后。
    private static let keySpecs: [KeySpec] = [
        KeySpec(title: "esc", code: HIDKey.escape, modifiers: 0),
        KeySpec(title: "tab", code: HIDKey.tab, modifiers: 0),
        KeySpec(title: "⇧tab", code: HIDKey.tab, modifiers: InputModifier.shift.rawValue),
        KeySpec(title: "←", code: HIDKey.left, modifiers: 0),
        KeySpec(title: "↑", code: HIDKey.up, modifiers: 0),
        KeySpec(title: "↓", code: HIDKey.down, modifiers: 0),
        KeySpec(title: "→", code: HIDKey.right, modifiers: 0),
        KeySpec(title: "⌫", code: HIDKey.backspace, modifiers: 0),
        KeySpec(title: "⏎", code: HIDKey.enter, modifiers: 0),
        KeySpec(title: "⌘C", code: HIDKey.c, modifiers: InputModifier.command.rawValue),
        KeySpec(title: "⌘V", code: HIDKey.v, modifiers: InputModifier.command.rawValue),
        KeySpec(title: "⌘X", code: HIDKey.x, modifiers: InputModifier.command.rawValue),
        KeySpec(title: "⌘Z", code: HIDKey.z, modifiers: InputModifier.command.rawValue),
        KeySpec(title: "⌘A", code: HIDKey.a, modifiers: InputModifier.command.rawValue),
        KeySpec(title: "⌘F", code: HIDKey.f, modifiers: InputModifier.command.rawValue),
        KeySpec(title: "⌘W", code: HIDKey.w, modifiers: InputModifier.command.rawValue),
        KeySpec(title: "⌘␣", code: HIDKey.space, modifiers: InputModifier.command.rawValue),
        KeySpec(title: "⌘⇥", code: HIDKey.tab, modifiers: InputModifier.command.rawValue),
    ]
}