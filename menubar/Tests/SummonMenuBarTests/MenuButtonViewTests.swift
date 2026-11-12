import AppKit
import Testing
@testable import SummonMenuBar

/// 上一版的「刷新」是死的:文字用了 NSTextField 子视图,点击在 hitTest 里被子视图截走,
/// `mouseUp` 永远不来 —— 而它照样编译通过、看起来完全正常。所以这条路必须有测试守着。
@MainActor
@Test func refreshButtonHasNoSubviewToStealTheClick() {
    let view = MenuButtonView(title: "刷新") {}
    #expect(view.subviews.isEmpty, "任何子视图都会在 hitTest 里截走点击,把按钮变成死的")
}

@MainActor
@Test func bottomActionsAreTextOnlyMenuItems() {
    for title in ["打开控制台…", "退出 Menu Bar"] {
        let item = textOnlyMenuItem(title: title, action: nil, keyEquivalent: "")
        #expect(item.image == nil)
        #expect(item.onStateImage == nil)
        #expect(item.offStateImage == nil)
        #expect(item.mixedStateImage == nil)
    }
}

@MainActor
@Test func refreshButtonInvokesCallbackOnMouseUp() throws {
    var clicks = 0
    let view = MenuButtonView(title: "刷新") { clicks += 1 }
    view.frame = NSRect(x: 0, y: 0, width: 200, height: 22)

    let inside = try #require(
        NSEvent.mouseEvent(
            with: .leftMouseUp,
            location: NSPoint(x: 100, y: 11),
            modifierFlags: [],
            timestamp: 0,
            windowNumber: 0,
            context: nil,
            eventNumber: 0,
            clickCount: 1,
            pressure: 1
        )
    )
    view.mouseUp(with: inside)
    #expect(clicks == 1, "点在按钮上必须触发刷新")

    // 松开在按钮之外(按下后拖出去)不该算一次点击。
    let outside = try #require(
        NSEvent.mouseEvent(
            with: .leftMouseUp,
            location: NSPoint(x: 100, y: 500),
            modifierFlags: [],
            timestamp: 0,
            windowNumber: 0,
            context: nil,
            eventNumber: 0,
            clickCount: 1,
            pressure: 1
        )
    )
    view.mouseUp(with: outside)
    #expect(clicks == 1, "松开在按钮之外不算点击")
}

/// 回归(2026-07-22 现场,macOS 26.5.2):「退出 Menu Bar」左边冒出一个系统符号图标,而同一个
/// 菜单里同样走 `textOnlyMenuItem` 的「打开控制台…」没有——差别只在 selector。macOS 26 会给
/// 绑定到**系统标准 selector**(如 `terminate:`)的菜单项在**显示时**自动补图标,构造时把
/// `image` 清成 nil 根本挡不住(上面那条测试传 action: nil,所以一直测不出来)。
/// 连带后果:只要菜单里有一项带图标,AppKit 就为整个菜单预留图标列,标准项被整体右推,
/// 与自定义视图的「刷新」错位——正是「去图标左对齐」当初想解决却没解决的那件事。
@MainActor
@Test func quitItemMustNotBindSystemTerminateSelector() {
    let item = quitMenuItem(target: nil as AnyObject?)
    #expect(
        item.action != #selector(NSApplication.terminate(_:)),
        "绑系统标准 selector 会被 macOS 26 自动加图标,并撑出图标列破坏左对齐"
    )
    #expect(item.image == nil)
    #expect(item.keyEquivalent == "q", "⌘Q 行为不能因为换 selector 丢掉")
}

/// 回归(2026-07-22 现场):「刷新」比相邻的「打开控制台…」/「退出 Menu Bar」明显靠左。
/// 起因是把 leading 降到 8,依据是「菜单里所有项都没有图标,文字就从行首起画」——前提错了:
/// AppKit 标准菜单项即使无图标也始终为状态列预留左内边距,文字从不贴行首。view-based 项
/// 不吃这套布局,必须自己补上同样的内边距,否则永远对不齐。
@MainActor
@Test func refreshRowKeepsStandardMenuTextInset() {
    #expect(
        MenuButtonView.leading >= 16,
        "把 leading 当成「行首」会让 view-based 的「刷新」比相邻标准项靠左一截"
    )
}

@MainActor
@Test func commentDraftActionIsFlatAndUpdatesInPlace() throws {
    let task = CommentTask(id: "task", agent: "writer", title: "Title", stage: "drafting", status: "Drafting", statusEn: "Drafting", busy: true, pid: 4242, error: nil)
    var clicks = 0
    let item = commentTaskMenuItem(task) { clicks += 1 }
    #expect(item.submenu == nil)
    #expect(item.action == nil)
    let view = try #require(item.view as? ChipRowView)
    #expect(view.subviews.isEmpty)
    #expect(view.label.contains("PID 4242"))
    let rect = try #require(ChipLayout.chipRects(view.chips, labelColumn: view.labelColumn, in: view.bounds).first)
    let event = try #require(NSEvent.mouseEvent(with: .leftMouseUp, location: NSPoint(x: rect.midX, y: rect.midY), modifierFlags: [], timestamp: 0, windowNumber: 0, context: nil, eventNumber: 0, clickCount: 1, pressure: 1))
    view.mouseUp(with: event)
    #expect(clicks == 1)
    view.configure(label: task.line, chips: commentDraftChips(task, stopping: true), detail: "Stopping…")
    view.mouseUp(with: event)
    #expect(clicks == 1)
    #expect(view.detail == "Stopping…")
    #expect(view.bounds.width >= ChipLayout.rowWidth(labelColumn: view.labelColumn, chips: view.chips, detail: view.detail))
    let ready = CommentTask(id: "task", agent: "writer", title: "Title", stage: "draft_ready", status: "Ready", statusEn: "Ready", busy: false, pid: nil, error: nil)
    view.configure(label: ready.line, chips: commentDraftChips(ready, stopping: false), detail: "")
    #expect(view.chips.isEmpty)
    #expect(item.submenu == nil)
}
