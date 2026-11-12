import AppKit
import Testing
@testable import SummonMenuBar

private func row(
    actions: [SessionAction] = [.handled, .exit, .archive],
    disabled: Set<SessionAction> = [],
    link: String? = "lark://group/g1",
    waiting: Bool = false
) -> SessionRow {
    SessionRow(
        alias: "saturn", tag: "g1", chatName: "研发群", state: waiting ? .waiting : .idle, age: "1m",
        busy: false, tmuxIdle: false, waiting: nil, link: link, actions: actions, disabled: disabled
    )
}

private let columns = SessionColumns(chat: 120, agent: 80)

/// 记下回调:点群名那一侧打开,点按钮做动作。
@MainActor
private final class Recorder {
    var opened = 0
    var actions: [SessionAction] = []

    func view(_ row: SessionRow) -> SessionRowView {
        SessionRowView(
            row: row,
            columns: columns,
            onOpen: { [unowned self] _ in opened += 1 },
            onAction: { [unowned self] action, _ in actions.append(action) }
        )
    }
}

private func mouse(_ type: NSEvent.EventType, at point: NSPoint, option: Bool = false) throws -> NSEvent {
    try #require(NSEvent.mouseEvent(
        with: type, location: point, modifierFlags: option ? [.option] : [], timestamp: 0,
        windowNumber: 0, context: nil, eventNumber: 0, clickCount: 1, pressure: 1
    ))
}

@MainActor
private func enter(_ view: SessionRowView, at point: NSPoint) throws {
    let event = try #require(NSEvent.enterExitEvent(
        with: .mouseEntered, location: point, modifierFlags: [], timestamp: 0, windowNumber: 0,
        context: nil, eventNumber: 0, trackingNumber: 0, userData: nil
    ))
    view.mouseEntered(with: event)
}

@MainActor
private func leave(_ view: SessionRowView, at point: NSPoint) throws {
    let event = try #require(NSEvent.enterExitEvent(
        with: .mouseExited, location: point, modifierFlags: [], timestamp: 0, windowNumber: 0,
        context: nil, eventNumber: 0, trackingNumber: 0, userData: nil
    ))
    view.mouseExited(with: event)
}

/// 照 AppKit 的做法取提示:找到指针所在的提示区,拿它的 tag 向视图要文字。
@MainActor
private func toolTip(in view: SessionRowView, at point: NSPoint) throws -> String {
    let region = try #require(view.toolTips.first { $0.region.rect.contains(point) })
    return view.view(view, stringForToolTip: region.tag, point: point, userData: nil)
}

@MainActor
private func center(of action: SessionAction, in view: SessionRowView) throws -> NSPoint {
    let rect = try #require(
        SessionRowLayout.buttonRects(view.row.actions, in: view.bounds).first { $0.0 == action }?.1
    )
    return NSPoint(x: rect.midX, y: rect.midY)
}

private let onTitle = NSPoint(x: 40, y: 11)

/// 子视图会在 hitTest 里截走点击,把整行变成死的(`MenuButtonView` 栽过)。
@MainActor
@Test func sessionRowHasNoSubviewToStealTheClick() {
    #expect(Recorder().view(row()).subviews.isEmpty)
}

/// 鼠标停上去才出按钮;没停上去时行尾是时间。
@MainActor
@Test func buttonsShowOnlyWhileHovered() throws {
    let view = Recorder().view(row())
    #expect(view.visibleActions.isEmpty)
    #expect(view.tailText == "1m")
    try enter(view, at: onTitle)
    #expect(view.visibleActions == [.handled, .exit, .archive])
}

/// 点哪个按钮做哪个动作,不打开飞书;点群名那一侧打开飞书,不做动作。
@MainActor
@Test func clickingAButtonRunsItsActionAndTheRestOpens() throws {
    let recorder = Recorder()
    let view = recorder.view(row())
    for action in SessionAction.allCases {
        view.mouseUp(with: try mouse(.leftMouseUp, at: center(of: action, in: view)))
    }
    #expect(recorder.actions == [.handled, .exit, .archive])
    #expect(recorder.opened == 0)

    view.mouseUp(with: try mouse(.leftMouseUp, at: onTitle))
    #expect(recorder.opened == 1)
    #expect(recorder.actions.count == 3)
}

/// 灰掉的按钮点了没反应,也不会穿透成「打开飞书」。
@MainActor
@Test func aDisabledButtonDoesNothing() throws {
    let recorder = Recorder()
    let view = recorder.view(row(disabled: [.exit, .archive]))
    view.mouseUp(with: try mouse(.leftMouseUp, at: center(of: .archive, in: view)))
    #expect(recorder.actions.isEmpty)
    #expect(recorder.opened == 0)
}

/// 按住 ⌥:行尾是 Exit,点整行即退出;判据以松开那一刻事件上的修饰键为准。
@MainActor
@Test func optionClickAnywhereExits() throws {
    let recorder = Recorder()
    let view = recorder.view(row())
    view.optionHeld = true
    #expect(view.tailText == "Exit")
    #expect(view.visibleActions.isEmpty, "⌥ 模式下不出按钮,整行就是 Exit")

    view.mouseUp(with: try mouse(.leftMouseUp, at: onTitle, option: true))
    #expect(recorder.actions == [.exit])
    #expect(recorder.opened == 0)

    // 画面上还是 ⌥ 模式,但松开时已经没按 ⌥:按普通点击处理。
    view.mouseUp(with: try mouse(.leftMouseUp, at: onTitle, option: false))
    #expect(recorder.opened == 1)
    #expect(recorder.actions == [.exit])
}

/// 退不了的行(只剩 waiting 的、或 daemon 没在跑)按住 ⌥ 也不变成 Exit。
@MainActor
@Test func optionDoesNothingWhereExitIsUnavailable() throws {
    for unavailable in [row(actions: [.handled]), row(disabled: [.exit, .archive])] {
        let recorder = Recorder()
        let view = recorder.view(unavailable)
        view.optionHeld = true
        #expect(view.tailText == "1m")
        view.mouseUp(with: try mouse(.leftMouseUp, at: onTitle, option: true))
        #expect(recorder.actions.isEmpty)
        #expect(recorder.opened == 1)
    }
}

/// 动作进行中:不出按钮、⌥ 也不退出,同一行不叠第二个动作。
@MainActor
@Test func noActionsWhileOneIsRunning() throws {
    let recorder = Recorder()
    let view = recorder.view(row())
    view.status = .running(.archive)
    #expect(view.tailText == "Archiving…")
    try enter(view, at: onTitle)
    #expect(view.visibleActions.isEmpty)
    view.optionHeld = true
    #expect(!view.isOptionExit)
    view.mouseUp(with: try mouse(.leftMouseUp, at: onTitle, option: true))
    #expect(recorder.actions.isEmpty)
}

/// 会话从最新列表里消失:行留在原位变灰、写 Ended,不再给按钮;已有的结果不被改写。
/// 会话又回到列表里(被新消息拉起):行跟着恢复。
@MainActor
@Test func aGoneRowLosesItsButtonsAndComesBackWithItsSession() throws {
    let view = Recorder().view(row())
    view.markGone()
    #expect(view.tailText == "Ended")
    try enter(view, at: onTitle)
    #expect(view.visibleActions.isEmpty)
    view.configure(row())
    #expect(view.tailText == "1m")
    #expect(view.visibleActions == [.handled, .exit, .archive])

    let handled = Recorder().view(row())
    handled.status = .done(.handled)
    handled.markGone()
    #expect(handled.tailText == "Handled")

    let exited = Recorder().view(row())
    exited.status = .closed(SessionAction.exit.doneDetail)
    exited.markGone()
    #expect(exited.tailText == "Exited")
}

/// Exit / Archive 交出去之后(哪怕会话还在列表里收尾),这一行不再出任何动作:被投第二次会再发一条退出回执。
/// Handled 之后照样能 Exit。
@MainActor
@Test func aClosedRowOffersNothingButAHandledRowStillCanExit() throws {
    let recorder = Recorder()
    let pending = recorder.view(row())
    pending.status = .closed("Still ending…")
    try enter(pending, at: onTitle)
    #expect(pending.visibleActions.isEmpty)
    pending.optionHeld = true
    #expect(!pending.isOptionExit)
    pending.mouseUp(with: try mouse(.leftMouseUp, at: onTitle, option: true))
    #expect(recorder.actions.isEmpty)

    let handled = Recorder().view(row())
    handled.status = .done(.handled)
    try enter(handled, at: onTitle)
    #expect(handled.visibleActions == [.exit, .archive], "做完的 Handled 不再出,刷新前也不会被点第二次")
}

/// CLI 没下发链接:点群名那一侧不做任何事,悬停提示说明原因;行尾按钮照常可用。
@MainActor
@Test func aRowWithoutLinkSaysSoAndDoesNotOpen() throws {
    let recorder = Recorder()
    let view = recorder.view(row(link: nil))
    #expect(!view.canOpen)
    #expect(try toolTip(in: view, at: onTitle).contains("No Feishu link for this chat"))
    view.mouseUp(with: try mouse(.leftMouseUp, at: onTitle))
    #expect(recorder.opened == 0)
    view.mouseUp(with: try mouse(.leftMouseUp, at: center(of: .exit, in: view)))
    #expect(recorder.actions == [.exit])
}

/// 群名一侧的提示给全群名与 agent(列里可能被截断);每个按钮单独一块提示区,说的是这个按钮。
/// 指针在行里移动不重新登记:重新登记会让 AppKit 丢掉正在计时的提示。
@MainActor
@Test func eachButtonHasItsOwnToolTipRegion() throws {
    let view = Recorder().view(row())
    #expect(try toolTip(in: view, at: onTitle).hasPrefix("研发群 · saturn · idle · 1m"))
    #expect(try toolTip(in: view, at: onTitle).contains("open this chat in Feishu"))
    for action in SessionAction.allCases {
        #expect(try toolTip(in: view, at: center(of: action, in: view)) == action.toolTip)
    }

    let tags = view.toolTips.map(\.tag)
    try enter(view, at: center(of: .archive, in: view))
    view.mouseMoved(with: try mouse(.mouseMoved, at: onTitle))
    view.mouseMoved(with: try mouse(.mouseMoved, at: center(of: .exit, in: view)))
    try leave(view, at: onTitle)
    #expect(view.toolTips.map(\.tag) == tags)
    #expect(view.toolTip == nil, "提示全由提示区给出;整行的 toolTip 会多出一块盖住按钮的区")
}

/// 提示区与画出来的按钮重合、互不重叠;没有按钮时整行一块。
@MainActor
@Test func toolTipRegionsMatchTheButtonsWithoutOverlap() {
    let bounds = NSRect(x: 0, y: 0, width: 400, height: SessionRowLayout.height)
    let regions = SessionRowLayout.toolTipRegions(SessionAction.allCases, in: bounds)
    #expect(regions.map(\.target) == [.row, .action(.handled), .action(.exit), .action(.archive)])
    #expect(regions.dropFirst().map(\.rect)
        == SessionRowLayout.buttonRects(SessionAction.allCases, in: bounds).map(\.1))
    for (index, region) in regions.enumerated() {
        for other in regions.dropFirst(index + 1) {
            #expect(region.rect.intersection(other.rect).isEmpty)
        }
    }
    #expect(SessionRowLayout.toolTipRegions([], in: bounds) == [SessionRowToolTip(target: .row, rect: bounds)])
}

/// 按钮少了(做完的动作、动作进行中、按住 ⌥、会话已不在),提示区跟着撤;行被拉宽时按钮的块跟着贴右边。
@MainActor
@Test func toolTipRegionsFollowTheButtons() throws {
    let view = Recorder().view(row())
    let all: [SessionRowToolTip.Target] = [.row, .action(.handled), .action(.exit), .action(.archive)]
    #expect(view.toolTips.map(\.region.target) == all)

    view.status = .done(.handled)
    #expect(view.toolTips.map(\.region.target) == [.row, .action(.exit), .action(.archive)])
    view.status = .running(.exit)
    #expect(view.toolTips.map(\.region.target) == [.row])
    view.status = .normal
    view.optionHeld = true
    #expect(view.toolTips.map(\.region.target) == [.row])
    view.optionHeld = false
    view.markGone()
    #expect(view.toolTips.map(\.region.target) == [.row])
    view.configure(row())
    #expect(view.toolTips.map(\.region.target) == all)

    view.setFrameSize(NSSize(width: view.frame.width + 60, height: view.frame.height))
    #expect(try toolTip(in: view, at: center(of: .archive, in: view)) == SessionAction.archive.toolTip)
}

/// 两列按最宽的取、各有上限:群名再长,菜单也不会被撑宽。行宽放得下两列与行尾的按钮。
@MainActor
@Test func columnsAreCappedAndTheRowFitsTheButtons() throws {
    let long = SessionRow(
        alias: String(repeating: "a", count: 80), tag: "g", chatName: String(repeating: "群", count: 80),
        state: .idle, age: "1m", busy: false, tmuxIdle: false, waiting: nil, link: nil, actions: [], disabled: []
    )
    let fitted = SessionColumns.fitting([row(), long])
    #expect(fitted.chat == SessionColumns.maxChat)
    #expect(fitted.agent == SessionColumns.maxAgent)
    let short = SessionColumns.fitting([row()])
    #expect(short.chat < SessionColumns.maxChat)

    let view = Recorder().view(row())
    let firstButton = try #require(SessionRowLayout.buttonRects(view.row.actions, in: view.bounds).first)
    #expect(firstButton.1.minX >= SessionRowLayout.agentX(columns) + columns.agent)
}

/// 每种状态一种颜色,waiting 与 idle 一眼分得开;没有要处理的事的 idle / exited 画空心。
@Test func eachStateHasItsOwnDot() {
    let colors = RowState.allCases.map(\.dotColor)
    #expect(Set(colors).count == RowState.allCases.count)
    #expect(RowState.waiting.dotColor != RowState.idle.dotColor)
    #expect(RowState.exited.dotColor != RowState.idle.dotColor)
    #expect(RowState.allCases.filter(\.dotIsHollow) == [.idle, .exited])
}

// MARK: - 胶囊行

@MainActor
private final class ChipRecorder {
    var clicks: [Int] = []

    func view(_ chips: [Chip]) -> ChipRowView {
        ChipRowView(label: "Sentinel", labelColumn: 80, chips: chips) { [unowned self] in
            clicks.append($0)
        }
    }
}

private let sentinelChips = [
    Chip(title: "Off", selected: false, enabled: true, toolTip: nil),
    Chip(title: "Dry-run", selected: false, enabled: false, toolTip: "Overridden in agent.yaml by a"),
    Chip(title: "On", selected: true, enabled: true, toolTip: nil),
]

@MainActor
private func chipCenter(_ index: Int, in view: ChipRowView) -> NSPoint {
    let rect = ChipLayout.chipRects(view.chips, labelColumn: view.labelColumn, in: view.bounds)[index]
    return NSPoint(x: rect.midX, y: rect.midY)
}

/// 点胶囊回调它的序号;点灰掉的胶囊、点标签区都不回调。
@MainActor
@Test func chipClicksReachOnlyEnabledChips() throws {
    let recorder = ChipRecorder()
    let view = recorder.view(sentinelChips)
    view.mouseUp(with: try mouse(.leftMouseUp, at: chipCenter(0, in: view)))
    view.mouseUp(with: try mouse(.leftMouseUp, at: chipCenter(1, in: view)))
    view.mouseUp(with: try mouse(.leftMouseUp, at: chipCenter(2, in: view)))
    view.mouseUp(with: try mouse(.leftMouseUp, at: NSPoint(x: 30, y: 12)))
    #expect(recorder.clicks == [0, 2])
}

/// 悬停提示跟着胶囊走;离开胶囊就清掉。
@MainActor
@Test func chipToolTipFollowsTheHoveredChip() throws {
    let view = ChipRecorder().view(sentinelChips)
    view.mouseMoved(with: try mouse(.mouseMoved, at: chipCenter(1, in: view)))
    #expect(view.toolTip == "Overridden in agent.yaml by a")
    view.mouseMoved(with: try mouse(.mouseMoved, at: chipCenter(0, in: view)))
    #expect(view.toolTip == nil)
}

/// 胶囊从标签列之后起排,几行共用一个标签列宽就上下对齐;行宽放得下标签列与全部胶囊。
@MainActor
@Test func chipsStartAfterTheSharedLabelColumn() {
    let view = ChipRecorder().view(sentinelChips)
    let rects = ChipLayout.chipRects(view.chips, labelColumn: 80, in: view.bounds)
    #expect(rects[0].minX == MenuButtonView.leading + 80 + ChipLayout.labelGap)
    #expect(zip(rects, rects.dropFirst()).allSatisfy { $0.maxX < $1.minX })
    #expect(view.frame.width >= rects.last!.maxX + ChipLayout.trailing)
}
