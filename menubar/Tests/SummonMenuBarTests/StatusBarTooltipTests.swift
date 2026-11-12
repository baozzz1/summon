import AppKit
import Testing
@testable import SummonMenuBar

@MainActor
@Test func statusButtonKeepsHoverDetailsDisabledAcrossRefreshes() {
    _ = NSApplication.shared
    let window = NSWindow(contentRect: NSRect(x: 240, y: 160, width: 600, height: 300), styleMask: .borderless, backing: .buffered, defer: false)
    let button = NSButton(frame: NSRect(x: 12, y: 8, width: 80, height: 22))
    window.contentView!.addSubview(button)
    button.toolTip = "Existing status"
    let image = NSImage(size: NSSize(width: 70, height: 18))
    applyStatusBarImage(image, accessibilityLabel: "summon · 2 working", to: button)
    #expect(button.visibleRect.intersection(button.bounds) == button.visibleRect)
    #expect(button.visibleRect.contains(NSPoint(x: 20, y: 10)))
    #expect(!button.visibleRect.contains(NSPoint(x: 100, y: 10)), "按钮外的位置应处于可见区域之外")
    #expect(button.toolTip == nil)
    for _ in 0..<5 { applyStatusBarImage(image, accessibilityLabel: "summon · 2 working", to: button) }
    #expect(button.toolTip == nil)
    applyStatusBarImage(image, accessibilityLabel: "summon · 3 working", to: button)
    #expect(button.toolTip == nil)
    #expect(button.accessibilityLabel() == "summon · 3 working")
    #expect(button.visibleRect.intersection(button.bounds) == button.visibleRect)
}

@MainActor
@Test func statusButtonTracksSizeAndScreenPosition() {
    _ = NSApplication.shared
    let window = NSWindow(contentRect: NSRect(x: -500, y: 120, width: 600, height: 300), styleMask: .borderless, backing: .buffered, defer: false)
    let button = NSButton(frame: NSRect(x: 10, y: 5, width: 100, height: 22))
    window.contentView!.addSubview(button)
    applyStatusBarImage(NSImage(size: NSSize(width: 90, height: 18)), accessibilityLabel: "status", to: button)
    let oldCursor = window.convertPoint(toScreen: button.convert(NSPoint(x: 75, y: 10), to: nil))
    #expect(button.visibleRect.contains(button.convert(window.convertPoint(fromScreen: oldCursor), from: nil)))
    button.setFrameSize(NSSize(width: 50, height: 22))
    #expect(!button.visibleRect.contains(button.convert(window.convertPoint(fromScreen: oldCursor), from: nil)), "按钮缩窄后旧范围内的指针已在外部")
    let cursor = window.convertPoint(toScreen: button.convert(NSPoint(x: 20, y: 10), to: nil))
    #expect(button.visibleRect.contains(button.convert(window.convertPoint(fromScreen: cursor), from: nil)))
    window.setFrameOrigin(NSPoint(x: 500, y: 200))
    #expect(!button.visibleRect.contains(button.convert(window.convertPoint(fromScreen: cursor), from: nil)))
    #expect(button.visibleRect.intersection(button.bounds) == button.visibleRect)
}
