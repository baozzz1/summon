import AppKit

/// 一行里的一个胶囊。
struct Chip: Equatable {
    let title: String
    /// 亮着:单选行里是当前值,多选行里是开着。纯动作(daemon 的 Restart / Stop)恒不亮。
    let selected: Bool
    let enabled: Bool
    let toolTip: String?
}

/// 胶囊的几何,抽成纯函数供测试。
@MainActor
enum ChipLayout {
    static let height: CGFloat = 24
    static let chipHeight: CGFloat = 18
    static let padding: CGFloat = 8
    static let gap: CGFloat = 4
    static let labelGap: CGFloat = 12
    static let trailing: CGFloat = 14

    static var font: NSFont {
        NSFont.systemFont(ofSize: NSFont.smallSystemFontSize)
    }

    static var labelFont: NSFont {
        NSFont.menuFont(ofSize: 0)
    }

    static func chipWidth(_ title: String) -> CGFloat {
        ceil((title as NSString).size(withAttributes: [.font: font]).width) + 2 * padding
    }

    static func labelWidth(_ label: String) -> CGFloat {
        ceil((label as NSString).size(withAttributes: [.font: labelFont]).width)
    }

    static func chipsWidth(_ chips: [Chip]) -> CGFloat {
        guard !chips.isEmpty else { return 0 }
        return chips.map { chipWidth($0.title) }.reduce(0, +) + CGFloat(chips.count - 1) * gap
    }

    /// 胶囊从标签列之后起排,一行内左对齐;几行共用同一个标签列宽,胶囊就上下对齐成列。
    static func chipRects(_ chips: [Chip], labelColumn: CGFloat, in bounds: NSRect) -> [NSRect] {
        var x = MenuButtonView.leading + labelColumn + labelGap
        return chips.map { chip in
            let width = chipWidth(chip.title)
            defer { x += width + gap }
            return NSRect(
                x: x,
                y: bounds.minY + (bounds.height - chipHeight) / 2,
                width: width,
                height: chipHeight
            )
        }
    }

    static func rowWidth(labelColumn: CGFloat, chips: [Chip], detail: String) -> CGFloat {
        let detailWidth = detail.isEmpty
            ? 0
            : ceil((detail as NSString).size(withAttributes: [.font: font]).width) + labelGap
        return MenuButtonView.leading + labelColumn + labelGap + chipsWidth(chips) + detailWidth
            + trailing
    }
}

/// 设置区与 daemon 那一行:左边标签,右边一排可点的胶囊,点了菜单不关。
///
/// 整行不高亮 —— 这一行本身不是一个动作,能点的是胶囊。其余约束同 `MenuButtonView`:
/// view-based 菜单项、不放子视图、`mouseDown` 吃掉。
final class ChipRowView: NSView {
    private(set) var label: String
    private(set) var chips: [Chip]
    /// 行尾的灰字:进度(`Restarting…`)或覆盖提示(`2 agents override`)。
    var detail: String {
        didSet { needsDisplay = true }
    }

    let labelColumn: CGFloat
    private let onClick: (Int) -> Void
    private var hovered: Int?

    init(label: String, labelColumn: CGFloat, chips: [Chip], detail: String = "", onClick: @escaping (Int) -> Void) {
        self.label = label
        self.chips = chips
        self.detail = detail
        self.labelColumn = labelColumn
        self.onClick = onClick
        super.init(frame: NSRect(
            x: 0, y: 0,
            width: ChipLayout.rowWidth(labelColumn: labelColumn, chips: chips, detail: detail),
            height: ChipLayout.height
        ))
        autoresizingMask = [.width]
        setAccessibilityElement(true)
        setAccessibilityRole(.group)
        setAccessibilityLabel(label)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError(L10n.text("m7ee3d3f4"))
    }

    /// 菜单开着时原地换数据。行宽在建行时已定,标签放不下就截断(见 `draw`);胶囊个数可以变
    /// (daemon 起停换掉动作),行尾放不下的灰字会被挤到右边距外。
    func configure(label: String, chips: [Chip], detail: String) {
        self.label = label
        self.chips = chips
        self.detail = detail
        setAccessibilityLabel(label)
        needsDisplay = true
    }

    /// 这一点落在哪个胶囊上。
    func chip(at point: NSPoint) -> Int? {
        ChipLayout.chipRects(chips, labelColumn: labelColumn, in: bounds)
            .firstIndex { $0.contains(point) }
    }

    // MARK: - 绘制

    override func draw(_ dirtyRect: NSRect) {
        // 标签只占标签列:菜单开着时它可能变长(错误信息、带出手改的数值),截断末尾,不压到胶囊上。
        let paragraph = NSMutableParagraphStyle()
        paragraph.lineBreakMode = .byTruncatingTail
        let labelText = NSAttributedString(string: label, attributes: [
            .font: ChipLayout.labelFont,
            .foregroundColor: NSColor.labelColor,
            .paragraphStyle: paragraph,
        ])
        let labelHeight = ceil(labelText.size().height)
        labelText.draw(
            with: NSRect(
                x: MenuButtonView.leading,
                y: (bounds.height - labelHeight) / 2,
                width: labelColumn,
                height: labelHeight
            ),
            options: [.usesLineFragmentOrigin, .truncatesLastVisibleLine],
            context: nil
        )
        let rects = ChipLayout.chipRects(chips, labelColumn: labelColumn, in: bounds)
        for (index, (chip, rect)) in zip(chips, rects).enumerated() {
            drawChip(chip, in: rect, hovered: hovered == index)
        }
        guard !detail.isEmpty else { return }
        let detailAttributes: [NSAttributedString.Key: Any] = [
            .font: ChipLayout.font,
            .foregroundColor: NSColor.tertiaryLabelColor,
        ]
        let size = (detail as NSString).size(withAttributes: detailAttributes)
        let x = max(
            (rects.last?.maxX ?? MenuButtonView.leading) + ChipLayout.labelGap,
            bounds.width - size.width - ChipLayout.trailing
        )
        (detail as NSString).draw(
            at: NSPoint(x: x, y: (bounds.height - size.height) / 2),
            withAttributes: detailAttributes
        )
    }

    /// 亮着:强调色实底、白字。不亮:浅灰底、正文色。悬停加深一档;不可点的整体调暗。
    private func drawChip(_ chip: Chip, in rect: NSRect, hovered: Bool) {
        let fill: NSColor
        let text: NSColor
        if chip.selected {
            fill = NSColor.controlAccentColor.withAlphaComponent(hovered ? 0.85 : 1)
            text = .white
        } else {
            fill = hovered && chip.enabled ? .tertiaryLabelColor : .quaternaryLabelColor
            text = .labelColor
        }
        let alpha: CGFloat = chip.enabled ? 1 : 0.45
        fill.withAlphaComponent(fill.alphaComponent * alpha).setFill()
        NSBezierPath(roundedRect: rect, xRadius: rect.height / 2, yRadius: rect.height / 2).fill()
        let attributes: [NSAttributedString.Key: Any] = [
            .font: ChipLayout.font,
            .foregroundColor: text.withAlphaComponent(alpha),
        ]
        let size = (chip.title as NSString).size(withAttributes: attributes)
        (chip.title as NSString).draw(
            at: NSPoint(x: rect.midX - size.width / 2, y: rect.midY - size.height / 2),
            withAttributes: attributes
        )
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
        updateHover(at: convert(event.locationInWindow, from: nil))
    }

    override func mouseMoved(with event: NSEvent) {
        updateHover(at: convert(event.locationInWindow, from: nil))
    }

    override func mouseExited(with event: NSEvent) {
        hovered = nil
        toolTip = nil
        needsDisplay = true
    }

    private func updateHover(at point: NSPoint) {
        let index = chip(at: point)
        guard index != hovered else { return }
        hovered = index
        toolTip = index.flatMap { chips[$0].toolTip }
        needsDisplay = true
    }

    /// 吃掉 mouseDown:菜单不把这次点击当成「选中了某项」而关菜单。
    override func mouseDown(with event: NSEvent) {}

    override func mouseUp(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        guard let index = chip(at: point), chips[index].enabled else { return }
        onClick(index)
    }
}

/// 分组标题:`Waiting  2`。不可点,只画字;行尾可带一句灰字说明(比如设置读不出来)。
final class SectionHeaderView: NSView {
    static let height: CGFloat = 20

    private(set) var title: String
    private(set) var count: Int?
    private(set) var detail = ""
    private let titleColor: NSColor

    init(title: String, count: Int? = nil, color: NSColor = .secondaryLabelColor) {
        self.title = title
        self.count = count
        titleColor = color
        super.init(frame: NSRect(x: 0, y: 0, width: 120, height: Self.height))
        autoresizingMask = [.width]
        setAccessibilityElement(true)
        setAccessibilityRole(.staticText)
        setAccessibilityLabel(accessibilityText)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError(L10n.text("m7ee3d3f4"))
    }

    func configure(count: Int?) {
        self.count = count
        setAccessibilityLabel(accessibilityText)
        needsDisplay = true
    }

    func configure(detail: String, toolTip: String?) {
        self.detail = detail
        self.toolTip = toolTip
        needsDisplay = true
    }

    private var accessibilityText: String {
        count.map { "\(title) \($0)" } ?? title
    }

    override func draw(_ dirtyRect: NSRect) {
        let font = NSFont.systemFont(ofSize: NSFont.smallSystemFontSize, weight: .semibold)
        let text = NSMutableAttributedString(
            string: title,
            attributes: [.font: font, .foregroundColor: titleColor]
        )
        if let count {
            text.append(NSAttributedString(
                string: "  \(count)",
                attributes: [.font: font, .foregroundColor: NSColor.tertiaryLabelColor]
            ))
        }
        let size = text.size()
        text.draw(at: NSPoint(x: MenuButtonView.leading, y: (bounds.height - size.height) / 2 - 1))
        guard !detail.isEmpty else { return }
        let attributes: [NSAttributedString.Key: Any] = [
            .font: NSFont.systemFont(ofSize: NSFont.smallSystemFontSize),
            .foregroundColor: NSColor.tertiaryLabelColor,
        ]
        let detailSize = (detail as NSString).size(withAttributes: attributes)
        (detail as NSString).draw(
            at: NSPoint(
                x: bounds.width - detailSize.width - ChipLayout.trailing,
                y: (bounds.height - detailSize.height) / 2 - 1
            ),
            withAttributes: attributes
        )
    }
}
