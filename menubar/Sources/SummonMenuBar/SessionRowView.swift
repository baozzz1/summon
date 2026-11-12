import AppKit

/// 会话行动作的进度。行在菜单关闭前一直留在原位,所以进度与结果都画在行尾。
enum SessionRowStatus: Equatable {
    case normal
    case running(SessionAction)
    /// 动作完成,这一行还能接着做别的(Handled 之后照样能 Exit),只是不再出这个动作。
    case done(SessionAction)
    /// 会话已交出去结束,或已不在列表里:菜单关闭前不再给任何动作。
    case closed(String)

    var allowsActions: Bool {
        switch self {
        case .normal, .done: true
        case .running, .closed: false
        }
    }
}

extension RowState {
    /// 状态点颜色。waiting 用强调蓝(轮到你了)、working 绿、idle 灰、leaked 红 —— 四种一眼分得开。
    var dotColor: NSColor {
        switch self {
        case .leaked: .systemRed
        case .waiting: .systemBlue
        case .working: .systemGreen
        case .idle: .secondaryLabelColor
        case .exited: .tertiaryLabelColor
        }
    }

    /// idle / exited 画空心圈:没有要处理的事,不该和实心的几种抢眼。
    var dotIsHollow: Bool {
        self == .idle || self == .exited
    }
}

/// 会话一栏共用的列宽:群名、agent 两列取全部行里最宽的那个,各有上限。整栏对齐成列,
/// 菜单宽度由上限封顶 —— 群名再长也只截断,不把整个菜单撑宽。
struct SessionColumns: Equatable {
    static let maxChat: CGFloat = 200
    static let maxAgent: CGFloat = 120

    let chat: CGFloat
    let agent: CGFloat

    @MainActor
    static func fitting(_ rows: [SessionRow]) -> SessionColumns {
        let font = SessionRowLayout.font
        func width(_ text: String) -> CGFloat {
            ceil((text as NSString).size(withAttributes: [.font: font]).width)
        }
        return SessionColumns(
            chat: min(rows.map { width($0.chatName) }.max() ?? 0, maxChat),
            agent: min(rows.map { width($0.alias) }.max() ?? 0, maxAgent)
        )
    }
}

/// 行上的一块悬停提示区:按钮左边是整行那一块,每个按钮各一块。
struct SessionRowToolTip: Equatable {
    enum Target: Equatable {
        case row
        case action(SessionAction)
    }

    let target: Target
    let rect: NSRect
}

/// 行的几何。抽成纯函数:点到哪个按钮、每列从哪开始由它决定,测试直接对它下断言。
@MainActor
enum SessionRowLayout {
    static let height: CGFloat = 22
    static let trailing: CGFloat = 14
    static let dotSize: CGFloat = 8
    static let dotGap: CGFloat = 8
    static let columnGap: CGFloat = 12
    static let tailGap: CGFloat = 16
    static let buttonSize: CGFloat = 22
    static let buttonGap: CGFloat = 2

    static var font: NSFont {
        NSFont.menuFont(ofSize: 0)
    }

    static var chatX: CGFloat {
        MenuButtonView.leading + dotSize + dotGap
    }

    static func agentX(_ columns: SessionColumns) -> CGFloat {
        chatX + columns.chat + columnGap
    }

    /// 行尾那一列要放得下:三个图标按钮,或最长的进度 / 结果文字。
    static var tailWidth: CGFloat {
        let texts = SessionAction.allCases.flatMap { [$0.runningDetail, $0.doneDetail] }
            + [L10n.text("m27de2566"), L10n.text("md391c386"), L10n.text("m7cdc804e"), L10n.text("md17d84a6")]
        let widest = texts.map { ceil(($0 as NSString).size(withAttributes: [.font: font]).width) }
            .max() ?? 0
        return max(buttonsWidth(SessionAction.allCases), widest)
    }

    static func rowWidth(_ columns: SessionColumns) -> CGFloat {
        agentX(columns) + columns.agent + tailGap + tailWidth + trailing
    }

    static func buttonsWidth(_ actions: [SessionAction]) -> CGFloat {
        guard !actions.isEmpty else { return 0 }
        return CGFloat(actions.count) * buttonSize + CGFloat(actions.count - 1) * buttonGap
    }

    /// 从右往左排:最后一个动作贴着右边距。
    static func buttonRects(_ actions: [SessionAction], in bounds: NSRect) -> [(SessionAction, NSRect)] {
        var x = bounds.maxX - trailing
        var rects: [(SessionAction, NSRect)] = []
        for action in actions.reversed() {
            x -= buttonSize
            rects.append((
                action,
                NSRect(
                    x: x,
                    y: bounds.minY + (bounds.height - buttonSize) / 2,
                    width: buttonSize,
                    height: buttonSize
                )
            ))
            x -= buttonGap
        }
        return rects.reversed()
    }

    /// 悬停提示区。AppKit 按块计时出提示,指针跨进另一块才换文字,所以每个按钮单独一块;
    /// 各块互不重叠,重叠处出哪一块的提示由 AppKit 决定。
    static func toolTipRegions(_ actions: [SessionAction], in bounds: NSRect) -> [SessionRowToolTip] {
        let buttons = buttonRects(actions, in: bounds)
        var row = bounds
        if let first = buttons.first {
            row.size.width = first.1.minX - bounds.minX
        }
        return [SessionRowToolTip(target: .row, rect: row)]
            + buttons.map { SessionRowToolTip(target: .action($0.0), rect: $0.1) }
    }
}

/// 一行会话:状态点、群名、agent 三列对齐,行尾是时间;鼠标停在这一行时行尾换成动作图标。
///
/// 做成 view-based 菜单项是因为普通 `NSMenuItem` 一点就关整个菜单,而这里点完动作要留在原地看结果
/// (理由与 `MenuButtonView` 相同,见那里)。点群名那一侧打开飞书,那时菜单该关,由回调方 `cancelTracking`。
///
/// 按住 ⌥ 时行尾变成「Exit」,点整行即退出 —— 与 macOS 标准备选项同一个手势,只是菜单不关。
/// 判据以松开鼠标那一刻事件上的修饰键为准,`optionHeld` 只负责提前把样子画出来。
///
/// **不要往里塞子视图**:子视图会在 hitTest 里截走点击(见 `MenuButtonView`)。全部画在自己身上。
final class SessionRowView: NSView {
    private(set) var row: SessionRow
    let columns: SessionColumns
    private let onOpen: (SessionRow) -> Void
    private let onAction: (SessionAction, SessionRow) -> Void

    var status: SessionRowStatus = .normal {
        didSet {
            needsDisplay = true
            refreshToolTips()
        }
    }

    /// 会话已不在最新的列表里(结束了,或被别处收掉)。菜单开着时行不能删,画成灰的、不再给按钮。
    private(set) var isGone = false

    var optionHeld = false {
        didSet {
            guard oldValue != optionHeld else { return }
            needsDisplay = true
            refreshToolTips()
        }
    }

    private var isHighlighted = false
    private var hoveredAction: SessionAction?

    /// 已交给 AppKit 的提示区与它回的 tag;文字到要显示时才取。
    private(set) var toolTips: [(tag: NSView.ToolTipTag, region: SessionRowToolTip)] = []

    init(
        row: SessionRow,
        columns: SessionColumns,
        onOpen: @escaping (SessionRow) -> Void,
        onAction: @escaping (SessionAction, SessionRow) -> Void
    ) {
        self.row = row
        self.columns = columns
        self.onOpen = onOpen
        self.onAction = onAction
        super.init(frame: NSRect(
            x: 0, y: 0, width: SessionRowLayout.rowWidth(columns), height: SessionRowLayout.height
        ))
        autoresizingMask = [.width]
        setAccessibilityElement(true)
        setAccessibilityRole(.menuItem)
        applyRowText()
        refreshToolTips()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError(L10n.text("m7ee3d3f4"))
    }

    /// 菜单开着时原地换数据:行还是这一行,标题与可用动作跟着最新快照走。会话重新出现在列表里时
    /// 行也跟着恢复;动作的进度与结果(`status`)由动作自己的回调写,不随快照变。
    func configure(_ row: SessionRow) {
        self.row = row
        isGone = false
        applyRowText()
        refreshToolTips()
        needsDisplay = true
    }

    /// 会话不在最新的列表里了。只改 `isGone`,行尾文字由它派生,会话回来时 `configure` 一清就复原。
    func markGone() {
        guard !isGone else { return }
        isGone = true
        refreshToolTips()
        needsDisplay = true
    }

    /// 点群名那一侧能不能跳回飞书。CLI 没下发链接时文字画成次级色,点了不做任何事。
    var canOpen: Bool {
        OpenableLink.url(row.link) != nil
    }

    private func applyRowText() {
        setAccessibilityLabel(row.summary)
    }

    /// 群名、agent 都可能被截断,悬停提示里给全。
    private var rowToolTip: String {
        let action: String
        if !canOpen {
            action = L10n.text("m9d2b6b5a")
        } else {
            action = row.waiting == nil ? L10n.text("m89142690") : L10n.text("m3af56acf")
        }
        return "\(row.summary)\n\(action)"
    }

    /// 此刻画着、可点的按钮:鼠标停在这一行时才有。
    var visibleActions: [SessionAction] {
        isHighlighted ? hoverActions : []
    }

    /// 鼠标停上来时出的按钮。动作进行中、交出去结束之后、会话已不在、或 ⌥ 模式下都不出;
    /// 做完的那个动作不再出。提示区按它划分:指针在行上时,这些按钮都画着。
    var hoverActions: [SessionAction] {
        guard !isGone, !isOptionExit, status.allowsActions else { return [] }
        guard case let .done(finished) = status else { return row.actions }
        return row.actions.filter { $0 != finished }
    }

    /// ⌥ 模式:这一行能退出时,整行就是「Exit」。
    var isOptionExit: Bool {
        optionHeld && !isGone && status.allowsActions && row.canRun(.exit)
    }

    /// 行尾文字:⌥ 模式写 Exit;有进度 / 结果写它;会话已不在写 Ended;否则是时间。
    var tailText: String {
        if isOptionExit { return L10n.text("md17d84a6") }
        switch status {
        case let .running(action): return action.runningDetail
        case let .done(action): return action.doneDetail
        case let .closed(text): return text
        case .normal: return isGone ? L10n.text("m7cdc804e") : row.age
        }
    }

    // MARK: - 绘制

    override func draw(_ dirtyRect: NSRect) {
        if isHighlighted {
            NSColor.selectedContentBackgroundColor.setFill()
            NSBezierPath(roundedRect: bounds.insetBy(dx: 5, dy: 0), xRadius: 5, yRadius: 5).fill()
        }
        drawDot()
        drawText(row.chatName, x: SessionRowLayout.chatX, width: columns.chat, color: primaryColor)
        drawText(
            row.alias,
            x: SessionRowLayout.agentX(columns),
            width: columns.agent,
            color: secondaryColor
        )
        let buttons = visibleActions
        if buttons.isEmpty {
            drawTail()
        } else {
            for (action, rect) in SessionRowLayout.buttonRects(buttons, in: bounds) {
                drawButton(action, in: rect)
            }
        }
    }

    private var primaryColor: NSColor {
        if isHighlighted { return .selectedMenuItemTextColor }
        if isGone { return .tertiaryLabelColor }
        return canOpen ? .labelColor : .secondaryLabelColor
    }

    private var secondaryColor: NSColor {
        if isHighlighted { return NSColor.selectedMenuItemTextColor.withAlphaComponent(0.8) }
        return isGone ? .tertiaryLabelColor : .secondaryLabelColor
    }

    private func drawDot() {
        let size = SessionRowLayout.dotSize
        let rect = NSRect(
            x: MenuButtonView.leading,
            y: (bounds.height - size) / 2,
            width: size,
            height: size
        )
        let color = isGone ? NSColor.tertiaryLabelColor : row.state.dotColor
        if row.state.dotIsHollow || isGone {
            let ring = NSBezierPath(ovalIn: rect.insetBy(dx: 0.75, dy: 0.75))
            ring.lineWidth = 1.5
            (isHighlighted ? NSColor.selectedMenuItemTextColor : color).setStroke()
            ring.stroke()
        } else {
            color.setFill()
            NSBezierPath(ovalIn: rect).fill()
        }
    }

    /// 单行、放不下截断末尾。
    private func drawText(_ text: String, x: CGFloat, width: CGFloat, color: NSColor) {
        let paragraph = NSMutableParagraphStyle()
        paragraph.lineBreakMode = .byTruncatingTail
        let string = NSAttributedString(string: text, attributes: [
            .font: SessionRowLayout.font,
            .foregroundColor: color,
            .paragraphStyle: paragraph,
        ])
        let height = ceil(string.size().height)
        string.draw(
            with: NSRect(x: x, y: (bounds.height - height) / 2, width: max(width, 0), height: height),
            options: [.usesLineFragmentOrigin, .truncatesLastVisibleLine],
            context: nil
        )
    }

    private func drawTail() {
        let text = tailText
        guard !text.isEmpty else { return }
        let color: NSColor
        if isHighlighted {
            color = .selectedMenuItemTextColor
        } else if isOptionExit {
            color = .labelColor
        } else {
            color = .tertiaryLabelColor
        }
        let font = isOptionExit
            ? NSFont.boldSystemFont(ofSize: SessionRowLayout.font.pointSize)
            : SessionRowLayout.font
        let attributes: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: color]
        let size = (text as NSString).size(withAttributes: attributes)
        (text as NSString).draw(
            at: NSPoint(
                x: bounds.width - size.width - SessionRowLayout.trailing,
                y: (bounds.height - size.height) / 2
            ),
            withAttributes: attributes
        )
    }

    /// 图标按钮画在选中态的底色上:白色图标,悬停的那个垫一层半透明白底;做不了的调暗。
    private func drawButton(_ action: SessionAction, in rect: NSRect) {
        let enabled = row.canRun(action)
        if enabled, hoveredAction == action {
            NSColor.white.withAlphaComponent(0.28).setFill()
            NSBezierPath(roundedRect: rect, xRadius: 5, yRadius: 5).fill()
        }
        let color = NSColor.selectedMenuItemTextColor.withAlphaComponent(enabled ? 1 : 0.4)
        let configuration = NSImage.SymbolConfiguration(pointSize: 13, weight: .regular)
            .applying(NSImage.SymbolConfiguration(hierarchicalColor: color))
        guard let image = NSImage(
            systemSymbolName: action.symbolName,
            accessibilityDescription: action.toolTip
        )?.withSymbolConfiguration(configuration) else { return }
        let size = image.size
        image.draw(in: NSRect(
            x: rect.midX - size.width / 2,
            y: rect.midY - size.height / 2,
            width: size.width,
            height: size.height
        ))
    }

    // MARK: - 鼠标

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        trackingAreas.forEach(removeTrackingArea)
        addTrackingArea(
            NSTrackingArea(
                rect: bounds,
                options: [.mouseEnteredAndExited, .mouseMoved, .activeAlways, .inVisibleRect],
                owner: self
            )
        )
    }

    override func mouseEntered(with event: NSEvent) {
        isHighlighted = true
        updateHover(at: convert(event.locationInWindow, from: nil))
        needsDisplay = true
    }

    override func mouseExited(with event: NSEvent) {
        isHighlighted = false
        hoveredAction = nil
        needsDisplay = true
    }

    override func mouseMoved(with event: NSEvent) {
        updateHover(at: convert(event.locationInWindow, from: nil))
    }

    private func updateHover(at point: NSPoint) {
        let hovered = action(at: point)
        if hovered != hoveredAction {
            hoveredAction = hovered
            needsDisplay = true
        }
    }

    /// 吃掉 mouseDown:不往上传,菜单的 tracking 循环就不会把这次点击当成「选中了某项」而关菜单。
    override func mouseDown(with event: NSEvent) {}

    override func mouseUp(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        guard bounds.contains(point) else { return }
        optionHeld = event.modifierFlags.contains(.option)
        // 鼠标没移动过也能点中:按下前未必收到过 mouseMoved/mouseEntered。
        isHighlighted = true
        if isOptionExit {
            onAction(.exit, row)
            return
        }
        if let action = action(at: point) {
            if row.canRun(action) {
                onAction(action, row)
            }
            return
        }
        if canOpen {
            onOpen(row)
        }
    }

    /// 这一点落在哪个可见按钮上。
    func action(at point: NSPoint) -> SessionAction? {
        SessionRowLayout.buttonRects(visibleActions, in: bounds)
            .first { $0.1.contains(point) }?
            .0
    }

    // MARK: - 悬停提示

    /// 按钮贴着右边排,菜单把行拉宽时提示区跟着走。
    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        refreshToolTips()
    }

    /// 按此刻的按钮重新划分提示区。分区没变就不碰:重新登记会让 AppKit 丢掉正在计时的提示。
    private func refreshToolTips() {
        let regions = SessionRowLayout.toolTipRegions(hoverActions, in: bounds)
        guard regions != toolTips.map(\.region) else { return }
        removeAllToolTips()
        toolTips = regions.map { (addToolTip($0.rect, owner: self, userData: nil), $0) }
    }
}

extension SessionRowView: NSViewToolTipOwner {
    /// 文字到要显示时才取:行上的状态、时间随刷新在变,提示区不必跟着重新登记。
    func view(
        _ view: NSView,
        stringForToolTip tag: NSView.ToolTipTag,
        point: NSPoint,
        userData data: UnsafeMutableRawPointer?
    ) -> String {
        switch toolTips.first(where: { $0.tag == tag })?.region.target {
        case let .action(action): action.toolTip
        case .row: rowToolTip
        case nil: ""
        }
    }
}
