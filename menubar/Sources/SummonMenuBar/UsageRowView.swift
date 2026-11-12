import AppKit

/// 费用是只读信息，按正文绘制，保持系统明暗主题下的文字对比度。
final class UsageRowView: NSView {
    private(set) var label: String
    private(set) var amount: String
    private(set) var tokens: String
    static let minimumWidth: CGFloat = 360
    static let height: CGFloat = 26
    private static let trailing: CGFloat = 14
    private static let gap: CGFloat = 8
    private static let amountColumnWidth: CGFloat = 112
    private static let tokenColumnWidth: CGFloat = 104

    init(label: String, amount: String, tokens: String) {
        self.label = label
        self.amount = amount
        self.tokens = tokens
        super.init(frame: NSRect(x: 0, y: 0, width: Self.minimumWidth, height: Self.height))
        autoresizingMask = [.width]
        setAccessibilityElement(true)
        setAccessibilityRole(.staticText)
        setAccessibilityLabel("\(label): \(tokens), \(amount)")
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError(L10n.text("m7ee3d3f4")) }

    func configure(amount: String, tokens: String, tooltip: String?) {
        self.amount = amount
        self.tokens = tokens
        toolTip = tooltip
        setAccessibilityLabel("\(label): \(tokens), \(amount)")
        needsDisplay = true
    }

    override func draw(_ dirtyRect: NSRect) {
        let font = NSFont.menuFont(ofSize: 0)
        let amountFont = NSFont.monospacedDigitSystemFont(ofSize: font.pointSize, weight: .semibold)
        let right = NSAttributedString(string: amount, attributes: [
            .font: amountFont, .foregroundColor: NSColor.labelColor,
        ])
        let size = right.size()
        let amountX = bounds.width - Self.trailing - size.width
        let tokenText = NSAttributedString(string: tokens, attributes: [
            .font: NSFont.monospacedDigitSystemFont(ofSize: font.pointSize, weight: .regular),
            .foregroundColor: NSColor.labelColor,
        ])
        let tokenEnd = bounds.width - Self.trailing - max(Self.amountColumnWidth, size.width) - Self.gap
        let tokenX = tokenEnd - tokenText.size().width
        let paragraph = NSMutableParagraphStyle()
        paragraph.lineBreakMode = .byTruncatingTail
        let left = NSAttributedString(string: label, attributes: [
            .font: font, .foregroundColor: NSColor.labelColor, .paragraphStyle: paragraph,
        ])
        left.draw(with: NSRect(
            x: MenuButtonView.leading, y: (bounds.height - left.size().height) / 2,
            width: max(0, tokenEnd - max(Self.tokenColumnWidth, tokenText.size().width) - Self.gap - MenuButtonView.leading), height: left.size().height
        ), options: [.usesLineFragmentOrigin, .truncatesLastVisibleLine])
        tokenText.draw(at: NSPoint(x: tokenX, y: (bounds.height - tokenText.size().height) / 2))
        right.draw(at: NSPoint(x: amountX, y: (bounds.height - size.height) / 2))
    }

    override func mouseDown(with event: NSEvent) {}
    override func mouseUp(with event: NSEvent) {}
}

final class UsageFooterView: NSView {
    private(set) var note = ""
    private(set) var updated = ""

    init() {
        super.init(frame: NSRect(x: 0, y: 0, width: UsageRowView.minimumWidth, height: 38))
        autoresizingMask = [.width]
        setAccessibilityElement(true)
        setAccessibilityRole(.staticText)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError(L10n.text("m7ee3d3f4")) }

    func configure(note: String, updated: String, tooltip: String?) {
        self.note = note
        self.updated = updated
        toolTip = tooltip
        setAccessibilityLabel("\(note). \(updated)")
        needsDisplay = true
    }

    override func draw(_ dirtyRect: NSRect) {
        let paragraph = NSMutableParagraphStyle()
        paragraph.lineBreakMode = .byTruncatingTail
        for (text, y, color) in [(note, CGFloat(20), NSColor.labelColor), (updated, CGFloat(4), NSColor.secondaryLabelColor)] {
            (text as NSString).draw(in: NSRect(
                x: MenuButtonView.leading, y: y, width: bounds.width - MenuButtonView.leading - 14, height: 15
            ), withAttributes: [
                .font: NSFont.systemFont(ofSize: NSFont.smallSystemFontSize),
                .foregroundColor: color, .paragraphStyle: paragraph,
            ])
        }
    }

    override func mouseDown(with event: NSEvent) {}
    override func mouseUp(with event: NSEvent) {}
}
