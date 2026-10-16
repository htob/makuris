import Combine
import SwiftUI
import WebRTC

/// 远程键盘控制器：桥接 SwiftUI 按钮与 UIKit 侧的输入捕获控件。
///
/// RemoteScreenView 是 UIViewRepresentable，SwiftUI 层拿不到内部那个 UIView，
/// 故用这个轻量对象转一手，避免把键盘按钮硬塞进 UIKit 视图层。
final class RemoteKeyboardController: ObservableObject {
    fileprivate weak var capture: KeyboardInputCaptureView?

    func toggle() { capture?.toggleKeyboard() }
    func dismiss() { capture?.resignKeyboard() }
}

extension SessionStats {
    /// 展示用的短文案。取不到就显示「—」：写 0 会被当成
    /// 「零延迟」「帧率归零」，把「暂无数据」误报成「已经卡死」。
    var rttText: String {
        guard let rttMs else { return "—" }
        return "\(Int(rttMs.rounded()))ms"
    }

    var fpsText: String {
        guard let fps else { return "—" }
        return "\(Int(fps.rounded()))fps"
    }

    var bitrateText: String {
        guard let bitrateMbps else { return "—" }
        // 码率波动大，保留一位小数即可，不必给到小数点后两位
        return String(format: "%.1fM", bitrateMbps)
    }

    var resolutionText: String {
        resolution ?? "—"
    }

    /// 诊断行：把「网络 RTT 之外还白等了多少」如实摆出来。
    ///
    /// 为「显示延迟 100ms、体感却有 1 秒」加的探针：抖动缓冲是那笔差额最可能
    /// 的藏身处，先把它量出来再决定砍哪里，避免凭印象优化。两项都取不到时
    /// 返回 nil，界面直接不显示这一行，不留空壳。
    var diagnosticText: String? {
        var first: [String] = []
        if let jb = jitterBufferMs { first.append("缓冲 \(Int(jb.rounded()))ms") }
        if let d = decodeMs { first.append("解码 \(Int(d.rounded()))ms") }

        // 第二行专测「抗丢包」这条路：丢包数 / 请求重传次数 / 实收重传包 / 抖动估计。
        // 重传长期为 0 而 NACK 很高，就说明请求发出去了却没有补包回来（缺 RTX）。
        var second: [String] = []
        if let lost = packetsLost { second.append("丢包 \(Int(lost.rounded()))") }
        if let nack = nackCount { second.append("NACK \(Int(nack.rounded()))") }
        if let rtx = retransmittedPacketsReceived { second.append("重传 \(Int(rtx.rounded()))") }
        if let jit = jitterMs { second.append("抖动 \(Int(jit.rounded()))ms") }

        let lines = [first, second]
            .filter { !$0.isEmpty }
            .map { $0.joined(separator: " · ") }
        return lines.isEmpty ? nil : lines.joined(separator: "\n")
    }
}

/// 状态条摆位：算出视频黑边有多厚，决定状态条放哪、多大。
///
/// 画面是 scaleAspectFit 的，四周必有黑边：竖屏时黑边在上下（很厚），
/// 横屏时在左右（较窄）。把状态条放进黑边，既不遮挡画面，又用上了那块空白。
///
/// 坐标一律用「全局坐标」（与画面视图同一个坐标系）。早先用
/// ZStack(alignment:) + padding 摆位，可 ZStack 处在被安全区裁剪过的坐标系里，
/// 而这里又额外扣了一遍安全区——安全区被重复扣减，横屏时状态条就被推进画面
/// 与屏幕内容重叠。现在改为直接算出中心点、交给 .position() 落位，
/// 谁也不必猜容器在哪个坐标系里。
struct StatusBarLayout {
    /// 竖排（横屏贴侧边）还是横排（竖屏贴上下黑边）
    let vertical: Bool
    /// 状态条自身尺寸（未指定的一边由内容撑开，这里给 0 表示不约束）
    let size: CGSize
    let center: CGPoint
    /// 空间局促时用更透的底（仍保证白字可读），避免把底下的画面挡死
    let overlay: Bool
    /// 是否收成紧凑版（竖排时竖向空间不够就只留两个按钮）
    var compact: Bool = false

    /// 竖屏面板高度的估算值：只用于算居中位置，实际高度由内容撑开
    /// （写死高度会把内容压扁，正是「数据没显示完整」的原因之一）
    static let portraitHeight: CGFloat = 191
    /// 横屏竖排的理想宽度
    static let landscapeWidth: CGFloat = 84

    static func compute(size: CGSize, safeInsets: EdgeInsets,
                        videoAspect: CGFloat, keyboardHeight: CGFloat = 0,
                        zoomScale: CGFloat = 1) -> StatusBarLayout {
        /// 与画面边缘、屏幕边缘之间的呼吸空间
        let gap: CGFloat = 10
        // 画面被放大后铺满全屏、没有黑边可用，只能浮在画面上
        let zoomed = zoomScale > 1.05

        // 尺寸还没算出来时给个保守默认值，避免首帧位置跳变
        guard size.width > 1, size.height > 1, videoAspect > 0.1 else {
            return StatusBarLayout(
                vertical: false,
                size: CGSize(width: 0, height: portraitHeight),
                center: CGPoint(x: size.width / 2,
                                y: max(portraitHeight / 2, size.height - portraitHeight / 2 - gap)),
                overlay: true)
        }

        if size.width > size.height {
            // ── 横屏：画面高度撑满，左右留黑边 → 竖排贴右侧黑边 ──
            let videoWidth = min(size.width, size.height * videoAspect)
            let band = max(0, (size.width - videoWidth) / 2)
            // 与画面边缘、屏幕边缘之间的留白
            let edge: CGFloat = 6
            // 可用宽度 = 黑边厚度 − 两侧留白。
            // 这里**不再**扣 safeInsets.trailing：横屏的安全区是屏幕圆角占位，
            // 而状态条纵向居中、离上下圆角远得很，白扣 59pt 后黑边只剩 25pt，
            // 再被最小宽度一撑就压到画面上去了——正是「跟远程屏幕重叠」的成因。
            let usable = max(0, band - edge * 2)
            let barWidth = min(landscapeWidth, usable)
            // 贴屏幕右侧：右端留 edge，整体落在黑边里（横屏黑边约 135pt，够放）
            // 这样「向右靠」是贴着屏幕边，而不是只在黑边中间
            let cx = size.width - edge - barWidth / 2

            // 纵向：在安全区与键盘之间居中；空间不足（或键盘弹起）就贴顶，
            // 否则竖排的十来项会被键盘从下面压掉一截，按钮都点不到
            let usableTop = safeInsets.top
            let usableBottom = size.height - safeInsets.bottom - max(0, keyboardHeight)
            let usableH = max(0, usableBottom - usableTop)
            // 竖排完整内容（4 项指标 + 两个圆钮）约 280pt；
            // 只剩两个圆钮的紧凑版约 126pt。
            // 横屏一弹键盘，可用高度会被压到 200pt 出头，完整版塞不下——
            // 那时只留「返回主页 / 键盘」，指标全撤，否则半截会被键盘盖住。
            let fullH: CGFloat = 280
            let compactH: CGFloat = 126
            let compact = usableH < fullH + gap * 2
            let contentH = compact ? compactH : fullH
            let cy = usableH >= contentH + gap * 2
                ? usableTop + usableH / 2
                : usableTop + gap + contentH / 2

            return StatusBarLayout(
                vertical: true,
                size: CGSize(width: barWidth, height: 0),
                center: CGPoint(x: cx, y: cy),
                // 黑边实在太窄（视频比例贴近屏幕比例）才退化成浮层
                overlay: zoomed || usable < 52,
                compact: compact)
        }

        // ── 竖屏：画面宽度撑满，上下留黑边 → 面板挂到屏幕上方黑边 ──
        let videoHeight = min(size.height, size.width / videoAspect)
        let band = max(0, (size.height - videoHeight) / 2)
        // 与屏幕左右边缘的留白
        let edge: CGFloat = 16
        let barWidth = min(size.width - edge * 2, 460)
        // 高度不写死：交给内容撑开，否则指标会被压扁（数据就显示不全了）
        let h = portraitHeight

        // 固定贴屏幕上方。上下黑边等厚，而上黑边在灵动岛之下、离画面还有
        // 一截，面板放这里整块落在黑边内；下方那片留白不再被面板占据，
        // 看着干净。贴顶后键盘弹起也不必再挪位置。
        let cy = safeInsets.top + gap + h / 2

        // 黑边装不下（画面被放大到铺满，或视频本就是竖屏比例）→ 退回浮层
        let room = band - safeInsets.top
        return StatusBarLayout(
            vertical: false,
            size: CGSize(width: barWidth, height: 0),
            center: CGPoint(x: size.width / 2, y: cy),
            overlay: zoomed || room < h + gap)
    }
}

/// 远程会话状态条：返回主页 + 键盘 + 实时指标。
///
/// 原先这些控件散在画面四角（关掉在右上、键盘在左下），既是浮层遮挡画面，
/// 也没告诉用户连接到底好不好。收进一条状态条并摆进黑边后，画面全净，
/// 延迟和帧率还能一眼看到——卡顿时能立刻分清是网络问题还是远端机器忙。
struct SessionStatusBar: View {
    @ObservedObject var session: WebRTCSession
    /// 界面语言：状态条上的按钮与指标标签都要跟着切
    @EnvironmentObject private var l10n: L10nStore
    let keyboard: RemoteKeyboardController
    let vertical: Bool
    /// 空间局促时用更透的底（仍保证白字可读），避免把底下的画面挡死
    let overlay: Bool
    /// 收成紧凑版：竖排时竖向空间不够（横屏弹键盘）就只留两个按钮
    let compact: Bool
    /// 状态条尺寸：竖排给宽度、横排给高度，另一维由内容撑开
    let layoutSize: CGSize
    let onHome: () -> Void

    /// 手绘风主色：墨棕。描边、文字、投影都取它，搁在暖色纸底上最协调。
    private var ink: Color { Color(red: 0.40, green: 0.25, blue: 0.13) }
    /// 卡面：与首页列表卡片同一种纸白；浮层模式（盖在画面上）略透一点，
    /// 让底下的画面能隐约透出来，不至于完全挡死。
    private var cardPaper: Color { Brand.cardPaper.opacity(overlay ? 0.93 : 1) }
    /// 深琥珀：指标小标题与图标用，比墨棕活泼、比亮琥珀压得住
    private var accentGold: Color { Color(red: 0.79, green: 0.44, blue: 0.09) }
    /// 卡片圆角：留小一点，手绘框的圆角本就不大
    private var cardCorner: CGFloat { vertical ? 16 : 18 }
    /// 圆形按钮直径
    private var circleSide: CGFloat { vertical ? 42 : 44 }
    /// 按钮渐变：主页用琥珀（主操作）、键盘用浅沙色（次要），
    /// 两者深浅拉开，一眼分得出哪个是哪个
    private var goldButton: [Color] {
        [Color(red: 1.00, green: 0.80, blue: 0.45),
         Color(red: 0.95, green: 0.58, blue: 0.20)]
    }
    private var sandButton: [Color] {
        [Color(red: 1.00, green: 0.91, blue: 0.72),
         Color(red: 0.94, green: 0.76, blue: 0.44)]
    }

    private var stats: SessionStats { session.stats }

    var body: some View {
        Group {
            if vertical { landscapeContent } else { portraitContent }
        }
        // 横排只有 84pt 宽，横向留白必须小，否则内容被挤得没地方
        .padding(.horizontal, vertical ? 7 : 14)
        // 竖排留白 16pt：面板总高要落进上方黑边（扣掉灵动岛后可用约 267pt）
        .padding(.vertical, vertical ? 12 : 16)
        // 0 表示该维不约束、由内容撑开
        .frame(width: layoutSize.width > 0 ? layoutSize.width : nil,
               height: layoutSize.height > 0 ? layoutSize.height : nil)
        // 卡面：纸白圆角卡片，与首页的列表卡片同一语言。
        // 不再用双线手绘勾边——小尺寸下描两道反而显脏，
        // 一道 1px 浅描边 + 柔和投影就够把卡片从背景里拎出来。
        .background(
            RoundedRectangle(cornerRadius: cardCorner, style: .continuous)
                .fill(cardPaper)
                .overlay(
                    RoundedRectangle(cornerRadius: cardCorner, style: .continuous)
                        .stroke(ink.opacity(0.12), lineWidth: 1)
                )
        )
        // 暖棕投影：纯黑投影压在暖色纸上会发脏
        .shadow(color: ink.opacity(overlay ? 0.25 : 0.18),
                radius: overlay ? 8 : 12, y: 4)
        .environment(\.colorScheme, .light)
    }

    // MARK: - 两种排布

    /// 竖屏：上排两个圆形按钮夹一行小标题，下面是 2×2 指标格。
    /// 上一版是「图标 + 数值」横排四行：图标语义得猜（speedometer 凭什么是帧率），
    /// 四行平铺也散。现在每项都是「小标签 + 大数值」的格子，一眼知道哪个是哪个。
    private var portraitContent: some View {
        VStack(spacing: 14) {
            HStack {
                circleButton(icon: "house.fill", label: l10n.t(.backToHome),
                             colors: goldButton) { onHome() }
                Spacer(minLength: 0)
                Text(l10n.t(.remoteSession))
                    .font(.system(size: 12, weight: .semibold, design: .rounded))
                    .tracking(2)
                    .foregroundColor(ink.opacity(0.42))
                Spacer(minLength: 0)
                circleButton(icon: "keyboard", label: l10n.t(.remoteKeyboard),
                             colors: sandButton) { keyboard.toggle() }
            }
            VStack(spacing: 0) {
                HStack(spacing: 0) {
                    metricCell(l10n.t(.metricLatency), stats.rttText)
                    gridLine(vertical: true)
                    metricCell(l10n.t(.metricFps), stats.fpsText)
                }
                gridLine(vertical: false)
                HStack(spacing: 0) {
                    metricCell(l10n.t(.metricBitrate), stats.bitrateText)
                    gridLine(vertical: true)
                    metricCell(l10n.t(.metricResolution), stats.resolutionText)
                }
            }
            // 诊断行（临时探针）：抖动缓冲 + 解码耗时。
            //
            // 「显示 100ms、体感 1 秒」的差额最可能藏在抖动缓冲里，这一行就是
            // 用来量它的——量清楚才知道该砍哪里。取不到数据时整行不显示。
            if let diag = stats.diagnosticText {
                Text(diag)
                    .font(.system(size: 11, weight: .medium, design: .rounded))
                    .foregroundColor(ink.opacity(0.5))
            }
        }
    }

    /// 横屏：贴侧边竖排。黑边只有 84pt 宽，放不下「1.4 Mbps」这类文本，
    /// 指标一律用缩写（32 / 7ms / 1.4M / 1964p），标签与数值上下叠放。
    private var landscapeContent: some View {
        VStack(spacing: 12) {
            circleButton(icon: "house.fill", label: l10n.t(.backToHome),
                         colors: goldButton) { onHome() }
            // 紧凑版只留按钮：横屏弹键盘时可用高度只剩 200pt 出头，
            // 塞不下四项指标，硬塞会有半截被键盘压住
            if !compact {
                gridLine(vertical: false)
                miniMetric(l10n.t(.metricFps), short(stats.fpsText))
                miniMetric(l10n.t(.metricLatency), short(stats.rttText))
                miniMetric(l10n.t(.metricBitrate), short(stats.bitrateText))
                miniMetric(l10n.t(.metricResolution), compactResolution)
                gridLine(vertical: false)
            }
            circleButton(icon: "keyboard", label: l10n.t(.remoteKeyboard),
                         colors: sandButton) { keyboard.toggle() }
        }
    }

    /// 缩写：去掉空格与冗余单位，供横屏窄条使用
    private func short(_ text: String) -> String {
        text.replacingOccurrences(of: " ", with: "")
            .replacingOccurrences(of: "Mbps", with: "M")
            .replacingOccurrences(of: "kbps", with: "k")
            .replacingOccurrences(of: "fps", with: "")
    }

    /// 竖排下的分辨率：只取高度，够表达清晰度又不撑破宽度
    private var compactResolution: String {
        // 注意取 stats.resolution：SessionStats 自身的属性，别写成裸名字
        guard let resolution = stats.resolution else { return "—" }
        let parts = resolution.split(separator: "×")
        guard parts.count == 2 else { return resolution }
        return "\(parts[1])p"
    }

    // MARK: - 组件

    /// 圆形图标钮：手绘圆 + 暖色渐变 + 墨棕描边。
    /// 上一版是带文字的胶囊按钮，两个并排占满一行、把指标挤到下面；改成圆钮后
    /// 按钮变轻，中间还能放一行会话标题，画面与数据才是主角。
    private func circleButton(icon: String, label: String,
                              colors: [Color],
                              action: @escaping () -> Void) -> some View {
        Button {
            tap()
            action()
        } label: {
            Image(systemName: icon)
                .font(.system(size: vertical ? 17 : 18, weight: .semibold))
                .foregroundColor(ink.opacity(0.9))
                .frame(width: circleSide, height: circleSide)
                .background(
                    ZStack {
                        HandCircle(wobble: 1.0)
                            .fill(LinearGradient(colors: colors,
                                                 startPoint: .top, endPoint: .bottom))
                        // 左上受光：纸上的光总从一边来，圆钮才显得鼓
                        HandCircle(wobble: 1.0)
                            .fill(LinearGradient(colors: [.white.opacity(0.32), .clear],
                                                 startPoint: .topLeading,
                                                 endPoint: .center))
                        HandCircle(wobble: -0.8)
                            .stroke(ink.opacity(0.62),
                                    style: StrokeStyle(lineWidth: 1.4,
                                                       lineCap: .round, lineJoin: .round))
                    }
                    .shadow(color: ink.opacity(0.20), radius: 4, y: 2)
                )
        }
        .buttonStyle(PressFeedback())
        .accessibilityLabel(label)
    }

    /// 手绘圆：四段曲线拼成，半径带轻微起伏，不是标准正圆。
    /// 圆规画的正圆太规整，这条略带起伏的闭合曲线才有笔尖绕圈的手感。
    private struct HandCircle: Shape {
        /// 起伏幅度，负号用于描边层以错开填充
        var wobble: CGFloat

        func path(in rect: CGRect) -> Path {
            let c = CGPoint(x: rect.midX, y: rect.midY)
            let r = min(rect.width, rect.height) / 2 - 0.6
            // 三次贝塞尔逼近圆弧的经典控制点比例
            let d = r * 0.5523
            let o = wobble
            var p = Path()
            p.move(to: CGPoint(x: c.x, y: c.y - r - o * 0.4))
            p.addCurve(to: CGPoint(x: c.x + r + o * 0.5, y: c.y),
                       control1: CGPoint(x: c.x + d, y: c.y - r - o * 0.4),
                       control2: CGPoint(x: c.x + r + o * 0.5, y: c.y - d))
            p.addCurve(to: CGPoint(x: c.x, y: c.y + r + o * 0.3),
                       control1: CGPoint(x: c.x + r + o * 0.5, y: c.y + d),
                       control2: CGPoint(x: c.x + d, y: c.y + r + o * 0.3))
            p.addCurve(to: CGPoint(x: c.x - r - o * 0.4, y: c.y),
                       control1: CGPoint(x: c.x - d, y: c.y + r + o * 0.3),
                       control2: CGPoint(x: c.x - r - o * 0.4, y: c.y + d))
            p.addCurve(to: CGPoint(x: c.x, y: c.y - r - o * 0.4),
                       control1: CGPoint(x: c.x - r - o * 0.4, y: c.y - d),
                       control2: CGPoint(x: c.x - d, y: c.y - r - o * 0.4))
            p.closeSubpath()
            return p
        }
    }

    /// 按下时缩小并轻微变暗，配合轻触反馈——与系统按钮手感一致
    private func tap() {
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
    }

    private struct PressFeedback: ButtonStyle {
        func makeBody(configuration: Configuration) -> some View {
            configuration.label
                .scaleEffect(configuration.isPressed ? 0.94 : 1)
                .brightness(configuration.isPressed ? -0.05 : 0)
                .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
        }
    }

    /// 指标格：上小标签、下大数值。
    /// 上一版用图标代替标签，可图标得猜语义（speedometer 凭什么是帧率）；
    /// 直接写「延迟 / 帧率 / 码率 / 分辨率」，谁也不用猜。
    private func metricCell(_ title: String, _ value: String) -> some View {
        VStack(spacing: 3) {
            // 标签用字距拉开，像本子上的眉批，与数值分明主次
            Text(title)
                .font(.system(size: 12, weight: .semibold, design: .rounded))
                .tracking(1.2)
                .foregroundColor(accentGold.opacity(0.85))
            Text(value)
                .font(.system(size: 19, weight: .bold, design: .rounded))
                .foregroundColor(ink)
                .lineLimit(1)
                .minimumScaleFactor(0.6)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 7)
    }

    /// 竖排窄条（横屏黑边仅 84pt 宽）里的指标：标签与数值都居中叠放
    private func miniMetric(_ title: String, _ value: String) -> some View {
        VStack(spacing: 1) {
            Text(title)
                .font(.system(size: 10, weight: .semibold, design: .rounded))
                .foregroundColor(accentGold.opacity(0.85))
            Text(value)
                .font(.system(size: 14, weight: .bold, design: .rounded))
                .foregroundColor(ink)
                .lineLimit(1)
                .minimumScaleFactor(0.6)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 3)
    }

    /// 指标之间的格线：手绘虚线，比实线轻，不会把格子切得死板
    @ViewBuilder
    private func gridLine(vertical: Bool) -> some View {
        if vertical {
            HandDash()
                .stroke(ink.opacity(0.26),
                        style: StrokeStyle(lineWidth: 1.2, lineCap: .round, dash: [4, 3]))
                .frame(width: 1.2)
                .frame(maxHeight: .infinity)
        } else {
            HandDivider(color: ink.opacity(0.26))
        }
    }

    /// 竖虚线：与 HandDivider 同风格，只是立了起来
    private struct HandDash: Shape {
        func path(in rect: CGRect) -> Path {
            var p = Path()
            p.move(to: CGPoint(x: rect.midX, y: 1))
            // 下端偏右 0.4pt：跟横线一个道理，不追求绝对笔直
            p.addLine(to: CGPoint(x: rect.midX + 0.4, y: max(1, rect.height - 1)))
            return p
        }
    }
}

/// 手绘边框：直边换成微幅抖动的二次曲线，四角收成小圆弧。
/// 直尺画出来的矩形太「数码」，抖上一两像素才有马克笔随手描边的味道。
/// 抖动量是固定常量而非随机数——随机会每帧重画，变成闪烁。
struct HandDrawnBorder: Shape {
    var corner: CGFloat = 14
    /// 抖动幅度（pt），越大越像手抖
    var wobble: CGFloat = 1.6

    func path(in rect: CGRect) -> Path {
        var p = Path()
        let r = max(0, min(corner, min(rect.width, rect.height) / 2))
        let l = rect.minX, rt = rect.maxX, t = rect.minY, b = rect.maxY
        let w = wobble
        let mx = (l + rt) / 2, my = (t + b) / 2

        p.move(to: CGPoint(x: l + r, y: t))
        // 上边：中点略上凸
        p.addQuadCurve(to: CGPoint(x: rt - r, y: t + w * 0.35),
                       control: CGPoint(x: mx, y: t - w))
        p.addQuadCurve(to: CGPoint(x: rt - w * 0.3, y: t + r),
                       control: CGPoint(x: rt + w * 0.5, y: t))
        // 右边：中点略外凸
        p.addQuadCurve(to: CGPoint(x: rt, y: b - r),
                       control: CGPoint(x: rt + w * 0.8, y: my))
        p.addQuadCurve(to: CGPoint(x: rt - r, y: b - w * 0.3),
                       control: CGPoint(x: rt, y: b + w * 0.5))
        // 下边
        p.addQuadCurve(to: CGPoint(x: l + r, y: b + w * 0.3),
                       control: CGPoint(x: mx, y: b + w))
        p.addQuadCurve(to: CGPoint(x: l + w * 0.3, y: b - r),
                       control: CGPoint(x: l - w * 0.5, y: b))
        // 左边
        p.addQuadCurve(to: CGPoint(x: l, y: t + r),
                       control: CGPoint(x: l - w * 0.8, y: my))
        p.addQuadCurve(to: CGPoint(x: l + r, y: t),
                       control: CGPoint(x: l, y: t - w * 0.5))
        p.closeSubpath()
        return p
    }
}

/// 手绘分割线：短划线、端点圆头、右端略低，像手画的虚线。
struct HandDivider: View {
    var color: Color

    var body: some View {
        HandLine()
            .stroke(color, style: StrokeStyle(lineWidth: 1.2, lineCap: .round, dash: [4, 3]))
            .frame(height: 1.4)
    }

    private struct HandLine: Shape {
        func path(in rect: CGRect) -> Path {
            var p = Path()
            p.move(to: CGPoint(x: 1, y: rect.midY))
            // 右端低 0.4pt：完全水平的线看着像印刷的，不像手画的
            p.addLine(to: CGPoint(x: max(1, rect.width - 1), y: rect.midY + 0.4))
            return p
        }
    }
}

/// 远程屏幕视图：渲染 WebRTC 视频流 + 触摸手势转输入事件
struct RemoteScreenView: UIViewRepresentable {
    @ObservedObject var session: WebRTCSession
    let keyboard: RemoteKeyboardController
    /// 视频几何信息回传（SwiftUI 层靠它把状态条摆进黑边）
    let metrics: RemoteScreenMetrics

    func makeUIView(context: Context) -> RemoteScreenUIView {
        let view = RemoteScreenUIView(session: session)
        keyboard.capture = view.keyboardCaptureView
        view.metrics = metrics
        return view
    }

    func updateUIView(_ uiView: RemoteScreenUIView, context: Context) {
        // 顺带重设一次：metrics 是 @StateObject，身份稳定，这里只是保险
        uiView.metrics = metrics
        uiView.updateVideoTrack()
    }

    static func dismantleUIView(_ uiView: RemoteScreenUIView, coordinator: ()) {
        uiView.detachTrack()
    }
}

/// 视频几何信息：SwiftUI 层需要知道画面实际占多大，才能把状态条
/// 放进 scaleAspectFit 留出的黑边里（而不是压在画面上）。
///
/// UIKit 侧的 RemoteScreenUIView 是唯一知道真实帧尺寸的地方
/// （由 RTCMTLVideoView 回调实测），所以由它写、SwiftUI 读。
final class RemoteScreenMetrics: ObservableObject {
    /// 远端视频宽高比。初值取 host 常见输出比例，收到真实回调后即被覆盖。
    @Published var videoAspect: CGFloat = 3024.0 / 1964.0
    /// 键盘高度（由键盘回调同步过来，供摆位计算使用）
    @Published var keyboardHeight: CGFloat = 0
    /// 安全区插入量（灵动岛 / Home indicator）。
    /// 必须由 UIKit 侧 layoutSubviews 回传：实测 GeometryReader 在
    /// .ignoresSafeArea() 下 safeAreaInsets 恒为 0，SwiftUI 层根本拿不到
    /// 灵动岛与 Home indicator 的占用，状态条就会压到它们上面。
    @Published var safeInsets: UIEdgeInsets = .zero
    /// 画面缩放倍数。放大后画面铺满全屏、不再有黑边可用，
    /// 状态条必须转为浮层——所以摆位计算需要知道它。
    @Published var zoomScale: CGFloat = 1
    /// 变换（缩放 + 平移 + 键盘让位）之后画面在视图里的实际矩形，
    /// 由 UIKit 侧随 applyVideoTransform 回传。RemoteBackdrop 按它挖洞铺纸：
    /// 放大后画面未必铺满全屏（竖屏放大看横屏画面，四周仍有未覆盖区），
    /// 那些区域必须继续铺纸色，不能露出 MTKView 的死黑。
    @Published var transformedVideoRect: CGRect?
}

/// 自定义 UIView：包含 RTCMTLVideoView + 触摸手势识别 + 光标准星 + 双指缩放
class RemoteScreenUIView: UIView, RTCVideoViewDelegate {
    private let videoView: RTCMTLVideoView
    private let session: WebRTCSession
    /// 远端轨道订阅：本视图出现后，轨道无论何时到达都能立刻挂上
    private var trackSubscription: AnyCancellable?
    /// 已经挂到 self.videoView 上的那条轨道
    private var attachedTrack: RTCVideoTrack?

    /// 视频几何信息回传通道（SwiftUI 层用）。这里不持有强引用以外的语义：
    /// 由 RemoteScreenView.makeUIView 注入，随容器视图同生命周期。
    var metrics: RemoteScreenMetrics?

    // MARK: - 光标准星（显示当前触摸/鼠标位置）

    private let cursorView = UIView()
    private let cursorRing = CAShapeLayer()
    private let cursorArrow = CAShapeLayer()
    private var cursorHideTimer: Timer?

    // MARK: - 拖拽状态（长按后拖动 = 按住左键拖拽）

    private var isDragging = false
    private var dragStartPoint: CGPoint = .zero

    // MARK: - 远程键盘

    /// 输入捕获控件：由它唤起系统键盘，画面本身不再被模态面板遮挡
    private let keyboardCapture = KeyboardInputCaptureView()
    /// 键盘当前遮住本视图的高度（0 表示未弹出）
    private var keyboardHeight: CGFloat = 0
    /// 键盘弹出时为露出被遮区域而对画面施加的向上位移（负值）
    private var keyboardShiftY: CGFloat = 0

    /// 供 SwiftUI 侧桥接使用
    var keyboardCaptureView: KeyboardInputCaptureView { keyboardCapture }

    // MARK: - 双指缩放

    /// 当前缩放倍数（1.0 = 原始适配）
    private var zoomScale: CGFloat = 1.0
    /// 缩放锚点（归一化坐标，缩放围绕该点）
    private var zoomAnchor: CGPoint = CGPoint(x: 0.5, y: 0.5)
    /// 缩放后画面在视图内的偏移（由 transform 处理）
    /// 滚动位移累积器（视图像素）：手指位移带 2x 增益后累积，
    /// 每攒满 20pt 才发一条事件。逐帧直发的小位移会被 Windows 侧
    /// (px/40).round() 归零丢掉，累积器保证「轻划一下」也能滚得动。
    private var scrollAccumX: Float = 0
    private var scrollAccumY: Float = 0
    /// 发事件的累积阈值（pt）：约半格滚轮，兼顾灵敏与不失控
    private let scrollEmitStep: Float = 20
    private var panOffset: CGPoint = .zero

    init(session: WebRTCSession) {
        self.session = session
        self.videoView = RTCMTLVideoView(frame: .zero)
        super.init(frame: .zero)

        // 渲染器不再注入会话层：会话的生命周期比视图长，切线路时它还会指向上一个
        // 已经不可见的视图，导致轨道被挂到看不见的渲染器上。改由本视图订阅轨道，
        // 自己挂自己的（见 subscribeRemoteTrack）。
        videoView.videoContentMode = .scaleAspectFit
        // 视频层与自身都透明：scaleAspectFit 留出的那一圈 letterbox 交给
        // 下层的暖色纸底（RemoteBackdrop）去画。不设透明，这圈就是死黑——
        // 也就是「远程页面没有背景」的样子。
        videoView.backgroundColor = .clear
        self.backgroundColor = .clear
        // 订阅真实视频尺寸变化，用于自适应 letterbox 计算
        videoView.delegate = self
        videoView.translatesAutoresizingMaskIntoConstraints = false
        addSubview(videoView)
        NSLayoutConstraint.activate([
            videoView.topAnchor.constraint(equalTo: topAnchor),
            videoView.bottomAnchor.constraint(equalTo: bottomAnchor),
            videoView.leadingAnchor.constraint(equalTo: leadingAnchor),
            videoView.trailingAnchor.constraint(equalTo: trailingAnchor),
        ])

        setupCursor()
        setupGestures()
        setupKeyboard()
        subscribeRemoteTrack()
    }

    required init?(coder: NSCoder) { fatalError() }

    /// 订阅远端轨道并把轨道挂到本视图的渲染器上（幂等）。
    ///
    /// 为什么由视图自己订阅、而不是让会话层记住「当前渲染器」：会话比视图活得久。
    /// 切换线路时远端轨道往往先到，若按「会话层的渲染器」去挂，它会挂到上一个
    /// 已经不可见的视图上，等新视图建好就再也挂不上了——表现是连接正常、解码
    /// 统计也在跳，屏幕却一片空白。视图各自订阅则天然是「谁在看谁挂」，
    /// 与轨道和渲染器的到达先后无关。
    private func subscribeRemoteTrack() {
        guard trackSubscription == nil else { return }
        trackSubscription = session.$remoteVideoTrack
            .receive(on: DispatchQueue.main)
            .sink { [weak self] track in
                guard let self, let track else { return }
                guard self.attachedTrack !== track else { return }
                // 换轨时先把本渲染器从旧轨摘掉，否则旧轨会继续持有它
                if let old = self.attachedTrack { old.remove(self.videoView) }
                track.add(self.videoView)
                track.isEnabled = true
                self.attachedTrack = track
                self.session.noteRendererAttached()
                print("[view] video track attached to RTCMTLVideoView")
            }
    }

    /// 视图销毁时把渲染器从轨道上摘掉，避免轨道继续持有已消失的渲染器。
    func detachTrack() {
        trackSubscription?.cancel()
        trackSubscription = nil
        if let track = attachedTrack {
            track.remove(videoView)
            attachedTrack = nil
        }
    }

    func updateVideoTrack() {
        // 兜一次订阅：SwiftUI 不保证 updateUIView 的调用时机，
        // 把挂载做成「订阅驱动」才能与轨道到达的先后无关。
        subscribeRemoteTrack()
    }

    // MARK: - 光标准星

    private func setupCursor() {
        let size: CGFloat = 34
        cursorView.frame = CGRect(x: 0, y: 0, width: size, height: size)
        cursorView.isUserInteractionEnabled = false
        cursorView.isHidden = true
        cursorView.alpha = 0

        // 外圈：细白环 + 柔和阴影，贴近系统指针观感
        cursorRing.path = UIBezierPath(ovalIn: CGRect(x: 2, y: 2, width: size - 4, height: size - 4)).cgPath
        cursorRing.fillColor = UIColor.clear.cgColor
        cursorRing.strokeColor = UIColor.white.cgColor
        cursorRing.lineWidth = 1.5
        cursorRing.shadowColor = UIColor.black.cgColor
        cursorRing.shadowOpacity = 0.55
        cursorRing.shadowRadius = 2
        cursorRing.shadowOffset = .zero
        cursorView.layer.addSublayer(cursorRing)

        // 中心箭头：经典鼠标指针形状（白色填充 + 黑色描边）
        let arrow = UIBezierPath()
        arrow.move(to: CGPoint(x: size / 2 - 4, y: size / 2 - 7))
        arrow.addLine(to: CGPoint(x: size / 2 - 4, y: size / 2 + 7))
        arrow.addLine(to: CGPoint(x: size / 2 - 0.5, y: size / 2 + 3.5))
        arrow.addLine(to: CGPoint(x: size / 2 + 2.5, y: size / 2 + 9))
        arrow.addLine(to: CGPoint(x: size / 2 + 5, y: size / 2 + 7.5))
        arrow.addLine(to: CGPoint(x: size / 2 + 2, y: size / 2 + 2))
        arrow.addLine(to: CGPoint(x: size / 2 + 6, y: size / 2 + 2))
        arrow.close()
        cursorArrow.path = arrow.cgPath
        cursorArrow.fillColor = UIColor.white.cgColor
        cursorArrow.strokeColor = UIColor.black.cgColor
        cursorArrow.lineWidth = 1
        cursorArrow.lineJoin = .round
        cursorArrow.shadowColor = UIColor.black.cgColor
        cursorArrow.shadowOpacity = 0.5
        cursorArrow.shadowRadius = 1.5
        cursorArrow.shadowOffset = .zero
        cursorView.layer.addSublayer(cursorArrow)

        addSubview(cursorView)
    }

    /// 在指定位置显示光标，并自动淡出
    private func showCursor(at point: CGPoint) {
        cursorView.center = point
        cursorView.isHidden = false

        cursorHideTimer?.invalidate()
        UIView.animate(withDuration: 0.12) {
            self.cursorView.alpha = 1.0
        }
        cursorHideTimer = Timer.scheduledTimer(withTimeInterval: 1.5, repeats: false) { [weak self] _ in
            UIView.animate(withDuration: 0.35) {
                self?.cursorView.alpha = 0
            } completion: { _ in
                self?.cursorView.isHidden = true
            }
        }
    }

    // MARK: - 键盘接入

    /// 把输入捕获控件挂进视图树。
    ///
    /// 尺寸 1x1、透明度 0.01：既能成为第一响应者（完全隐藏的视图不行），
    /// 又因 alpha ≤ 0.01 不参与命中测试，不会吞掉画面上的任何手势。
    private func setupKeyboard() {
        keyboardCapture.frame = CGRect(x: 0, y: 0, width: 1, height: 1)
        keyboardCapture.onText = { [weak self] text in
            self?.session.sendInput(.text(content: text))
        }
        keyboardCapture.onKey = { [weak self] code, mods in
            guard let self else { return }
            // 按一次发一对按下/抬起，远端才判定为一次完整按键
            self.session.sendInput(.key(keycode: code, down: true, modifiers: mods))
            self.session.sendInput(.key(keycode: code, down: false, modifiers: mods))
        }
        keyboardCapture.onKeyboardHeightChange = { [weak self] height in
            self?.updateKeyboardHeight(height)
        }
        addSubview(keyboardCapture)
    }

    /// 键盘高度变化：把画面整体上移，让被键盘盖住的区域重新露出来。
    ///
    /// 选择「位移」而非「缩小」：缩小整屏会让远端文字一并变小，而清晰度正是
    /// 这台设备上最在意的东西；位移只换视野，分辨率与字号都不变。
    private func updateKeyboardHeight(_ height: CGFloat) {
        guard abs(height - keyboardHeight) > 1 else { return }
        keyboardHeight = height
        // 同步给 SwiftUI 层：状态条要知道键盘是否弹出，才能从底部黑边
        // 挪到顶部黑边（否则打字时被键盘压住，返回主页就点不到了）。
        let metrics = self.metrics
        DispatchQueue.main.async { metrics?.keyboardHeight = height }
        // 让位量由 applyVideoTransform 内部重算，此处不重复调用
        applyVideoTransform()
    }

    /// 计算让位位移：让视频画面的下边缘落在键盘上沿之上。
    ///
    /// 未放大时视频是居中的一条带、本来就大多在键盘上方，此时位移为 0，
    /// 不做无谓跳动；放大后画面铺满全屏，才会真正上移。
    private func applyKeyboardShift() {
        guard keyboardHeight > 0, zoomScale > 0 else {
            keyboardShiftY = 0
            return
        }
        let rect = videoDisplayRect()
        let center = CGPoint(x: bounds.midX, y: bounds.midY)
        let bottomWithoutShift = center.y + (rect.maxY - center.y) * zoomScale + panOffset.y
        let visibleBottom = bounds.height - keyboardHeight
        // 只允许上移（负值），且不多移
        keyboardShiftY = min(0, visibleBottom - bottomWithoutShift)
    }

    /// 统一施加画面变换（缩放 + 平移 + 键盘让位）。
    /// 加入键盘让位后必须集中管理：原先散落在多处直接写 transform，漏改一处就会错位。
    private func applyVideoTransform() {
        // 先重算让位量：缩放与平移都会改变画面下缘，让位量必须跟着变，
        // 否则放大之后画面仍会被键盘盖住一截。
        applyKeyboardShift()
        videoView.transform = CGAffineTransform(
            translationX: panOffset.x,
            y: panOffset.y + keyboardShiftY
        ).scaledBy(x: zoomScale, y: zoomScale)
        // 放大后画面会铺满全屏、黑边消失，状态条得知道这件事才能转为浮层。
        // 只上报整数档位以外的变化，避免每帧都触发 SwiftUI 重算。
        let scale = zoomScale
        // 变换后的画面矩形：transform 是「先平移后缩放」（translation 在外），
        // 视图中心点 c 映射到 center + (c-center)*scale + offset，
        // 故矩形宽高各乘 scale、中心相对视图中心偏移 offset。
        // 与 bounds 求交：超出屏幕的部分不用挖洞（纸只画屏内）。
        let fit = videoDisplayRect()
        let visible = CGRect(
            x: bounds.midX + (fit.midX - bounds.midX) * scale + panOffset.x - fit.width * scale / 2,
            y: bounds.midY + (fit.midY - bounds.midY) * scale + panOffset.y + keyboardShiftY - fit.height * scale / 2,
            width: fit.width * scale,
            height: fit.height * scale
        ).intersection(bounds)
        let metrics = self.metrics
        DispatchQueue.main.async {
            guard let metrics else { return }
            if abs(metrics.zoomScale - scale) > 0.01 {
                metrics.zoomScale = scale
            }
            // 无效矩形（首帧前 bounds 为零）不上报：SwiftUI 侧会退回
            // fit 几何，避免把整屏洞成零、纸盖住画面
            if visible.width > 1, visible.height > 1,
               metrics.transformedVideoRect != visible {
                metrics.transformedVideoRect = visible
            }
        }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        // 尺寸变化（旋转、分屏）会让 letterbox 与让位量失效，须重算
        applyVideoTransform()
        // 回传真实安全区：本视图铺满全屏，只有 UIKit 侧拿得到灵动岛与
        // Home indicator 的真实占用（SwiftUI 那侧在 ignoresSafeArea 下是 0）
        let insets = safeAreaInsets
        let metrics = self.metrics
        DispatchQueue.main.async {
            guard let metrics else { return }
            if metrics.safeInsets != insets { metrics.safeInsets = insets }
        }
    }

    // MARK: - 触摸手势 → 输入事件

    private func setupGestures() {
        // 单指拖动 = 鼠标移动（严格限 1 指：多指时不与双指滚动/三指平移抢识别）
        let pan = UIPanGestureRecognizer(target: self, action: #selector(handlePan(_:)))
        pan.maximumNumberOfTouches = 1
        addGestureRecognizer(pan)

        // 单指点击 = 鼠标左键
        let tap = UITapGestureRecognizer(target: self, action: #selector(handleTap(_:)))
        addGestureRecognizer(tap)

        // 双指点击 = 鼠标右键
        let twoFingerTap = UITapGestureRecognizer(target: self, action: #selector(handleTwoFingerTap(_:)))
        twoFingerTap.numberOfTouchesRequired = 2
        addGestureRecognizer(twoFingerTap)

        // 双指拖动 = 滚轮（放大与否都是滚动）。上一版一个函数两用：
        // 放大后双指被平移独占，视野还有移动空间时每帧都被平移吃掉，
        // 用户实测「放大态滚不动」。按用户拍板分通道：双指恒滚动。
        let twoFingerPan = UIPanGestureRecognizer(target: self, action: #selector(handleTwoFingerScroll(_:)))
        twoFingerPan.minimumNumberOfTouches = 2
        twoFingerPan.maximumNumberOfTouches = 2
        addGestureRecognizer(twoFingerPan)

        // 三指拖动 = 放大后平移视野（低频操作，让出双指这条最直觉的通道；
        // 复用原「平移+到边界残余转滚动」函数，撞边界继续划还能兜底滚动）
        let threeFingerPan = UIPanGestureRecognizer(target: self, action: #selector(handleTwoFingerPan(_:)))
        threeFingerPan.minimumNumberOfTouches = 3
        threeFingerPan.maximumNumberOfTouches = 3
        addGestureRecognizer(threeFingerPan)

        // 双指捏合 = 缩放画面
        let pinch = UIPinchGestureRecognizer(target: self, action: #selector(handlePinch(_:)))
        addGestureRecognizer(pinch)

        // 长按后拖动 = 按住左键拖拽（选中文字、拖窗口、拖文件）
        let dragPress = UILongPressGestureRecognizer(target: self, action: #selector(handleDragPress(_:)))
        dragPress.minimumPressDuration = 0.45
        dragPress.allowableMovement = .greatestFiniteMagnitude
        addGestureRecognizer(dragPress)

        // 双击 = 重置缩放
        let doubleTap = UITapGestureRecognizer(target: self, action: #selector(handleDoubleTap(_:)))
        doubleTap.numberOfTapsRequired = 2
        addGestureRecognizer(doubleTap)
        // 双击优先于单击
        tap.require(toFail: doubleTap)
    }

    /// 长按后拖动 = 按住左键拖拽；长按不动则弹出右键菜单
    @objc private func handleDragPress(_ g: UILongPressGestureRecognizer) {
        let loc = g.location(in: self)
        let (x, y) = normalizedPoint(loc)

        switch g.state {
        case .began:
            isDragging = false
            dragStartPoint = loc
            session.sendInput(.mouseMove(x: x, y: y))
            session.sendInput(.mouseButton(x: x, y: y, button: 0, down: true))
            showCursor(at: loc)

        case .changed:
            // 移动超过阈值即视为拖拽
            if !isDragging {
                let dx = loc.x - dragStartPoint.x
                let dy = loc.y - dragStartPoint.y
                if (dx * dx + dy * dy) > 64 { isDragging = true }
            }
            session.sendInput(.mouseMove(x: x, y: y))
            showCursor(at: loc)

        case .ended:
            if isDragging {
                // 拖拽结束：释放左键
                session.sendInput(.mouseButton(x: x, y: y, button: 0, down: false))
                print("[view] drag ended at (\(x), \(y))")
            } else {
                // 长按未移动：释放左键后弹右键菜单
                session.sendInput(.mouseButton(x: x, y: y, button: 0, down: false))
                session.sendInput(.mouseButton(x: x, y: y, button: 1, down: true))
                session.sendInput(.mouseButton(x: x, y: y, button: 1, down: false))
                print("[view] long press → right click at (\(x), \(y))")
            }
            isDragging = false
            UIImpactFeedbackGenerator(style: .medium).impactOccurred()

        case .cancelled, .failed:
            if isDragging {
                session.sendInput(.mouseButton(x: x, y: y, button: 0, down: false))
            }
            isDragging = false

        default:
            break
        }
    }

    /// 双指拖动 = 滚轮（放大与否同语义）：2x 增益 + 累积器，
    /// 每攒 20pt 发一条事件，手势结束把尾数冲出——轻划也滚得动。
    @objc private func handleTwoFingerScroll(_ g: UIPanGestureRecognizer) {
        let translation = g.translation(in: self)
        g.setTranslation(.zero, in: self)

        switch g.state {
        case .began:
            scrollAccumX = 0
            scrollAccumY = 0
        case .changed:
            // 2x 增益与 dy 翻负都在 feedScroll 内部处理，这里传原始位移即可
            feedScroll(dx: translation.x, dy: translation.y)
        case .ended, .cancelled:
            // 松手把不足一档的尾数冲出去，别欠着一小段滚动
            flushScroll(force: true)
        default:
            break
        }
    }

    /// 三指拖动（现由 threeFingerPan 触发，函数名沿用）：放大时平移画面，
    /// 推到边界后残余位移自动转为滚轮——平移与滚动一个手势兜底。
    @objc private func handleTwoFingerPan(_ g: UIPanGestureRecognizer) {
        let translation = g.translation(in: self)
        g.setTranslation(.zero, in: self)

        switch g.state {
        case .began:
            scrollAccumX = 0
            scrollAccumY = 0

        case .changed:
            if zoomScale > 1.01 {
                // 放大状态：先平移画面。clampOffset 会在边界处截断，
                // 「想推的量」与「实际推动的量」之差就是推不动的残余。
                let before = panOffset
                panOffset.x += translation.x
                panOffset.y += translation.y
                clampOffset()
                applyVideoTransform()
                let residualX = translation.x - (panOffset.x - before.x)
                let residualY = translation.y - (panOffset.y - before.y)
                // 残余超过 1pt 说明该方向画面已到头，把残余转成滚轮。
                // 于是放大后长划一段：先移视野，到头了页面接着滚，
                // 两种需求共用一个手势，不必再抢双指。
                if abs(residualX) > 1 || abs(residualY) > 1 {
                    feedScroll(dx: residualX, dy: residualY)
                }
            } else {
                // 未放大：纯滚轮
                feedScroll(dx: translation.x, dy: translation.y)
            }

        case .ended, .cancelled, .failed:
            // 手势收尾把不足一档的尾数冲出去（至少 1px），
            // 否则最后一段位移会被永远留在累积器里
            flushScroll(force: true)

        default:
            break
        }
    }

    /// 手指位移 → 滚轮事件：2x 增益 + 20pt 累积整档发送。
    /// 逐帧直发的小位移会被 Windows 侧 (px/40).round() 归零丢弃，
    /// 这正是「不灵敏、划了没反应」的根因；攒够一档再发，轻划一下
    /// 也保证滚得动，且增益固定不会越划越快。
    private func feedScroll(dx: CGFloat, dy: CGFloat) {
        // 手指上滑 = 页面向下滚动（内容上移），故 dy 取负
        scrollAccumX += Float(dx) * 2.0
        scrollAccumY += Float(-dy) * 2.0
        flushScroll(force: false)
    }

    private func flushScroll(force: Bool) {
        var outX: Float = 0
        var outY: Float = 0
        if force {
            outX = scrollAccumX.rounded()
            outY = scrollAccumY.rounded()
            scrollAccumX = 0
            scrollAccumY = 0
        } else {
            let step = scrollEmitStep
            outX = (scrollAccumX / step).rounded(.towardZero) * step
            outY = (scrollAccumY / step).rounded(.towardZero) * step
            scrollAccumX -= outX
            scrollAccumY -= outY
        }
        if outX != 0 || outY != 0 {
            session.sendInput(.scroll(dx: outX, dy: outY))
        }
    }

    /// 视频画面的实际显示区域（scaleAspectFit 下的 letterbox 内框）
    /// 远端视频宽高比。由 RTCVideoViewDelegate 回调实测填充；
    /// 避免硬编码——host 改推流分辨率时若此处固定 16:9，letterbox 会算错导致点击偏移。
    private var videoAspect: CGFloat = 1280.0 / 832.0

    /// 视频尺寸回调：拿到解码后的真实帧尺寸
    func videoView(_ videoView: RTCVideoRenderer, didChangeVideoSize size: CGSize) {
        guard size.width > 0, size.height > 0 else { return }
        // 首帧到达即撤掉连接遮罩。必须放在比例判断之前——比例恰好与初值
        // 相同（或差值在阈值内）时后面的分支会整段跳过，遮罩就永远撤不掉，
        // 画面明明已经出来了还盖着一层「正在接收画面…」。
        let sessionRef = self.session
        DispatchQueue.main.async {
            if !sessionRef.firstFrameArrived { sessionRef.firstFrameArrived = true }
        }
        let aspect = size.width / size.height
        if abs(aspect - videoAspect) > 0.001 {
            videoAspect = aspect
            print("[view] video size changed: \(size.width)x\(size.height) aspect=\(aspect)")
            setNeedsLayout()
            // 同步给 SwiftUI 层：状态条要按真实比例算黑边位置。
            // 主线程更新——@Published 属主线程语义，回调线程不保证。
            let metrics = self.metrics
            DispatchQueue.main.async { metrics?.videoAspect = aspect }
        }
    }

    /// 计算视频画面在视图内的实际显示矩形（扣掉 scaleAspectFit 的黑边）
    private func videoDisplayRect() -> CGRect {
        let viewW = bounds.width
        let viewH = bounds.height
        guard viewW > 0, viewH > 0 else { return bounds }
        let viewAspect = viewW / viewH
        if viewAspect > videoAspect {
            // 视图比视频更宽 → 左右有黑边，视频高度撑满
            let h = viewH
            let w = h * videoAspect
            return CGRect(x: (viewW - w) / 2, y: 0, width: w, height: h)
        } else {
            // 视图比视频更高 → 上下有黑边，视频宽度撑满
            let w = viewW
            let h = w / videoAspect
            return CGRect(x: 0, y: (viewH - h) / 2, width: w, height: h)
        }
    }

    /// 将视图坐标转换为归一化坐标 (0.0-1.0)，考虑缩放、偏移与 letterbox 黑边
    private func normalizedPoint(_ location: CGPoint) -> (Float, Float) {
        // 1. 反解 transform：视图坐标 → 原始画面坐标
        let center = CGPoint(x: bounds.midX, y: bounds.midY)
        let dx = (location.x - center.x - panOffset.x) / zoomScale
        let dy = (location.y - center.y - panOffset.y) / zoomScale
        let origX = center.x + dx
        let origY = center.y + dy

        // 2. 扣掉 letterbox 黑边：映射到视频实际显示区域
        let rect = videoDisplayRect()
        let x = Float((origX - rect.minX) / rect.width)
        let y = Float((origY - rect.minY) / rect.height)
        return (min(max(x, 0), 1), min(max(y, 0), 1))
    }

    @objc private func handlePan(_ g: UIPanGestureRecognizer) {
        let loc = g.location(in: self)
        let (x, y) = normalizedPoint(loc)
        session.sendInput(.mouseMove(x: x, y: y))
        showCursor(at: loc)
    }

    @objc private func handleTap(_ g: UITapGestureRecognizer) {
        let loc = g.location(in: self)
        let (x, y) = normalizedPoint(loc)
        session.sendInput(.mouseButton(x: x, y: y, button: 0, down: true))
        session.sendInput(.mouseButton(x: x, y: y, button: 0, down: false))
        showCursor(at: loc)
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
    }

    @objc private func handleTwoFingerTap(_ g: UITapGestureRecognizer) {
        let loc = g.location(in: self)
        let (x, y) = normalizedPoint(loc)
        session.sendInput(.mouseButton(x: x, y: y, button: 1, down: true))
        session.sendInput(.mouseButton(x: x, y: y, button: 1, down: false))
        showCursor(at: loc)
    }

    @objc private func handleScroll(_ g: UIPanGestureRecognizer) {
        let velocity = g.velocity(in: self)
        let dx = Float(velocity.x) / 1000.0
        let dy = Float(-velocity.y) / 1000.0  // 反转 Y 轴
        session.sendInput(.scroll(dx: dx, dy: dy))
    }

    /// 双指捏合缩放画面
    @objc private func handlePinch(_ g: UIPinchGestureRecognizer) {
        switch g.state {
        case .began:
            // 记录缩放锚点（捏合中心）
            let loc = g.location(in: self)
            zoomAnchor = CGPoint(x: loc.x / bounds.width, y: loc.y / bounds.height)

        case .changed:
            let newScale = min(max(zoomScale * g.scale, 1.0), 5.0)
            g.scale = 1.0
            applyZoom(newScale, anchor: g.location(in: self))

        case .ended, .cancelled:
            // 若缩回接近 1.0，归位
            if zoomScale < 1.05 {
                resetZoom()
            }

        default:
            break
        }
    }

    /// 双击重置缩放
    @objc private func handleDoubleTap(_ g: UITapGestureRecognizer) {
        if zoomScale > 1.05 {
            resetZoom()
        } else {
            // 双击放大到 2x，以双击点为中心
            let loc = g.location(in: self)
            applyZoom(2.0, anchor: loc)
        }
    }

    /// 应用缩放：围绕 anchor 点放大，并调整偏移使 anchor 保持在原位
    private func applyZoom(_ scale: CGFloat, anchor: CGPoint) {
        let center = CGPoint(x: bounds.midX, y: bounds.midY)
        // anchor 相对中心的偏移
        let ax = anchor.x - center.x
        let ay = anchor.y - center.y
        // 保持 anchor 不动：newOffset = oldOffset + (1 - newScale/oldScale) * (anchor - center - oldOffset)
        let ratio = scale / zoomScale
        panOffset.x = panOffset.x + (1 - ratio) * (ax - panOffset.x)
        panOffset.y = panOffset.y + (1 - ratio) * (ay - panOffset.y)
        zoomScale = scale

        // 限制偏移，避免画面跑出视野
        clampOffset()

        applyVideoTransform()
    }

    private func clampOffset() {
        let maxX = bounds.width * (zoomScale - 1) / 2
        let maxY = bounds.height * (zoomScale - 1) / 2
        panOffset.x = min(max(panOffset.x, -maxX), maxX)
        panOffset.y = min(max(panOffset.y, -maxY), maxY)
    }

    private func resetZoom() {
        zoomScale = 1.0
        panOffset = .zero
        UIView.animate(withDuration: 0.2) {
            self.applyVideoTransform()
        }
    }
}
// MARK: - 远程页面背景

/// 远程页面的背景：暖色纸底 + 手绘相框。
///
/// 视频是 scaleAspectFit 的，四周必然留一圈 letterbox。以前那圈是死黑，
/// 远程页面看着就是一块没设计过的黑。现在这圈铺上暖色纸、沿画面外沿描一道
/// 手绘框把它「裱」起来——黑边从此不是浪费，而是装裱的留白，与主界面那套
/// 暖色也是一路。
///
/// 注意它压在视频「上层」：暖色只画在 letterbox 上（画面矩形被挖空），
/// 所以不遮视频，但调用方必须 .allowsHitTesting(false)，否则触摸全被吞掉。
struct RemoteBackdrop: View {
    /// 真实视频宽高比：纸要贴着画面挖洞，不能贴着屏幕画
    var videoAspect: CGFloat
    /// 缩放倍数（保留给调用方语义；纸的洞按 transformedVideoRect 走，
    /// 不再用它决定画不画）
    var zoomScale: CGFloat
    /// 变换（缩放+平移+键盘让位）后画面在视图里的实际矩形，UIKit 侧回传。
    /// 「放大后背景色没了」的根因就在这：旧实现 zoomScale>1.05 时整层不画，
    /// 可放大后的画面未必铺满全屏（竖屏放大看横屏画面，四周仍有未覆盖区），
    /// 那些区域立刻露出 MTKView 的死黑。正确做法是始终铺纸，
    /// 洞按变换后的真实矩形挖——画面铺满时洞=全屏，自然无处铺纸，
    /// 画面没铺满时未覆盖区继续是纸色。
    var transformedVideoRect: CGRect?

    // 与首页统一：letterbox 不再铺深棕底（太重、和主界面割裂），
    // 直接用主界面同款的米白→浅驼纸色渐变（Brand.paper）。

    var body: some View {
        GeometryReader { geo in
            // 洞矩形：优先用 UIKit 实测的变换后矩形；
            // 拿不到（首帧前/未接线）时退回 scaleAspectFit 几何
            let hole = (transformedVideoRect.flatMap {
                $0.width > 1 && $0.height > 1 ? $0 : nil
            } ?? videoRect(in: geo.size))
            ZStack {
                // 暖色纸底用「挖洞」填充：整屏减去画面矩形，画面那块留空。
                // 之所以挖洞而不是整块铺底 + 让视频层透明——RTCMTLVideoView
                // 内部是 MTKView，它自己的 drawable 照样会把 letterbox 清成
                // 不透明黑，铺在下层根本看不见。挖洞后装饰可以压在视频之上，
                // 又不遮画面。
                BandPath(video: hole)
                    .fill(Brand.paper, style: FillStyle(eoFill: true))
            }
        }
        .ignoresSafeArea()
    }

    /// 挖洞：整屏减去画面矩形。even-odd 填充规则下，内层矩形区域交叉次数为偶数，
/// 自然被挖空——这样暖色纸底只出现在 letterbox 上，画面一丝不遮。
    private struct BandPath: Shape {
        var video: CGRect

        func path(in rect: CGRect) -> Path {
            var p = Path()
            p.addRect(rect)
            if video.width > 1, video.height > 1 {
                p.addRect(video)
            }
            return p
        }
    }

    /// 画面矩形：与 scaleAspectFit 完全一致的几何。
    /// 尺寸还没拿到时退成整个屏幕，避免首帧跳位。
    private func videoRect(in size: CGSize) -> CGRect {
        guard size.width > 1, size.height > 1, videoAspect > 0.1 else {
            return CGRect(origin: .zero, size: size)
        }
        var w = size.width
        var h = size.width / videoAspect
        if h > size.height {
            h = size.height
            w = size.height * videoAspect
        }
        return CGRect(x: (size.width - w) / 2, y: (size.height - h) / 2,
                      width: w, height: h)
    }

    /// 相框矩形：画面外扩 12pt 给线条让位，但不超过屏幕。
    /// 横屏时画面几乎顶到左右边，硬扩会跑出屏幕。
    private func frameRect(from video: CGRect, in size: CGSize) -> CGRect {
        guard video.width > 1, video.height > 1 else {
            return CGRect(origin: .zero, size: size).insetBy(dx: 12, dy: 12)
        }
        let out = video.insetBy(dx: -12, dy: -12)
        return out.intersection(CGRect(origin: .zero, size: size).insetBy(dx: 5, dy: 5))
    }

    /// 只描有黑边的那两侧，中段微微外凸。
    /// 不画整圈：竖屏画面左右撑满屏幕、横屏上下撑满，闭合框的边会压在画面上。
    private struct EdgeLines: Shape {
        /// true=只画左右两条竖线（横屏用），false=只画上下两条横线（竖屏用）
        var side: Bool
        /// 起伏幅度，抖幅大些才有手作感
        var wobble: CGFloat

        func path(in rect: CGRect) -> Path {
            var p = Path()
            let w = wobble
            if side {
                // 左右两条竖线：中段略向外凸
                let y0 = rect.minY + 8, y1 = rect.maxY - 8
                let my = rect.midY
                p.move(to: CGPoint(x: rect.minX, y: y0))
                p.addQuadCurve(to: CGPoint(x: rect.minX, y: y1),
                               control: CGPoint(x: rect.minX - w, y: my))
                p.move(to: CGPoint(x: rect.maxX, y: y0))
                p.addQuadCurve(to: CGPoint(x: rect.maxX, y: y1),
                               control: CGPoint(x: rect.maxX + w, y: my))
            } else {
                // 上下两条横线：中段略向外凸
                let x0 = rect.minX + 8, x1 = rect.maxX - 8
                let mx = rect.midX
                p.move(to: CGPoint(x: x0, y: rect.minY))
                p.addQuadCurve(to: CGPoint(x: x1, y: rect.minY),
                               control: CGPoint(x: mx, y: rect.minY - w))
                p.move(to: CGPoint(x: x0, y: rect.maxY))
                p.addQuadCurve(to: CGPoint(x: x1, y: rect.maxY),
                               control: CGPoint(x: mx, y: rect.maxY + w))
            }
            return p
        }
    }
}