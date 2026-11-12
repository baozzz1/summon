import AppKit

/// 菜单里「点了不该关菜单」的那类项(目前只有「刷新」)。
///
/// 普通 `NSMenuItem` 一旦有 action,点击必然 dismiss 整个菜单 —— AppKit 的固有行为,没有开关可关。
/// 而「刷新」的全部意义就是**留在原地看结果**,一点就关等于没有。view-based 的菜单项把鼠标事件
/// 交给 view 自己处理,菜单不会 dismiss —— 这是唯一的官方途径。
///
/// **不要往里塞 NSTextField 之类的子视图**:子视图会在 hitTest 里截走点击,`mouseUp` 永远不来,
/// 按钮看着能点其实是死的(第一版就栽在这)。文字直接画在自己身上,点击必然落到本 view。
final class MenuButtonView: NSView {
    private let title: String
    private let onClick: () -> Void
    private var isHighlighted = false

    /// 右侧灰字。刷新用它做反馈:取数中显示「刷新中…」,取完显示时刻 ——
    /// 数据可能刷完一模一样,时间戳变了才是「确实刷过了」的证据。
    var detail: String {
        didSet { needsDisplay = true }
    }

    /// 与相邻**标准** `NSMenuItem` 的文字左边缘对齐。
    ///
    /// 上一版按「菜单里所有项都没有图标,文字就从行首起画」把它降到 8,结果「刷新」比相邻项
    /// 明显靠左 —— 前提是错的:AppKit 标准菜单项即使没有任何图标,也始终为状态列(勾选标记)
    /// 预留一段左内边距,文字**从不**从行首起画。view-based 项不吃这套布局,得自己补上。
    ///
    /// AppKit 没有公开这个内边距的 API。16pt 是**量出来**的,不是猜的:对同一菜单在两个不同
    /// leading 值(8 和 19)下各截一张图,量出「刷新」相对标准项的像素偏移,联立消掉未知的截图
    /// 缩放后解得 —— 两张图各自反解出的缩放都落在 2.00(retina),这个物理约束没有被代入,
    /// 是解出来的,可作交叉校验。同法解得菜单行距 ≈ 23.1pt。
    /// 改这个值请重新按上法实测,别凭肉眼估。
    static let leading: CGFloat = 16
    private static let trailing: CGFloat = 14
    private static let height: CGFloat = 22

    /// 没有 keyEquivalent:view-based 菜单项不参与菜单的快捷键分发,标一个按不动的 ⌘R 是骗人。
    init(title: String, detail: String = "", onClick: @escaping () -> Void) {
        self.title = title
        self.detail = detail
        self.onClick = onClick
        super.init(frame: NSRect(x: 0, y: 0, width: 220, height: Self.height))
        autoresizingMask = [.width]
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError(L10n.text("m7ee3d3f4"))
    }

    private var textAttributes: [NSAttributedString.Key: Any] {
        [
            .font: NSFont.menuFont(ofSize: 0),
            .foregroundColor: isHighlighted
                ? NSColor.selectedMenuItemTextColor
                : NSColor.labelColor,
        ]
    }

    private var detailAttributes: [NSAttributedString.Key: Any] {
        [
            .font: NSFont.menuFont(ofSize: 0),
            .foregroundColor: isHighlighted
                ? NSColor.selectedMenuItemTextColor
                : NSColor.tertiaryLabelColor,
        ]
    }

    override func draw(_ dirtyRect: NSRect) {
        if isHighlighted {
            NSColor.selectedContentBackgroundColor.setFill()
            NSBezierPath(roundedRect: bounds.insetBy(dx: 5, dy: 0), xRadius: 5, yRadius: 5).fill()
        }
        let text = title as NSString
        let size = text.size(withAttributes: textAttributes)
        text.draw(
            at: NSPoint(x: Self.leading, y: (bounds.height - size.height) / 2),
            withAttributes: textAttributes
        )

        guard !detail.isEmpty else { return }
        let right = detail as NSString
        let rightSize = right.size(withAttributes: detailAttributes)
        right.draw(
            at: NSPoint(
                x: bounds.width - rightSize.width - Self.trailing,
                y: (bounds.height - rightSize.height) / 2
            ),
            withAttributes: detailAttributes
        )
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        trackingAreas.forEach(removeTrackingArea)
        addTrackingArea(
            NSTrackingArea(
                rect: bounds,
                options: [.mouseEnteredAndExited, .activeAlways, .inVisibleRect],
                owner: self
            )
        )
    }

    override func mouseEntered(with event: NSEvent) {
        setHighlighted(true)
    }

    override func mouseExited(with event: NSEvent) {
        setHighlighted(false)
    }

    /// 吃掉 mouseDown:不往上传,菜单的 tracking 循环就不会把这次点击当成「选中了某项」而关菜单。
    override func mouseDown(with event: NSEvent) {}

    /// 点击只调回调,**不** `cancelTracking()` —— 菜单保持打开正是这个 view 存在的理由。
    override func mouseUp(with event: NSEvent) {
        guard bounds.contains(convert(event.locationInWindow, from: nil)) else { return }
        onClick()
    }

    private func setHighlighted(_ on: Bool) {
        isHighlighted = on
        needsDisplay = true
    }
}
